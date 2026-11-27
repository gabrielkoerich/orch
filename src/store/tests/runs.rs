use super::super::*;
use sqlx::Row;

#[tokio::test]
async fn append_memory_fails_on_corrupt_memory_and_preserves_db_value() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Corrupt memory test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Inject invalid JSON into the memory column directly
    let corrupt = "not valid json {{{";
    sqlx::query("UPDATE tasks SET memory = ? WHERE id = ?")
        .bind(corrupt)
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    let entry = MemoryEntry {
        attempt: 1,
        agent: "claude".to_string(),
        model: None,
        learnings: vec![],
        error: None,
        files_modified: vec![],
        approach: "attempt".to_string(),
        timestamp: String::new(),
    };

    // append_memory should return an error and must not overwrite the DB value
    let res = store.append_memory(id, &entry).await;
    assert!(res.is_err(), "append_memory must fail on corrupt JSON");

    let row = sqlx::query("SELECT memory FROM tasks WHERE id = ?")
        .bind(id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    let mem: String = row.try_get("memory").unwrap();
    assert_eq!(
        mem, corrupt,
        "corrupt memory payload should be preserved in DB"
    );
}

#[tokio::test]
async fn recent_memory_errors_on_corrupt_json() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Recent memory corrupt test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Inject invalid JSON into the memory column directly
    let corrupt = "[ this is not valid json";
    sqlx::query("UPDATE tasks SET memory = ? WHERE id = ?")
        .bind(corrupt)
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    let res = store.recent_memory(id, 10).await;
    assert!(
        res.is_err(),
        "recent_memory must return error on corrupt JSON"
    );
}

#[tokio::test]
async fn memory_append_and_recent() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    for i in 1..=5 {
        store
            .append_memory(
                id,
                &MemoryEntry {
                    attempt: i,
                    agent: "claude".to_string(),
                    model: Some("sonnet".to_string()),
                    learnings: vec![format!("Learning {i}")],
                    error: None,
                    files_modified: vec![],
                    approach: format!("Approach {i}"),
                    timestamp: format!("2026-01-0{i}T00:00:00Z"),
                },
            )
            .await
            .unwrap();
    }

    let recent = store.recent_memory(id, 3).await.unwrap();
    assert_eq!(recent.len(), 3);
    assert_eq!(recent[0].attempt, 3);
    assert_eq!(recent[2].attempt, 5);
}

#[tokio::test]
async fn task_runs_lifecycle() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let run_id = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude -p ...",
            prompt: "system prompt",
        })
        .await
        .unwrap();

    store
        .complete_run(&CompleteRun {
            run_id,
            exit_code: Some(0),
            stdout: "agent output here",
            stderr: "",
            parsed: r#"{"summary":"fixed it"}"#,
            outcome: "success",
            error: "",
            tokens: RunTokenUsage {
                input_tokens: 50000,
                output_tokens: 10000,
                total_cost_usd: 0.30,
                duration_secs: 45.0,
            },
        })
        .await
        .unwrap();

    let runs = store.get_runs(task_id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, "success");
    assert_eq!(runs[0].exit_code, Some(0));

    let last = store.get_last_run(task_id, "agent").await.unwrap();
    assert!(last.is_some());
    assert_eq!(last.unwrap().agent, "claude");

    let none = store.get_last_run(task_id, "review").await.unwrap();
    assert!(none.is_none());
}

#[tokio::test]
async fn complete_run_stores_null_error_for_empty_string() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let run_id = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude -p ...",
            prompt: "system prompt",
        })
        .await
        .unwrap();

    store
        .complete_run(&CompleteRun {
            run_id,
            exit_code: Some(0),
            stdout: "",
            stderr: "",
            parsed: "{}",
            outcome: "success",
            error: "",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    let stored_error: Option<String> =
        sqlx::query_scalar("SELECT error FROM task_runs WHERE id = ?")
            .bind(run_id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
    assert_eq!(stored_error, None);
}

#[tokio::test]
async fn complete_run_redacts_sensitive_git_error_fields() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let run_id = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "codex",
            model: "gpt-5.2",
            command: "git push",
            prompt: "push branch",
        })
        .await
        .unwrap();

    let leaked =
        "https://x-access-token:ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij@github.com/owner/repo.git";
    let stderr = format!("remote rejected {leaked}");
    let error = format!("push failed: {stderr}");

    store
        .complete_run(&CompleteRun {
            run_id,
            exit_code: Some(1),
            stdout: "",
            stderr: &stderr,
            parsed: "{}",
            outcome: "failed",
            error: &error,
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    let row = sqlx::query("SELECT stderr, error FROM task_runs WHERE id = ?")
        .bind(run_id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
    let stored_stderr: String = row.try_get("stderr").unwrap();
    let stored_error: String = row.try_get("error").unwrap();
    assert!(!stored_stderr.contains("ghp_"));
    assert!(!stored_error.contains("ghp_"));
    assert!(stored_stderr.contains("[REDACTED:github-pat]"));
    assert!(stored_error.contains("[REDACTED:github-pat]"));

    let activity = sqlx::query_scalar::<_, String>(
        "SELECT details FROM task_activity WHERE task_id = ? AND event_type = 'error' ORDER BY id DESC LIMIT 1",
    )
    .bind(task_id)
    .fetch_one(&store.pool)
    .await
    .unwrap();
    assert!(!activity.contains("ghp_"));
    assert!(activity.contains("[REDACTED:github-pat]"));
}

#[tokio::test]
async fn finalize_incomplete_runs_marks_open_runs_aborted() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let _agent_run = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude -p ...",
            prompt: "system prompt",
        })
        .await
        .unwrap();

    let _review_run = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "review",
            agent: "codex",
            model: "gpt-5.3-codex",
            command: "codex --model gpt-5.3-codex",
            prompt: "review prompt",
        })
        .await
        .unwrap();

    let finalized = store
        .finalize_incomplete_runs(task_id, "aborted", "graceful shutdown")
        .await
        .unwrap();
    assert_eq!(finalized, 2);

    let runs = store.get_runs(task_id).await.unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.outcome == "aborted"));
    assert!(runs.iter().all(|r| r.completed_at.is_some()));
    assert!(runs.iter().all(|r| r.exit_code == Some(-1)));
}

#[tokio::test]
async fn finalize_incomplete_runs_ignores_completed_runs() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let run_id = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude -p ...",
            prompt: "system prompt",
        })
        .await
        .unwrap();
    store
        .complete_run(&CompleteRun {
            run_id,
            exit_code: Some(0),
            stdout: "",
            stderr: "",
            parsed: "{}",
            outcome: "success",
            error: "",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    let finalized = store
        .finalize_incomplete_runs(task_id, "aborted", "graceful shutdown")
        .await
        .unwrap();
    assert_eq!(finalized, 0);
}

#[tokio::test]
async fn start_run_self_heals_orphaned_prior_run() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Simulate a crash: start a run and never complete it
    store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude -p ...",
            prompt: "system prompt",
        })
        .await
        .unwrap();

    // Starting the next attempt should finalize the orphaned row first
    store
        .start_run(&StartRun {
            task_id,
            attempt: 2,
            run_type: "agent",
            agent: "codex",
            model: "gpt-5",
            command: "codex -p ...",
            prompt: "system prompt",
        })
        .await
        .unwrap();

    let runs = store.get_runs(task_id).await.unwrap();
    assert_eq!(runs.len(), 2);
    let first = runs.iter().find(|r| r.attempt == 1).unwrap();
    let second = runs.iter().find(|r| r.attempt == 2).unwrap();
    assert_eq!(first.outcome, "aborted");
    assert!(first.completed_at.is_some());
    assert_eq!(second.outcome, "");
    assert!(second.completed_at.is_none());
}

#[tokio::test]
async fn store_tokens() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    store
        .store_tokens(id, 50000, 10000, Some(0.42))
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.input_tokens, 50000);
    assert_eq!(task.output_tokens, 10000);
    assert_eq!(
        task.total_cost_usd, 0.42,
        "the reported cost is stored exactly"
    );
    // model must NOT be overwritten by store_tokens — it is set exclusively by routing
    assert_eq!(task.model, None);
}

#[tokio::test]
async fn memory_empty_by_default() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let memory = store.recent_memory(id, 10).await.unwrap();
    assert!(memory.is_empty(), "{memory:?}");
}

#[tokio::test]
async fn multiple_runs_per_task() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Multi-run".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Attempt 1: agent run fails
    let r1 = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude -p ...",
            prompt: "prompt v1",
        })
        .await
        .unwrap();
    store
        .complete_run(&CompleteRun {
            run_id: r1,
            exit_code: Some(1),
            stdout: "error output",
            stderr: "compile failed",
            parsed: "",
            outcome: "failed",
            error: "compilation error",
            tokens: RunTokenUsage {
                input_tokens: 30000,
                output_tokens: 5000,
                total_cost_usd: 0.10,
                duration_secs: 20.0,
            },
        })
        .await
        .unwrap();

    // Attempt 2: agent run succeeds
    let r2 = store
        .start_run(&StartRun {
            task_id,
            attempt: 2,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "claude -p ...",
            prompt: "prompt v2",
        })
        .await
        .unwrap();
    store
        .complete_run(&CompleteRun {
            run_id: r2,
            exit_code: Some(0),
            stdout: "success output",
            stderr: "",
            parsed: r#"{"summary":"done"}"#,
            outcome: "success",
            error: "",
            tokens: RunTokenUsage {
                input_tokens: 40000,
                output_tokens: 8000,
                total_cost_usd: 0.80,
                duration_secs: 60.0,
            },
        })
        .await
        .unwrap();

    // Attempt 2: review run
    let r3 = store
        .start_run(&StartRun {
            task_id,
            attempt: 2,
            run_type: "review",
            agent: "claude",
            model: "sonnet",
            command: "claude -p review ...",
            prompt: "review prompt",
        })
        .await
        .unwrap();
    store
        .complete_run(&CompleteRun {
            run_id: r3,
            exit_code: Some(0),
            stdout: "LGTM",
            stderr: "",
            parsed: r#"{"verdict":"approve"}"#,
            outcome: "success",
            error: "",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    // All 3 runs
    let runs = store.get_runs(task_id).await.unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].attempt, 1);
    assert_eq!(runs[0].outcome, "failed");
    assert_eq!(runs[1].attempt, 2);
    assert_eq!(runs[1].run_type, "agent");
    assert_eq!(runs[2].attempt, 2);
    assert_eq!(runs[2].run_type, "review");

    // Last agent run is attempt 2
    let last_agent = store.get_last_run(task_id, "agent").await.unwrap().unwrap();
    assert_eq!(last_agent.attempt, 2);
    assert_eq!(last_agent.model, "opus");

    // Last review run
    let last_review = store
        .get_last_run(task_id, "review")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last_review.outcome, "success");
}

#[tokio::test]
async fn start_run_upserts_on_conflict() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Start same run twice (same task_id, attempt, run_type)
    let id1 = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "cmd1",
            prompt: "prompt1",
        })
        .await
        .unwrap();

    let id2 = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "codex",
            model: "gpt-5.2",
            command: "cmd2",
            prompt: "prompt2",
        })
        .await
        .unwrap();

    assert_eq!(id1, id2, "upsert should return same ID");

    // Should have updated agent/model
    let runs = store.get_runs(task_id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].agent, "codex");
    assert_eq!(runs[0].model, "gpt-5.2");
}

#[tokio::test]
async fn prune_old_runs_deletes_runs_for_old_done_tasks() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Old task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Add a run
    let run_id = store
        .start_run(&StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "echo test",
            prompt: "do stuff",
        })
        .await
        .unwrap();
    store
        .complete_run(&CompleteRun {
            run_id,
            exit_code: Some(0),
            stdout: "ok",
            stderr: "",
            parsed: "{}",
            outcome: "success",
            error: "",
            tokens: RunTokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                total_cost_usd: 0.001,
                duration_secs: 5.0,
            },
        })
        .await
        .unwrap();

    store.update_status(id, TaskStatus::Done).await.unwrap();
    backdate_done_entry(&store, id).await;

    let pruned = store.prune_old_runs(30).await.unwrap();
    assert_eq!(pruned, 1, "should clear 1 run for old done task");

    let runs = store.get_runs(id).await.unwrap();
    assert_eq!(runs.len(), 1, "the row stays after prune");
    let run = &runs[0];
    assert_eq!(run.stdout, "");
    assert_eq!(run.prompt, "");
    assert_eq!(run.parsed_response, "");
    assert_eq!(run.stderr, "");
    assert_eq!(run.outcome, "success");
    assert_eq!(run.input_tokens, 100);
    assert_eq!(run.output_tokens, 50);
    assert!((run.total_cost_usd - 0.001).abs() < 1e-9);
    assert!((run.duration_secs - 5.0).abs() < 1e-9);
}

async fn backdate_done_entry(store: &TaskStore, id: i64) {
    sqlx::query(
        "UPDATE task_activity SET timestamp = '2020-01-01T00:00:00Z' \
         WHERE task_id = ? AND event_type = 'status_change' AND to_status = 'done'",
    )
    .bind(id)
    .execute(&store.pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn prune_old_runs_keeps_runs_for_blocked_tasks() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();
    store
        .start_run(&StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "",
            prompt: "do stuff",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    sqlx::query("UPDATE task_activity SET timestamp = '2020-01-01T00:00:00Z' WHERE task_id = ?")
        .bind(id)
        .execute(&store.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(&store.pool)
        .await
        .unwrap();

    assert_eq!(store.prune_old_runs(30).await.unwrap(), 0);
    assert_eq!(store.get_runs(id).await.unwrap()[0].prompt, "do stuff");
}

#[tokio::test]
async fn prune_old_runs_ages_done_tasks_from_the_done_entry_not_updated_at() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();
    store
        .start_run(&StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "",
            prompt: "do stuff",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Done).await.unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(&store.pool)
        .await
        .unwrap();

    assert_eq!(store.prune_old_runs(30).await.unwrap(), 0);
    assert_eq!(store.get_runs(id).await.unwrap()[0].prompt, "do stuff");
}

#[tokio::test]
async fn prune_old_runs_keeps_runs_for_active_tasks() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Active task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .start_run(&StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "",
            prompt: "do stuff",
        })
        .await
        .unwrap();

    // Task stays in 'new' status — not done/blocked
    // Backdate it too
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(&store.pool)
        .await
        .unwrap();

    let pruned = store.prune_old_runs(30).await.unwrap();
    assert_eq!(
        pruned, 0,
        "should not prune runs for active (non-done/blocked) tasks"
    );

    let runs = store.get_runs(id).await.unwrap();
    assert_eq!(runs.len(), 1, "run should still exist");
}

#[tokio::test]
async fn prune_old_runs_keeps_recent_done_tasks() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Recent done task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .start_run(&StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "",
            prompt: "",
        })
        .await
        .unwrap();

    // Mark done but keep updated_at recent (default is now)
    store.update_status(id, TaskStatus::Done).await.unwrap();

    let pruned = store.prune_old_runs(30).await.unwrap();
    assert_eq!(
        pruned, 0,
        "should not prune runs for recently completed tasks"
    );
}

#[tokio::test]
async fn complete_run_records_all_fields() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let run_id = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "claude --model opus",
            prompt: "Fix the bug",
        })
        .await
        .unwrap();

    store
        .complete_run(&CompleteRun {
            run_id,
            exit_code: Some(0),
            stdout: "Fixed it!",
            stderr: "warning: unused var",
            parsed: r#"{"summary":"Fixed bug"}"#,
            outcome: "success",
            error: "",
            tokens: RunTokenUsage {
                input_tokens: 5000,
                output_tokens: 2000,
                total_cost_usd: 0.105,
                duration_secs: 45.3,
            },
        })
        .await
        .unwrap();

    let run = store.get_last_run(task_id, "agent").await.unwrap().unwrap();
    assert_eq!(run.agent, "claude");
    assert_eq!(run.model, "opus");
    assert_eq!(run.command, "claude --model opus");
    assert_eq!(run.prompt, "Fix the bug");
    assert_eq!(run.exit_code, Some(0));
    assert_eq!(run.stdout, "Fixed it!");
    assert_eq!(run.stderr, "warning: unused var");
    assert_eq!(run.parsed_response, r#"{"summary":"Fixed bug"}"#);
    assert_eq!(run.outcome, "success");
    assert_eq!(run.error, "");
    assert_eq!(run.input_tokens, 5000);
    assert_eq!(run.output_tokens, 2000);
    assert!((run.total_cost_usd - 0.105).abs() < 0.001);
    assert!((run.duration_secs - 45.3).abs() < 0.1);
    assert!(run.completed_at.is_some());
}

#[tokio::test]
async fn complete_run_keeps_the_outcome_of_a_run_closed_by_recovery() {
    let store = TaskStore::open_memory().await.unwrap();
    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let run_id = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "claude",
            prompt: "p",
        })
        .await
        .unwrap();
    store
        .finalize_incomplete_runs(
            task_id,
            "aborted",
            "stuck-task recovery: active session killed",
        )
        .await
        .unwrap();

    store
        .complete_run(&CompleteRun {
            run_id,
            exit_code: Some(0),
            stdout: "",
            stderr: "",
            parsed: "",
            outcome: "failed",
            error: "task was reset by the engine while the session ran (status new)",
            tokens: RunTokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                total_cost_usd: 0.0,
                duration_secs: 0.0,
            },
        })
        .await
        .unwrap();

    let run = store.get_last_run(task_id, "agent").await.unwrap().unwrap();
    assert_eq!(run.outcome, "aborted");
    assert_eq!(run.error, "stuck-task recovery: active session killed");
}

#[tokio::test]
async fn complete_run_derives_duration_from_started_at_when_unreported() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let run_id = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "review",
            agent: "claude",
            model: "sonnet",
            command: "",
            prompt: "",
        })
        .await
        .unwrap();

    sqlx::query("UPDATE task_runs SET started_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-120 seconds') WHERE id = ?")
        .bind(run_id)
        .execute(&store.pool)
        .await
        .unwrap();

    store
        .complete_run(&CompleteRun {
            run_id,
            exit_code: Some(0),
            stdout: "",
            stderr: "",
            parsed: "",
            outcome: "timeout",
            error: "",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    let run = store
        .get_last_run(task_id, "review")
        .await
        .unwrap()
        .unwrap();
    assert!(run.duration_secs >= 119.0, "got {}", run.duration_secs);
}

#[tokio::test]
async fn get_last_run_filters_by_run_type() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Create runs of different types
    store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "route",
            agent: "claude",
            model: "haiku",
            command: "",
            prompt: "route this",
        })
        .await
        .unwrap();
    store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "codex",
            model: "gpt-5.2",
            command: "codex run",
            prompt: "fix bug",
        })
        .await
        .unwrap();
    store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "review",
            agent: "claude",
            model: "sonnet",
            command: "",
            prompt: "review PR",
        })
        .await
        .unwrap();

    let route_run = store.get_last_run(task_id, "route").await.unwrap().unwrap();
    assert_eq!(route_run.agent, "claude");
    assert_eq!(route_run.model, "haiku");

    let agent_run = store.get_last_run(task_id, "agent").await.unwrap().unwrap();
    assert_eq!(agent_run.agent, "codex");

    let review_run = store
        .get_last_run(task_id, "review")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(review_run.prompt, "review PR");

    let missing = store.get_last_run(task_id, "nonexistent").await.unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn get_runs_returns_all_attempts_ordered() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Multi-attempt".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    for attempt in 1..=3 {
        store
            .start_run(&StartRun {
                task_id,
                attempt,
                run_type: "agent",
                agent: "claude",
                model: "sonnet",
                command: "",
                prompt: &format!("attempt {attempt}"),
            })
            .await
            .unwrap();
    }

    let runs = store.get_runs(task_id).await.unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].attempt, 1);
    assert_eq!(runs[1].attempt, 2);
    assert_eq!(runs[2].attempt, 3);
    assert_eq!(runs[0].prompt, "attempt 1");
    assert_eq!(runs[2].prompt, "attempt 3");
}

#[tokio::test]
async fn run_lifecycle_start_complete_query() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Lifecycle test".to_string(),
            external_id: Some("100".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

    // Attempt 1: agent run
    let run1 = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude --model sonnet",
            prompt: "Fix the login bug",
        })
        .await
        .unwrap();

    store
        .complete_run(&CompleteRun {
            run_id: run1,
            exit_code: Some(1),
            stdout: "Error: couldn't find file",
            stderr: "WARN: deprecated API",
            parsed: r#"{"status":"failed"}"#,
            outcome: "failed",
            error: "file not found",
            tokens: RunTokenUsage {
                input_tokens: 3000,
                output_tokens: 1500,
                total_cost_usd: 0.045,
                duration_secs: 30.0,
            },
        })
        .await
        .unwrap();

    // Attempt 2: retry with different model
    let run2 = store
        .start_run(&StartRun {
            task_id,
            attempt: 2,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "claude --model opus",
            prompt: "Fix the login bug (retry)",
        })
        .await
        .unwrap();

    store
        .complete_run(&CompleteRun {
            run_id: run2,
            exit_code: Some(0),
            stdout: "Fixed the bug by updating the path",
            stderr: "",
            parsed: r#"{"status":"done","summary":"Fixed path"}"#,
            outcome: "success",
            error: "",
            tokens: RunTokenUsage {
                input_tokens: 5000,
                output_tokens: 3000,
                total_cost_usd: 0.12,
                duration_secs: 60.0,
            },
        })
        .await
        .unwrap();

    // Attempt 1: review run
    let run3 = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "review",
            agent: "codex",
            model: "gpt-5.2",
            command: "codex review",
            prompt: "Review PR #42",
        })
        .await
        .unwrap();

    store
        .complete_run(&CompleteRun {
            run_id: run3,
            exit_code: Some(0),
            stdout: "LGTM",
            stderr: "",
            parsed: r#"{"decision":"approve"}"#,
            outcome: "success",
            error: "",
            tokens: RunTokenUsage {
                input_tokens: 2000,
                output_tokens: 500,
                total_cost_usd: 0.03,
                duration_secs: 15.0,
            },
        })
        .await
        .unwrap();

    // Query: get all runs
    let all_runs = store.get_runs(task_id).await.unwrap();
    assert_eq!(all_runs.len(), 3);
    assert_eq!(all_runs[0].attempt, 1);
    assert_eq!(all_runs[0].run_type, "agent");
    assert_eq!(all_runs[0].outcome, "failed");
    assert_eq!(all_runs[1].attempt, 1);
    assert_eq!(all_runs[1].run_type, "review");
    assert_eq!(all_runs[2].attempt, 2);
    assert_eq!(all_runs[2].run_type, "agent");
    assert_eq!(all_runs[2].outcome, "success");

    // Query: last agent run should be attempt 2
    let last_agent = store.get_last_run(task_id, "agent").await.unwrap().unwrap();
    assert_eq!(last_agent.attempt, 2);
    assert_eq!(last_agent.model, "opus");
    assert_eq!(last_agent.exit_code, Some(0));

    // Query: last review run
    let last_review = store
        .get_last_run(task_id, "review")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last_review.agent, "codex");
    assert_eq!(last_review.parsed_response, r#"{"decision":"approve"}"#);
}

#[tokio::test]
async fn memory_append_and_retrieve() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Memory test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Append memory entries
    let entry1 = MemoryEntry {
        attempt: 1,
        agent: "claude".to_string(),
        model: None,
        learnings: vec!["file is at src/main.rs".to_string()],
        error: None,
        files_modified: vec![],
        approach: "read the code".to_string(),
        timestamp: "2026-01-01T00:00:00Z".to_string(),
    };
    let entry2 = MemoryEntry {
        attempt: 2,
        agent: "codex".to_string(),
        model: None,
        learnings: vec!["need to update Cargo.toml too".to_string()],
        error: None,
        files_modified: vec![],
        approach: "edit files".to_string(),
        timestamp: "2026-01-01T01:00:00Z".to_string(),
    };

    store.append_memory(id, &entry1).await.unwrap();
    store.append_memory(id, &entry2).await.unwrap();

    // Retrieve recent
    let recent = store.recent_memory(id, 10).await.unwrap();
    assert_eq!(recent.len(), 2);

    // Retrieve with limit
    let limited = store.recent_memory(id, 1).await.unwrap();
    assert_eq!(limited.len(), 1);
}

#[tokio::test]
async fn store_tokens_accumulates_across_calls() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Token test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store.store_tokens(id, 1000, 500, Some(0.25)).await.unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.input_tokens, 1000);
    assert_eq!(task.output_tokens, 500);
    assert_eq!(task.total_cost_usd, 0.25);

    // Second call accumulates (not replaces)
    store.store_tokens(id, 2000, 1000, Some(0.5)).await.unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.input_tokens, 3000);
    assert_eq!(task.output_tokens, 1500);
    assert_eq!(task.total_cost_usd, 0.75);
}

#[tokio::test]
async fn store_tokens_without_a_reported_cost_stores_none() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "No cost".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store.store_tokens(id, 1000, 500, None).await.unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.input_tokens, 1000);
    assert_eq!(task.output_tokens, 500);
    assert_eq!(task.reported_cost_usd(), None);
}

#[tokio::test]
async fn append_memory_within_transaction() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Memory tx test".to_string(),
            body: String::new(),
            source: String::new(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Append two entries sequentially — both should be preserved
    store
        .append_memory(
            id,
            &MemoryEntry {
                attempt: 1,
                agent: "claude".to_string(),
                model: Some("opus".to_string()),
                learnings: vec!["first learning".to_string()],
                error: None,
                files_modified: vec![],
                approach: "first approach".to_string(),
                timestamp: String::new(),
            },
        )
        .await
        .unwrap();
    store
        .append_memory(
            id,
            &MemoryEntry {
                attempt: 2,
                agent: "codex".to_string(),
                model: Some("gpt-5".to_string()),
                learnings: vec!["second learning".to_string()],
                error: None,
                files_modified: vec![],
                approach: "second approach".to_string(),
                timestamp: String::new(),
            },
        )
        .await
        .unwrap();

    let mem = store.recent_memory(id, 10).await.unwrap();
    assert_eq!(mem.len(), 2);
    assert_eq!(mem[0].approach, "first approach");
    assert_eq!(mem[1].approach, "second approach");
}

#[tokio::test]
async fn prune_old_runs_only_removes_old_terminal_tasks() {
    let store = TaskStore::open_memory().await.unwrap();

    // Create a done task
    let done_id = store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Old done".to_string(),
            body: String::new(),
            source: String::new(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store
        .update_status(done_id, TaskStatus::Done)
        .await
        .unwrap();

    backdate_done_entry(&store, done_id).await;

    // Add a run to the old done task
    store
        .start_run(&StartRun {
            task_id: done_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "cmd",
            prompt: "prompt",
        })
        .await
        .unwrap();

    // Create an active task with a run
    let active_id = store
        .create(&NewTask {
            external_id: Some("2".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Active".to_string(),
            body: String::new(),
            source: String::new(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store
        .start_run(&StartRun {
            task_id: active_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "cmd",
            prompt: "prompt",
        })
        .await
        .unwrap();

    // Prune runs older than 30 days
    let pruned = store.prune_old_runs(30).await.unwrap();
    assert_eq!(pruned, 1);

    let old_runs = store.get_runs(done_id).await.unwrap();
    assert_eq!(old_runs[0].prompt, "");

    let active_runs = store.get_runs(active_id).await.unwrap();
    assert_eq!(active_runs[0].prompt, "prompt");
}

#[tokio::test]
async fn foreign_key_constraint_on_task_runs() {
    let store = TaskStore::open_memory().await.unwrap();

    // Try to create a run for a non-existent task — should fail with FK constraint
    let result = store
        .start_run(&StartRun {
            task_id: 99999,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "cmd",
            prompt: "prompt",
        })
        .await;
    assert!(
        result.is_err(),
        "FK constraint should prevent orphaned runs"
    );
}

#[tokio::test]
async fn store_tokens_overwrites_previous_values() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Token test".to_string(),
            body: String::new(),
            source: String::new(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // First token store
    store.store_tokens(id, 1000, 500, None).await.unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.input_tokens, 1000);
    assert_eq!(task.output_tokens, 500);

    // Second store accumulates (not overwrites)
    store.store_tokens(id, 2000, 1000, None).await.unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.input_tokens, 3000);
    assert_eq!(task.output_tokens, 1500);
    // model is not written by store_tokens — it is set exclusively by routing
    assert_eq!(task.model, None);
}

#[tokio::test]
async fn get_last_run_returns_most_recent() {
    let store = TaskStore::open_memory().await.unwrap();
    let task_id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();

    // Create two runs
    let run1 = StartRun {
        task_id,
        attempt: 1,
        run_type: "agent",
        agent: "claude",
        model: "sonnet",
        command: "cmd1",
        prompt: "p1",
    };
    store.start_run(&run1).await.unwrap();

    let run2 = StartRun {
        task_id,
        attempt: 2,
        run_type: "agent",
        agent: "claude",
        model: "opus",
        command: "cmd2",
        prompt: "p2",
    };
    store.start_run(&run2).await.unwrap();

    let last = store.get_last_run(task_id, "agent").await.unwrap().unwrap();
    assert_eq!(last.attempt, 2);
    assert_eq!(last.model, "opus");
}

#[tokio::test]
async fn get_last_run_filters_by_type() {
    let store = TaskStore::open_memory().await.unwrap();
    let task_id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();

    let agent_run = StartRun {
        task_id,
        attempt: 1,
        run_type: "agent",
        agent: "claude",
        model: "sonnet",
        command: "",
        prompt: "",
    };
    store.start_run(&agent_run).await.unwrap();

    let review_run = StartRun {
        task_id,
        attempt: 1,
        run_type: "review",
        agent: "codex",
        model: "gpt-5",
        command: "",
        prompt: "",
    };
    store.start_run(&review_run).await.unwrap();

    let last_review = store
        .get_last_run(task_id, "review")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last_review.agent, "codex");
    assert_eq!(last_review.run_type, "review");

    // No route run exists
    let no_route = store.get_last_run(task_id, "route").await.unwrap();
    assert!(no_route.is_none());
}

/// Regression test for issue #3386.
///
/// `reset_failure_counters` used to reset `review_invocations` to 0.  When a
/// new review cycle started after that reset the first dispatch received
/// `attempt=1` again, and `start_run()`'s `ON CONFLICT DO UPDATE` overwrote the
/// previous `attempt=1` record — erasing the audit trail for the earlier review.
///
/// The production path is: `review_invocations` is incremented each time
/// `build_review_context()` runs and the resulting value is used directly as the
/// `attempt` key in `start_run()`.  When `reset_failure_counters` zeroed
/// `review_invocations`, the next invocation produced `attempt=1` again, causing
/// the ON CONFLICT clause to silently overwrite the previous record.
#[tokio::test]
async fn review_invocations_preserved_across_reset_prevents_task_run_overwrite() {
    let store = TaskStore::open_memory().await.unwrap();
    let task_id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();

    // ── Cycle A: a previous successful review reset the counter. ─────────────
    // Simulate: one review dispatch completed (attempt=1), then
    // reset_failure_counters was called (e.g. on Approve).
    store
        .increment(task_id, "review_invocations")
        .await
        .unwrap(); // → 1
    let prev_run_id = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "review",
            agent: "prev-agent",
            model: "prev-model",
            command: "",
            prompt: "",
        })
        .await
        .unwrap();
    store
        .complete_run(&CompleteRun {
            run_id: prev_run_id,
            exit_code: Some(0),
            stdout: "",
            stderr: "",
            parsed: "",
            outcome: "success",
            error: "",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();
    // End of cycle A: reset_failure_counters is called (Approve path).
    // With the OLD code this zeroed review_invocations back to 0.
    // With the fix it stays at 1.
    store.reset_failure_counters(task_id).await.unwrap();

    let after_reset = store.get(task_id).await.unwrap();
    assert_eq!(
        after_reset.review_invocations, 1,
        "review_invocations must survive reset_failure_counters (issue #3386)"
    );

    // ── Cycle B: codex billing-limit fast-fail, then kimi succeeds. ──────────
    // Increment → attempt=2 for codex (not 1, because review_invocations is 1).
    let codex_attempt = store
        .increment(task_id, "review_invocations")
        .await
        .unwrap(); // → 2
    let codex_run_id = store
        .start_run(&StartRun {
            task_id,
            attempt: codex_attempt,
            run_type: "review",
            agent: "codex",
            model: "gpt-5.4",
            command: "",
            prompt: "",
        })
        .await
        .unwrap();
    store
        .complete_run(&CompleteRun {
            run_id: codex_run_id,
            exit_code: Some(1),
            stdout: "",
            stderr: "billing limit",
            parsed: "",
            outcome: "rate_limit",
            error: "codex rate limit: billing cycle exhausted",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    // Kimi is dispatched as the retry (attempt=3).
    let kimi_attempt = store
        .increment(task_id, "review_invocations")
        .await
        .unwrap(); // → 3
    let kimi_run_id = store
        .start_run(&StartRun {
            task_id,
            attempt: kimi_attempt,
            run_type: "review",
            agent: "kimi",
            model: "kimi-model",
            command: "",
            prompt: "",
        })
        .await
        .unwrap();
    store
        .complete_run(&CompleteRun {
            run_id: kimi_run_id,
            exit_code: Some(0),
            stdout: "",
            stderr: "",
            parsed: r#"{"decision":"approve"}"#,
            outcome: "success",
            error: "",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    // All three records must be visible — codex's audit trail was NOT
    // overwritten because review_invocations was NOT zeroed by the earlier reset.
    let all_runs = store.get_runs(task_id).await.unwrap();
    let review_runs: Vec<_> = all_runs.iter().filter(|r| r.run_type == "review").collect();
    assert_eq!(
        review_runs.len(),
        3,
        "all three review runs must be in the audit trail"
    );

    let prev_run = review_runs.iter().find(|r| r.agent == "prev-agent");
    assert!(prev_run.is_some(), "cycle-A run must still be present");
    assert_eq!(prev_run.unwrap().attempt, 1);

    let codex_run = review_runs.iter().find(|r| r.agent == "codex");
    assert!(
        codex_run.is_some(),
        "codex billing-limit run must be present (was erased by the bug)"
    );
    assert_eq!(codex_run.unwrap().outcome, "rate_limit");
    assert_eq!(codex_run.unwrap().attempt, 2);

    let kimi_run = review_runs.iter().find(|r| r.agent == "kimi");
    assert!(kimi_run.is_some(), "kimi success run must be present");
    assert_eq!(kimi_run.unwrap().outcome, "success");
    assert_eq!(kimi_run.unwrap().attempt, 3);
}

#[tokio::test]
async fn prune_old_runs_removes_completed_task_runs() {
    let store = TaskStore::open_memory().await.unwrap();
    let task_id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();
    store
        .update_status(task_id, TaskStatus::Done)
        .await
        .unwrap();
    backdate_done_entry(&store, task_id).await;

    let run = StartRun {
        task_id,
        attempt: 1,
        run_type: "agent",
        agent: "claude",
        model: "sonnet",
        command: "",
        prompt: "do stuff",
    };
    store.start_run(&run).await.unwrap();

    let pruned = store.prune_old_runs(30).await.unwrap();
    assert_eq!(pruned, 1);
    assert_eq!(store.get_runs(task_id).await.unwrap()[0].prompt, "");
}

#[tokio::test]
async fn prune_old_runs_keeps_recent_task_runs() {
    let store = TaskStore::open_memory().await.unwrap();
    let task_id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();
    store
        .update_status(task_id, TaskStatus::Done)
        .await
        .unwrap();
    // Task stays with recent updated_at (default = now)

    let run = StartRun {
        task_id,
        attempt: 1,
        run_type: "agent",
        agent: "claude",
        model: "sonnet",
        command: "",
        prompt: "do stuff",
    };
    store.start_run(&run).await.unwrap();

    let pruned = store.prune_old_runs(30).await.unwrap();
    assert_eq!(pruned, 0);
    assert_eq!(store.get_runs(task_id).await.unwrap()[0].prompt, "do stuff");
}

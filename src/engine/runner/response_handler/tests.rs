use std::fs as std_fs;
use std::process::Command;

use super::*;
use crate::store::NewTask;

/// A successful run between timeouts restarts the count, so only timeouts in a row block a task
#[tokio::test]
async fn successful_run_resets_timeout_retries() {
    let store = Some(Arc::new(TaskStore::open_memory().await.unwrap()));
    let id = store
        .as_ref()
        .unwrap()
        .create_internal("owner/repo", "t", "b", "test", "", None)
        .await
        .unwrap();
    let task_id = format!("internal:{id}");
    let ctx = StoreCtx {
        store: &store,
        store_id_opt: None,
        repo: "owner/repo",
        task_id: &task_id,
    };

    ctx.increment("timeout_retries").await.unwrap();
    ctx.increment("network_retries").await.unwrap();
    ctx.reset_retry_streaks().await;
    ctx.increment("timeout_retries").await.unwrap();
    ctx.increment("timeout_retries").await.unwrap();

    let task = ctx.get_task().await.unwrap();
    assert_eq!(task.timeout_retries, 2);
    assert_eq!(task.network_retries, 0);
    assert_ne!(task.status.as_str(), "blocked");
}

#[test]
fn local_rate_limit_skip_is_detected() {
    assert!(is_local_rate_limit_skip(
        "GitHub API error: GitHub REST API rate-limited, skipping call for 236s"
    ));
    assert!(!is_local_rate_limit_skip(
        "GitHub API POST failed (502): Bad Gateway"
    ));
}

/// Helper: run git in `dir`, panicking on failure
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to start: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_clone(remote: &Path, dir: &Path) {
    git(dir, &["clone", remote.to_str().unwrap(), "."]);
    git(dir, &["config", "user.email", "t@t.com"]);
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

#[test]
fn remote_rejected_failure_is_not_diverged() {
    let err = "remote: fatal error in commit_refs\n ! [remote rejected] b -> b (failure)";
    assert!(!push_error_is_diverged(err));
    assert!(push_error_is_diverged("! [rejected] b -> b (fetch first)"));
    assert!(push_error_is_diverged(
        "! [rejected] b -> b (non-fast-forward)"
    ));
}

/**
 * Someone rebased the PR branch from another clone and force-pushed it, so orch's
 * push is rejected with a stale lease. Pushing is orch's own step: it must rebase
 * onto the moved branch and push again, never hand finished work back to the agent
 * as a fresh attempt (#3783)
 **/
#[tokio::test]
async fn push_rejected_after_remote_rebase_is_pushed_not_rerouted() {
    let remote = tempfile::tempdir().unwrap();
    git(remote.path(), &["init", "--bare", "-b", "main"]);

    // orch's worktree: main, then a feature branch with the task's commit
    let work = tempfile::tempdir().unwrap();
    init_clone(remote.path(), work.path());
    std_fs::write(work.path().join("README.md"), "base").unwrap();
    git(work.path(), &["add", "."]);
    git(work.path(), &["commit", "-m", "init"]);
    git(work.path(), &["push", "origin", "HEAD:main"]);
    git(work.path(), &["checkout", "-b", "feat"]);
    std_fs::write(work.path().join("fix.txt"), "the fix").unwrap();
    git(work.path(), &["add", "."]);
    git(work.path(), &["commit", "-m", "apply the fix"]);
    git(work.path(), &["push", "-u", "origin", "feat"]);

    // Another clone rewrites the same change and force-pushes it
    let other = tempfile::tempdir().unwrap();
    init_clone(remote.path(), other.path());
    git(other.path(), &["checkout", "feat"]);
    git(
        other.path(),
        &["commit", "--amend", "-m", "apply the fix (rebased)"],
    );
    git(other.path(), &["push", "--force", "origin", "feat"]);

    // orch now has an unpushed commit of its own on top of the stale branch
    std_fs::write(work.path().join("more.txt"), "follow-up").unwrap();
    git(work.path(), &["add", "."]);
    git(work.path(), &["commit", "-m", "follow-up"]);

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let store_id = store
        .create_internal("owner/repo", "Push recovery", "", "task", "3783", None)
        .await
        .unwrap();
    let task_id = format!("internal:{store_id}");
    let store_opt = Some(Arc::clone(&store));
    let ctx = StoreCtx {
        store: &store_opt,
        store_id_opt: Some(store_id),
        repo: "owner/repo",
        task_id: &task_id,
    };

    let result =
        push_branch_with_log(&ctx, work.path(), "feat", "main", "claude", Some("opus")).await;

    assert!(
        matches!(result, PushResult::Success),
        "orch must land the push itself after adopting the moved remote branch"
    );

    let task = store.get(store_id).await.unwrap();
    assert!(
        task.last_error.is_empty(),
        "a recovered push must leave no error behind, got: {}",
        task.last_error
    );
    assert_eq!(task.push_failures, 0, "recovery is not a push failure");

    // push_branch recovers the stale lease itself, so the log below is the proof: the
    // follow-up commit is on the remote, on top of the rebased history
    let log = Command::new("git")
        .current_dir(work.path())
        .args(["log", "--oneline", "origin/feat"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(
        log.contains("follow-up"),
        "remote must carry the task's commit: {log}"
    );
    assert!(
        log.contains("apply the fix (rebased)"),
        "orch must adopt the rebased remote history, not re-push its own: {log}"
    );
    assert!(
        !log.contains("apply the fix\n"),
        "the superseded commit must not come back: {log}"
    );
}

#[test]
fn missing_path_error_detects_io_not_found() {
    let err = anyhow::Error::from(io::Error::new(
        ErrorKind::NotFound,
        "simulated missing path",
    ));
    assert!(is_missing_path_error(&err));
}

#[test]
fn missing_path_error_detects_message_fallback() {
    let err = anyhow::anyhow!("push failed: No such file or directory (os error 2)");
    assert!(is_missing_path_error(&err));
}

#[test]
fn superseded_conflict_pr_requires_conflict_reroute_and_no_commits() {
    // Conflict reroute, nothing ahead of the default branch → close the PR.
    assert_eq!(superseded_conflict_pr(1, Some(42), false), Some(42));
    // Commits exist → the agent did work, keep the PR.
    assert_eq!(superseded_conflict_pr(1, Some(42), true), None);
    // No conflict reroute → an ordinary no-code run.
    assert_eq!(superseded_conflict_pr(0, Some(42), false), None);
    // No PR to close.
    assert_eq!(superseded_conflict_pr(1, None, false), None);
}

/// End-to-end: a conflict reroute that comes back with zero commits ahead of
/// the default branch closes the superseded PR and marks the task done,
/// instead of blocking it as a same-agent no-code loop (#3780).
#[tokio::test]
async fn conflict_reroute_with_no_commits_closes_pr_and_marks_done() {
    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git runs")
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    // A worktree whose branch has nothing ahead of origin/main — the state
    // the agent leaves behind when the fix already landed on the base.
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin.git");
    std_fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "--bare", "-b", "main"]);
    let work_dir = dir.path().join("work");
    std_fs::create_dir(&work_dir).unwrap();
    git(&work_dir, &["init", "-b", "main"]);
    git(
        &work_dir,
        &["remote", "add", "origin", &origin.display().to_string()],
    );
    std_fs::write(work_dir.join("a.txt"), "a").unwrap();
    git(&work_dir, &["add", "-A"]);
    git(&work_dir, &["commit", "-m", "init"]);
    git(&work_dir, &["push", "-u", "origin", "main"]);
    git(&work_dir, &["checkout", "-b", "fix-branch"]);

    let wt = worktree::WorktreeSetup {
        work_dir: work_dir.clone(),
        branch: "fix-branch".to_string(),
        default_branch: "main".to_string(),
        main_project_dir: work_dir.clone(),
    };

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let store_id = store
        .create(&NewTask {
            external_id: Some("3772".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Fix the thing".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: "bot".to_string(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store
        .set_fields(store_id, &[("pr_number", serde_json::json!(42))])
        .await
        .unwrap();
    // review_poll rerouted this task to resolve a merge conflict.
    store
        .increment(store_id, "merge_conflict_retries")
        .await
        .unwrap();
    // A previous no-code run by the same agent — without the superseded
    // check this run would block the task as a same-agent loop.
    store
        .set_fields(
            store_id,
            &[("no_code_last_agent", serde_json::json!("claude"))],
        )
        .await
        .unwrap();

    let parsed = agents::ParsedResponse {
        response: AgentResponse {
            status: "done".to_string(),
            summary: "the fix is already on main".to_string(),
            ..Default::default()
        },
        input_tokens: None,
        output_tokens: None,
        cost_usd: None,
        duration_ms: None,
    };

    let (final_status, push_failed) = handle_success(
        "3772",
        parsed,
        &wt,
        "Fix the thing",
        "claude",
        Some("sonnet"),
        1,
        "owner/repo",
        &Some(Arc::clone(&store)),
        "",
    )
    .await
    .unwrap();

    assert_eq!(
        final_status, "done",
        "a superseded conflict reroute must end done, not blocked"
    );
    assert!(!push_failed);

    let task = store.get(store_id).await.unwrap();
    assert!(
        task.last_error.contains("superseded"),
        "the close attempt must be recorded on the task: {}",
        task.last_error
    );
    assert_eq!(
        task.pr_number, None,
        "a superseded PR must not stay on the task, the blocker refresh would read it as closed unmerged"
    );
}

/// A conflict reroute with zero commits ahead marks the task done instead of
/// blocking it as a same-agent no-code loop (#3780).
#[test]
fn classify_superseded_conflict_marks_done() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "done",
        requires_pr: true,
        is_same_agent: true,
        is_superseded_conflict: true,
        ..Default::default()
    });
    assert_eq!(status, "done");
}

// ── classify_final_status — one test per decision branch ─────────────────

/// Branch 1: workflow-scope push failure → block immediately (non-retryable).
#[test]
fn classify_workflow_scope_failure_blocks() {
    let status = classify_final_status(&DecisionInput {
        is_workflow_scope_failure: true,
        push_failed: true,
        push_failures: 1,
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// Branch 1b: automatic rebase recovery failed (conflicts) → block immediately.
#[test]
fn classify_rebase_conflict_failure_blocks() {
    let status = classify_final_status(&DecisionInput {
        is_rebase_conflict_failure: true,
        push_failed: true,
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// Branch 2a: push failed, < 3 times → reroute with "new".
#[test]
fn classify_push_failed_under_threshold_reroutes() {
    let status = classify_final_status(&DecisionInput {
        push_failed: true,
        push_failures: 2, // < 3
        ..Default::default()
    });
    assert_eq!(status, "new");
}

#[test]
fn denied_push_block_detected() {
    let resp = |s: &str| AgentResponse {
        status: "blocked".to_string(),
        summary: s.to_string(),
        ..Default::default()
    };
    assert!(is_denied_push_block(&resp(
        "Work committed, but `git push -u origin HEAD` was denied"
    )));
    assert!(is_denied_push_block(&resp(
        "It is not pushed: `git push --force-with-lease` needs approval that this session could not get."
    )));
    assert!(is_denied_push_block(&resp(
        "`git push --force-with-lease` was denied because this session cannot get approval"
    )));
    assert!(is_denied_push_block(&resp("The push requires approval")));
    assert!(!is_denied_push_block(&resp("Missing API key")));
}

/// Branch 2b: push failed >= 3 times → block for human intervention.
#[test]
fn classify_push_failed_at_threshold_blocks() {
    let status = classify_final_status(&DecisionInput {
        push_failed: true,
        push_failures: 3, // >= 3
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// A GitHub-side push failure never reaches the router, the engine retries the push (#4025).
#[test]
fn classify_transient_push_failure_waits_for_the_engine_retry() {
    for push_failures in 1..push_retry::MAX_PUSH_FAILURES {
        let status = classify_final_status(&DecisionInput {
            push_failed: true,
            is_transient_push_failure: true,
            push_failures,
            ..Default::default()
        });
        assert_eq!(status, "needs_review", "push_failures={push_failures}");
    }
}

#[test]
fn classify_transient_push_failure_blocks_after_the_last_retry() {
    let status = classify_final_status(&DecisionInput {
        push_failed: true,
        is_transient_push_failure: true,
        push_failures: push_retry::MAX_PUSH_FAILURES,
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// Branch 3: done + PR exists → send to review.
#[test]
fn classify_done_with_pr_goes_to_needs_review() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "done",
        has_pr: true,
        ..Default::default()
    });
    assert_eq!(status, "needs_review");
}

/// Branch 4: done + delegations, no PR → block (waiting on child tasks).
#[test]
fn classify_done_with_delegations_no_pr_blocks() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "done",
        has_delegations: true,
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// Branch 5: done + commits pushed but PR creation failed → re-dispatch.
#[test]
fn classify_done_pushed_no_pr_reroutes_for_pr_creation() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "done",
        has_pushed: true,
        ..Default::default()
    });
    assert_eq!(status, "routed");
}

/// Branch 5b: done + pushed + PR base invalid → block (terminal error).
#[test]
fn classify_done_pushed_pr_base_invalid_blocks() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "done",
        has_pushed: true,
        is_pr_base_invalid: true,
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// Branch 5c: completed + pushed + PR base invalid → block (terminal error).
#[test]
fn classify_completed_pushed_pr_base_invalid_blocks() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "completed",
        is_completed_status: true,
        has_pushed: true,
        is_pr_base_invalid: true,
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// Branch 6a: done + requires_pr, under max reroutes → reroute.
#[test]
fn classify_done_requires_pr_under_max_reroutes() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "done",
        requires_pr: true,
        no_code_reroutes: 1,
        max_reroutes: 3,
        is_same_agent: false,
        ..Default::default()
    });
    assert_eq!(status, "new");
}

/// Branch 6a edge: same agent on no-code reroute → block immediately.
#[test]
fn classify_done_requires_pr_same_agent_blocks_immediately() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "done",
        requires_pr: true,
        no_code_reroutes: 1,
        max_reroutes: 3,
        is_same_agent: true, // Same agent would be selected again
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// Branch 6b: done + requires_pr, at max reroutes → block.
#[test]
fn classify_done_requires_pr_at_max_reroutes_blocks() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "done",
        requires_pr: true,
        no_code_reroutes: 3,
        max_reroutes: 3,
        is_same_agent: false,
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// Branch 6b edge: exceeding max also blocks.
#[test]
fn classify_done_requires_pr_exceeds_max_reroutes_blocks() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "done",
        requires_pr: true,
        no_code_reroutes: 5,
        max_reroutes: 3,
        is_same_agent: false,
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// Same agent on external task requiring PR → block immediately (not wait for max).
#[test]
fn classify_done_external_no_pushed_same_agent_blocks_immediately() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "done",
        requires_pr: true, // always true for external tasks with !has_pr + done
        no_code_reroutes: 0,
        max_reroutes: 3,
        is_same_agent: true,
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// External task with no pushed commits: reroute (agent must produce commits
/// before the issue can be closed). This was the source of issue #1898 — an
/// agent claiming "already implemented" without any code changes would
/// previously match non-code keywords and close the issue falsely.
///
/// External tasks with done+!has_pr+!has_pushed always have requires_pr=true
/// (see handle_success), so they are caught by branch 6, not branch 7.
#[test]
fn classify_done_external_no_pushed_commits_reroutes() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "done",
        has_pushed: false,
        requires_pr: true, // always true for external tasks with !has_pr + done
        no_code_reroutes: 0,
        max_reroutes: 3,
        is_same_agent: false,
        ..Default::default()
    });
    assert_eq!(status, "new");
}

/// Internal task with no pushed commits may still be marked done — internal
/// tasks may produce no git-visible changes.
#[test]
fn classify_done_internal_task_no_pr_is_done() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "done",
        has_pushed: false,
        requires_pr: false,
        ..Default::default()
    });
    assert_eq!(status, "done");
}

/// Pass-through: non-"done" status is returned unchanged.
#[test]
fn classify_non_done_status_passes_through() {
    for status_str in &["in_progress", "needs_review", "blocked"] {
        let result = classify_final_status(&DecisionInput {
            agent_status: status_str,
            ..Default::default()
        });
        assert_eq!(
            result, *status_str,
            "status '{status_str}' should pass through unchanged"
        );
    }
}

/// Descriptive completion text should normalize to done.
#[test]
fn classify_descriptive_completion_status_normalizes_to_done() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "Trading scan complete. Nothing to trade.",
        ..Default::default()
    });
    assert_eq!(status, "done");
}

/// Do not normalize to done when completion-like text includes failure cues.
#[test]
fn classify_descriptive_completion_with_failure_cues_does_not_normalize() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "completed but failed to push branch due to error",
        ..Default::default()
    });
    assert_eq!(status, "completed but failed to push branch due to error");
}

/// push_failed takes precedence over done+has_pr (push failure detected first).
#[test]
fn classify_push_failed_takes_precedence_over_has_pr() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "done",
        has_pr: true,
        push_failed: true,
        push_failures: 1,
        ..Default::default()
    });
    // push_failed branch runs before done+has_pr check
    assert_eq!(status, "new");
}

/// workflow_scope_failure takes precedence over push_failed < 3.
#[test]
fn classify_workflow_scope_failure_takes_precedence_over_push_reroute() {
    let status = classify_final_status(&DecisionInput {
        is_workflow_scope_failure: true,
        push_failed: true,
        push_failures: 1, // would normally reroute
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

// ── "completed" status tests ────────────────────────────────────────────────

/// "completed" + has_pr → needs_review.
#[test]
fn classify_completed_with_pr_goes_to_needs_review() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "completed",
        has_pr: true,
        is_completed_status: true,
        ..Default::default()
    });
    assert_eq!(status, "needs_review");
}

/// "completed" + delegations, no PR → blocked.
#[test]
fn classify_completed_with_delegations_no_pr_blocks() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "completed",
        has_delegations: true,
        is_completed_status: true,
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// "completed" + pushed, no PR → routed for PR creation.
#[test]
fn classify_completed_pushed_no_pr_reroutes_for_pr_creation() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "completed",
        has_pushed: true,
        is_completed_status: true,
        ..Default::default()
    });
    assert_eq!(status, "routed");
}

/// "completed" + external task requiring PR, under max reroutes → reroute.
#[test]
fn classify_completed_requires_pr_under_max_reroutes() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "completed",
        requires_pr: true,
        no_code_reroutes: 1,
        max_reroutes: 3,
        is_completed_status: true,
        is_same_agent: false,
        ..Default::default()
    });
    assert_eq!(status, "new");
}

/// "completed" + external task, at max reroutes → block.
#[test]
fn classify_completed_requires_pr_at_max_reroutes_blocks() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "completed",
        requires_pr: true,
        no_code_reroutes: 3,
        max_reroutes: 3,
        is_completed_status: true,
        is_same_agent: false,
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// "completed" + external task, same agent on no-code reroute → block immediately.
#[test]
fn classify_completed_same_agent_blocks_immediately() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "completed",
        requires_pr: true,
        no_code_reroutes: 1,
        max_reroutes: 3,
        is_completed_status: true,
        is_same_agent: true,
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

/// "completed" on internal task (no requires_pr) → mark done.
#[test]
fn classify_completed_internal_task_is_done() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "completed",
        requires_pr: false,
        is_completed_status: true,
        ..Default::default()
    });
    assert_eq!(status, "done");
}

// ── agent_blocked_reason — regression tests for issue #3012 ──────────────

/// Agent explicitly returned blocked with an error field — use error as reason.
#[test]
fn agent_blocked_reason_prefers_error_field() {
    let resp = AgentResponse {
        status: "blocked".to_string(),
        error: Some("CDP endpoint not reachable".to_string()),
        summary: "Could not complete research".to_string(),
        remaining: vec!["Run x-twitter-brave with reachable endpoint".to_string()],
        ..Default::default()
    };
    let reason = agent_blocked_reason(&resp);
    assert!(
        reason.contains("CDP endpoint not reachable"),
        "reason: {reason}"
    );
    assert!(reason.contains("Remaining:"), "reason: {reason}");
    assert!(reason.contains("x-twitter-brave"), "reason: {reason}");
}

/// No error field — falls back to summary.
#[test]
fn agent_blocked_reason_falls_back_to_summary() {
    let resp = AgentResponse {
        status: "blocked".to_string(),
        error: None,
        summary: "Could not access required data sources in sandboxed environment".to_string(),
        remaining: vec![],
        ..Default::default()
    };
    let reason = agent_blocked_reason(&resp);
    assert_eq!(
        reason,
        "Could not access required data sources in sandboxed environment"
    );
}

/// No error or summary — returns generic fallback.
#[test]
fn agent_blocked_reason_generic_fallback() {
    let resp = AgentResponse {
        status: "blocked".to_string(),
        ..Default::default()
    };
    let reason = agent_blocked_reason(&resp);
    assert_eq!(reason, "agent returned blocked status without a reason");
}

/// Agent-returned "blocked" status passes through classify_final_status unchanged.
#[test]
fn classify_agent_blocked_passes_through() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "blocked",
        ..Default::default()
    });
    assert_eq!(status, "blocked");
}

#[test]
fn classify_retryable_agent_blocked_reroutes() {
    let status = classify_final_status(&DecisionInput {
        agent_status: "blocked",
        is_retryable_blocked: true,
        ..Default::default()
    });
    assert_eq!(status, "new");
}

#[test]
fn retryable_blocked_reason_detects_transient_patterns() {
    assert!(is_retryable_blocked_reason(
        "Twitter access unavailable: service timed out"
    ));
    assert!(is_retryable_blocked_reason(
        "final commit blocked by git worktree lock permission error"
    ));
    assert!(is_retryable_blocked_reason(
        "could not commit due a worktree git lock permission error"
    ));
    assert!(is_retryable_blocked_reason(
        "git lockfile permission restrictions in this environment"
    ));
    assert!(is_retryable_blocked_reason(
        "API rate limit hit, try again later"
    ));
    assert!(is_retryable_blocked_reason(
        "Credential bean/hyperliquid-address exists in the pass store but GPG decryption fails in this agent session — no passphrase available."
    ));
    assert!(is_retryable_blocked_reason(
        "credential resolution failed via PasswordStoreProvider; AgentVaultProvider unavailable"
    ));
}

#[test]
fn retryable_blocked_reason_rejects_permanent_patterns() {
    assert!(!is_retryable_blocked_reason(
        "invalid request payload: malformed JSON"
    ));
    assert!(!is_retryable_blocked_reason(
        "resource not found in repository"
    ));
}

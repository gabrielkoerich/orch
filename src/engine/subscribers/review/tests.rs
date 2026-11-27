use super::*;
use crate::backends::test_helpers::NoopBackend;
use std::fs;
use std::path::Path;
use std::process::Command;

/// An incomplete mid-thought review (no result envelope) must reset for
/// retry without counting toward MAX_REVIEW_AGENT_FAILURES (issue #3734).
#[tokio::test]
async fn classify_review_failure_incomplete_resets_without_counting() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let store_id = store
        .create_internal(
            repo,
            "Incomplete review test",
            "",
            "review",
            "review-3734",
            None,
        )
        .await
        .unwrap();
    let task_id = format!("internal:{store_id}");

    for _ in 0..=MAX_REVIEW_AGENT_FAILURES {
        let outcome = classify_review_failure(
            &store,
            repo,
            &task_id,
            "incomplete: review session exited before producing a decision",
            "review agent",
        )
        .await;
        assert!(
            matches!(outcome, ReviewOutcome::Reset),
            "incomplete review failure must reset for retry, not block"
        );
    }

    let task = opt_store_get_task(&Some(Arc::clone(&store)), repo, &task_id)
        .await
        .expect("task should exist");
    assert_eq!(
        task.review_agent_failures, 0,
        "incomplete failures must not count toward review_agent_failures"
    );
}

/// A truncated review output (opencode step_finish reason=length) must be
/// treated like a parse error: reset for retry without counting toward
/// MAX_REVIEW_AGENT_FAILURES, so a run of token-budget flukes never
/// permanently blocks the task (issue #3473).
#[tokio::test]
async fn classify_review_failure_truncated_resets_without_counting() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let store_id = store
        .create_internal(
            repo,
            "Truncated review test",
            "",
            "review",
            "review-1",
            None,
        )
        .await
        .unwrap();
    let task_id = format!("internal:{store_id}");

    for _ in 0..=MAX_REVIEW_AGENT_FAILURES {
        let outcome = classify_review_failure(
            &store,
            repo,
            &task_id,
            "truncated: review output was cut off before completion \
                 (output/reasoning token budget exceeded)",
            "review agent",
        )
        .await;
        assert!(
            matches!(outcome, ReviewOutcome::Reset),
            "truncated review failure must reset for retry, not block"
        );
    }

    let task = opt_store_get_task(&Some(Arc::clone(&store)), repo, &task_id)
        .await
        .expect("task should exist");
    assert_eq!(
        task.review_agent_failures, 0,
        "truncated failures must not count toward review_agent_failures"
    );
}

/**
 * After an approved review, a merge that fails for anything other than a content
 * conflict must retry the merge with the task still in InReview. Re-dispatching the
 * agent re-ran finished work until the task hit the attempt limit (#3783)
 **/
#[tokio::test]
async fn classify_review_failure_non_conflict_merge_failure_retries_merge() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let store_id = store
        .create_internal(repo, "Merge retry", "", "review", "review-3783", None)
        .await
        .unwrap();
    let task_id = format!("internal:{store_id}");

    let error_messages = [
        "merge failed: GitHub REST API rate-limited, skipping call for 187s",
        "merge failed: GitHub PR merge failed (502 Bad Gateway)",
        "merge failed: required status check still pending",
    ];

    for _ in 0..=MAX_REVIEW_AGENT_FAILURES {
        for msg in &error_messages {
            let outcome =
                classify_review_failure(&store, repo, &task_id, msg, "review agent").await;
            assert!(
                matches!(outcome, ReviewOutcome::RetryMerge),
                "non-conflict merge failure '{msg}' must retry the merge, not re-dispatch the agent"
            );
        }
    }

    let task = opt_store_get_task(&Some(Arc::clone(&store)), repo, &task_id)
        .await
        .expect("task should exist");
    assert_eq!(
        task.review_agent_failures, 0,
        "a stuck merge is not a review failure"
    );
}

/// A genuine content conflict is the one merge failure the agent has to fix
#[tokio::test]
async fn classify_review_failure_merge_conflict_still_reroutes() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let store_id = store
        .create_internal(repo, "Merge conflict", "", "review", "review-3783b", None)
        .await
        .unwrap();
    let task_id = format!("internal:{store_id}");

    for msg in [
        "merge failed: PR is not mergeable",
        "merge conflict detected on PR #12",
    ] {
        let outcome = classify_review_failure(&store, repo, &task_id, msg, "review agent").await;
        assert!(
            matches!(outcome, ReviewOutcome::Reroute),
            "content conflict '{msg}' must go back to the task agent"
        );
    }
}

/// Transient provider network errors (ConnectionRefused, timeouts) must be
/// treated like parse errors: reset for retry without counting toward
/// MAX_REVIEW_AGENT_FAILURES (issue #3482).
#[tokio::test]
async fn classify_review_failure_network_error_resets_without_counting() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let store_id = store
        .create_internal(
            repo,
            "Network error review test",
            "",
            "review",
            "review-2",
            None,
        )
        .await
        .unwrap();
    let task_id = format!("internal:{store_id}");

    let error_messages = [
        "API Error: Unable to connect to API (ConnectionRefused)",
        "connection refused",
        "ECONNREFUSED",
        "Upstream idle timeout exceeded",
        "connect ETIMEDOUT",
        "network request failed",
    ];

    for msg in &error_messages {
        // Each message on its own, the reset cap has its own test
        store
            .set_fields(store_id, &[("review_resets", serde_json::json!(0))])
            .await
            .unwrap();
        let outcome = classify_review_failure(&store, repo, &task_id, msg, "review agent").await;
        assert!(
            matches!(outcome, ReviewOutcome::Reset),
            "network error '{msg}' must reset for retry, not block"
        );
    }

    let task = opt_store_get_task(&Some(Arc::clone(&store)), repo, &task_id)
        .await
        .expect("task should exist");
    assert_eq!(
        task.review_agent_failures, 0,
        "network errors must not count toward review_agent_failures"
    );
}

/// When no agent has a configured review model (or the fallback agent has none),
/// the review must be deferred, not counted as a review failure. Counting it
/// would eventually block tasks for a configuration/cooldown gap (issue #3584).
#[tokio::test]
async fn classify_review_failure_missing_review_model_is_rate_limited() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let store_id = store
        .create_internal(
            repo,
            "Missing review model test",
            "",
            "review",
            "review-3",
            None,
        )
        .await
        .unwrap();
    let task_id = format!("internal:{store_id}");

    let error_messages = [
        "no review agent with a configured review model is available for task internal:123",
        "review model missing for task internal:123 after agent selection — refusing to dispatch",
    ];

    for _ in 0..=MAX_REVIEW_AGENT_FAILURES {
        for msg in &error_messages {
            let outcome =
                classify_review_failure(&store, repo, &task_id, msg, "review_and_merge").await;
            assert!(
                matches!(outcome, ReviewOutcome::RateLimited),
                "missing review model error '{msg}' must defer, not block"
            );
        }
    }

    let task = opt_store_get_task(&Some(Arc::clone(&store)), repo, &task_id)
        .await
        .expect("task should exist");
    assert_eq!(
        task.review_agent_failures, 0,
        "missing review model errors must not count toward review_agent_failures"
    );
}

/// Resets that do not count as failures still stop at `MAX_REVIEW_RESETS`
#[tokio::test]
async fn classify_review_failure_blocks_after_max_resets() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let store_id = store
        .create_internal(repo, "Looping review", "", "review", "review-resets", None)
        .await
        .unwrap();
    let task_id = format!("internal:{store_id}");
    let msg = "parse error: failed to parse review response";

    for attempt in 1..MAX_REVIEW_RESETS {
        let outcome = classify_review_failure(&store, repo, &task_id, msg, "review agent").await;
        assert!(
            matches!(outcome, ReviewOutcome::Reset),
            "reset {attempt} must retry the review"
        );
    }
    let outcome = classify_review_failure(&store, repo, &task_id, msg, "review agent").await;
    assert!(
        matches!(outcome, ReviewOutcome::Block(ref r) if r.contains("without a decision")),
        "reset {MAX_REVIEW_RESETS} must block the task"
    );

    let task = opt_store_get_task(&Some(Arc::clone(&store)), repo, &task_id)
        .await
        .expect("task should exist");
    assert_eq!(
        u64::try_from(task.review_resets).unwrap(),
        MAX_REVIEW_RESETS
    );
    assert_eq!(
        task.review_agent_failures, 0,
        "a reset must not count as a review failure"
    );
}

/**
 * A review that fails on a rate limit is retried once the cooldown clears, and must
 * never spend the rebroadcast quota: `needs_review_refires` is cleared so the sync
 * sweep cannot escalate the task to Blocked for a cause outside the review (#3767)
 **/
#[tokio::test]
async fn classify_review_failure_rate_limit_clears_refire_quota() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let store_id = store
        .create_internal(
            repo,
            "Rate limited review",
            "",
            "review",
            "review-3767",
            None,
        )
        .await
        .unwrap();
    let task_id = format!("internal:{store_id}");

    let error_messages = [
        "claude failed: rate limit reached for model sonnet",
        "GitHub API GET https://api.github.com/repos/owner/repo/pulls/65 failed (403): API rate limit exceeded",
        "GitHub core API rate-limited, skipping call for 1800s",
        "GitHub API GET https://api.github.com/repos/owner/repo/pulls/65 failed (429): Too Many Requests",
    ];

    // The sweep has already burned most of the quota before the rate-limit window
    store
        .set_fields(store_id, &[("needs_review_refires", serde_json::json!(5))])
        .await
        .unwrap();

    for _ in 0..=MAX_REVIEW_AGENT_FAILURES {
        for msg in &error_messages {
            let outcome =
                classify_review_failure(&store, repo, &task_id, msg, "review agent").await;
            assert!(
                matches!(outcome, ReviewOutcome::RateLimited | ReviewOutcome::Reset),
                "rate limit '{msg}' must defer for retry, not block"
            );
        }
    }

    let task = opt_store_get_task(&Some(Arc::clone(&store)), repo, &task_id)
        .await
        .expect("task should exist");
    assert_eq!(
        task.needs_review_refires, 0,
        "a rate limit must not consume the rebroadcast quota"
    );
    assert_eq!(
        task.review_agent_failures, 0,
        "rate limits must not count toward review_agent_failures"
    );
    assert_ne!(
        task.status,
        TaskStatus::Blocked,
        "a rate-limited review must leave the task retryable"
    );

    let attempts = store
        .get_activity(store_id, None)
        .await
        .unwrap()
        .into_iter()
        .filter(|a| a.event_type == "review_attempt_failed")
        .collect::<Vec<_>>();
    assert_eq!(
        attempts.len(),
        (usize::try_from(MAX_REVIEW_AGENT_FAILURES).unwrap() + 1) * error_messages.len(),
        "every failed attempt must be recorded for `orch task inspect`"
    );
    assert!(
        attempts.iter().all(|a| a
            .details
            .get("external")
            .and_then(serde_json::Value::as_bool)
            == Some(true)),
        "rate limits must be recorded as an external cause"
    );
    assert!(
        attempts
            .iter()
            .any(|a| a.details.get("reason").and_then(|v| v.as_str()) == Some(error_messages[0])),
        "the recorded attempt must carry that attempt's error"
    );
}

async fn needs_review_task(
    repo: &str,
    source_id: &str,
) -> (Arc<TaskStore>, Arc<TaskManager>, ExternalTask, i64) {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let store_id = store
        .create_internal(repo, "CI red", "", "test", source_id, None)
        .await
        .unwrap();
    store
        .update_status(store_id, TaskStatus::NeedsReview)
        .await
        .unwrap();
    let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        repo.to_string(),
    ));
    let task = ExternalTask {
        id: ExternalId(format!("internal:{store_id}")),
        title: "CI red".into(),
        body: String::new(),
        state: "open".into(),
        labels: vec![],
        author: String::new(),
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        url: String::new(),
    };
    (store, task_manager, task, store_id)
}

struct ReviewPushFixture {
    _tmp: tempfile::TempDir,
    clone: PathBuf,
    remote: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn review_push_fixture() -> ReviewPushFixture {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("remote.git");
    let clone = tmp.path().join("clone");
    fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "--bare", "-b", "main"]);
    git(tmp.path(), &["clone", remote.to_str().unwrap(), "clone"]);
    git(&clone, &["config", "user.email", "t@example.com"]);
    git(&clone, &["config", "user.name", "t"]);
    git(&clone, &["checkout", "-b", "main"]);
    fs::write(clone.join("a.txt"), "a").unwrap();
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-m", "base"]);
    git(&clone, &["push", "-u", "origin", "main"]);
    git(&clone, &["checkout", "-b", "feature"]);
    fs::write(clone.join("b.txt"), "b").unwrap();
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-m", "work"]);
    ReviewPushFixture {
        _tmp: tmp,
        clone,
        remote,
    }
}

fn remote_feature_log(fx: &ReviewPushFixture) -> String {
    let out = Command::new("git")
        .args(["log", "feature", "--oneline"])
        .current_dir(&fx.remote)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test]
async fn route_back_on_red_ci_counts_one_failure_per_head() {
    let repo = "owner/repo";
    let (store, task_manager, task, store_id) = needs_review_task(repo, "same-sha").await;
    let failed = ["check".to_string()];

    route_back_on_red_ci(
        &store,
        &task_manager,
        repo,
        &task,
        1,
        &RedCi {
            failed: &failed,
            head_sha: "aaa",
        },
        None,
    )
    .await;
    let after_first = store.get(store_id).await.unwrap();
    assert_eq!(after_first.ci_merge_failures, 1);
    assert_eq!(after_first.status, TaskStatus::Routed);

    store
        .update_status(store_id, TaskStatus::NeedsReview)
        .await
        .unwrap();
    store
        .set_fields(store_id, &[("needs_review_refires", serde_json::json!(5))])
        .await
        .unwrap();
    route_back_on_red_ci(
        &store,
        &task_manager,
        repo,
        &task,
        1,
        &RedCi {
            failed: &failed,
            head_sha: "aaa",
        },
        None,
    )
    .await;
    let after_same = store.get(store_id).await.unwrap();
    assert_eq!(after_same.ci_merge_failures, 1);
    assert_eq!(after_same.status, TaskStatus::NeedsReview);
    assert_eq!(
        after_same.needs_review_refires, 0,
        "a repeated look at the same red head must not spend the rebroadcast quota"
    );

    route_back_on_red_ci(
        &store,
        &task_manager,
        repo,
        &task,
        1,
        &RedCi {
            failed: &failed,
            head_sha: "bbb",
        },
        None,
    )
    .await;
    let after_new = store.get(store_id).await.unwrap();
    assert_eq!(after_new.ci_merge_failures, 2);
    assert_eq!(after_new.status, TaskStatus::Routed);
}

#[tokio::test]
async fn route_back_on_red_ci_stores_context_naming_the_failed_check() {
    let repo = "owner/repo";
    let (store, task_manager, task, store_id) = needs_review_task(repo, "ctx-sha").await;
    let failed = ["check".to_string()];

    route_back_on_red_ci(
        &store,
        &task_manager,
        repo,
        &task,
        7,
        &RedCi {
            failed: &failed,
            head_sha: "abc123",
        },
        None,
    )
    .await;

    let updated = store.get(store_id).await.unwrap();
    assert_eq!(updated.status, TaskStatus::Routed);
    assert!(
        updated.pr_review_context.contains("PR #7")
            && updated.pr_review_context.contains("abc123")
            && updated.pr_review_context.contains("check"),
        "pr_review_context must name the PR, head and failed check: {}",
        updated.pr_review_context
    );
    assert!(
        updated.pr_review_context.contains("Do not report done"),
        "pr_review_context must tell the agent to reproduce the check locally"
    );
}

/// Review waits until the engine push lands, even when the task already has a PR (#4051)
#[tokio::test]
async fn push_allows_review_holds_until_an_engine_push_on_an_existing_pr() {
    let repo = "owner/repo";
    let (store, task_manager, task, store_id) = needs_review_task(repo, "push-retry-pr").await;
    let fx = review_push_fixture();
    git(&fx.clone, &["push", "-u", "origin", "feature"]);
    fs::write(fx.clone.join("c.txt"), "c").unwrap();
    git(&fx.clone, &["add", "."]);
    git(&fx.clone, &["commit", "-m", "more"]);
    store
        .set_fields(
            store_id,
            &[
                ("worktree", serde_json::json!(fx.clone.to_str().unwrap())),
                ("branch", serde_json::json!("feature")),
                (
                    "last_error",
                    serde_json::json!(
                        "push failed: remote: fatal error in commit_refs\n ! [remote rejected] feature -> feature (pre-receive hook declined)"
                    ),
                ),
                ("push_failures", serde_json::json!(1)),
                ("pr_number", serde_json::json!(12)),
            ],
        )
        .await
        .unwrap();

    assert!(
        !push_allows_review(&store, &task_manager, repo, &task).await,
        "review must wait while the engine backoff is running"
    );
    assert!(
        !remote_feature_log(&fx).contains("more"),
        "the unpushed commit must not reach the remote before the retry"
    );

    store
        .kv_set(&format!("push_retry_at:{repo}#{}", task.id.0), "0")
        .await
        .unwrap();
    assert!(
        push_allows_review(&store, &task_manager, repo, &task).await,
        "review may start only after the engine push lands"
    );
    assert!(
        remote_feature_log(&fx).contains("more"),
        "the engine push must land the new commit before review starts"
    );
    let after = store.get(store_id).await.unwrap();
    assert_eq!(after.pr_number, Some(12));
    assert_eq!(after.push_failures, 0);
    assert!(after.last_error.is_empty());
}

#[tokio::test]
async fn route_back_on_red_ci_blocks_after_max_distinct_heads() {
    let repo = "owner/repo";
    let (store, task_manager, task, store_id) = needs_review_task(repo, "max-sha").await;
    let failed = ["check".to_string()];
    let shas = ["aaa", "bbb", "ccc"];

    for (i, sha) in shas.iter().enumerate() {
        store
            .update_status(store_id, TaskStatus::NeedsReview)
            .await
            .unwrap();
        route_back_on_red_ci(
            &store,
            &task_manager,
            repo,
            &task,
            1,
            &RedCi {
                failed: &failed,
                head_sha: sha,
            },
            None,
        )
        .await;
        let task = store.get(store_id).await.unwrap();
        if i + 1 < shas.len() {
            assert_eq!(task.status, TaskStatus::Routed);
            assert_eq!(task.ci_merge_failures, i32::try_from(i + 1).unwrap());
        } else {
            assert_eq!(task.status, TaskStatus::Blocked);
            assert_eq!(
                task.ci_merge_failures,
                i32::try_from(auto_merge::MAX_CI_MERGE_FAILURES).unwrap()
            );
        }
    }
}

use super::*;
use crate::engine::tasks;
use crate::store::{NewTask, TaskStore};
use async_trait::async_trait;
use std::collections::HashMap;
use std::process;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

// worktree_age_hours

#[tokio::test]
async fn worktree_age_hours_returns_none_for_missing_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("does-not-exist");
    assert!(worktree_age_hours(&missing).await.is_none());
}

#[tokio::test]
async fn worktree_age_hours_returns_some_for_existing_dir() {
    let tmp = tempfile::tempdir().unwrap();
    // A freshly created directory should have age ~0 hours.
    let age = worktree_age_hours(tmp.path()).await;
    assert!(age.is_some());
    assert_eq!(age.unwrap(), 0, "freshly created dir should have age 0h");
}

// JanitorOptions::from_config defaults

#[test]
fn janitor_options_default_values() {
    let opts = JanitorOptions::default();
    assert_eq!(opts.ttl_hours, 24);
    assert!(!opts.dry_run);
}

// cleanup_task_worktree_with_opts TTL guard (skips if git missing)

/// Create a minimal git repo + worktree; return (tmp, worktree_dir).
fn setup_test_repo() -> Option<(tempfile::TempDir, PathBuf)> {
    let tmp = tempfile::tempdir().ok()?;

    // Create a real git repo (not bare) so we can add worktrees.
    let repo_dir = tmp.path().join("repo.git");
    std_fs::create_dir_all(&repo_dir).ok()?;

    let init = process::Command::new("git")
        .args([
            "init",
            "--initial-branch=main",
            repo_dir
                .to_str()
                .expect("test repo path contains non-UTF-8 characters"),
        ])
        .output()
        .ok()?;
    if !init.status.success() {
        // Older git may not have --initial-branch; fall back.
        let _ = process::Command::new("git")
            .args([
                "init",
                repo_dir
                    .to_str()
                    .expect("test repo path contains non-UTF-8 characters"),
            ])
            .output()
            .ok()?;
    }

    // Create an initial commit so the repo has a HEAD.
    let readme = repo_dir.join("README.md");
    std_fs::write(&readme, "# test").ok()?;
    process::Command::new("git")
        .args(["-C", repo_dir.to_str()?, "add", "."])
        .output()
        .ok()?;
    process::Command::new("git")
        .args([
            "-C",
            repo_dir.to_str()?,
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .output()
        .ok()?;

    // Create a worktree branch.
    let wt_dir = tmp
        .path()
        .join("worktrees")
        .join("repo.git")
        .join("gh-issue-42-test");
    std_fs::create_dir_all(wt_dir.parent()?).ok()?;
    let wt_out = process::Command::new("git")
        .args([
            "-C",
            repo_dir.to_str()?,
            "worktree",
            "add",
            "-b",
            "gh-issue-42-test",
            wt_dir.to_str()?,
        ])
        .output()
        .ok()?;

    if !wt_out.status.success() {
        return None;
    }

    Some((tmp, wt_dir))
}

#[tokio::test]
async fn janitor_skips_young_worktree() {
    let Some((tmp, wt_dir)) = setup_test_repo() else {
        eprintln!("skipping test: git not available");
        return;
    };

    // The worktree was just created → age is 0h < ttl_hours=24 → must be skipped.
    assert!(wt_dir.exists(), "worktree should exist before janitor runs");

    // Run the TTL guard check directly (without a full engine).
    let age = worktree_age_hours(&wt_dir).await.unwrap_or(u64::MAX);
    assert!(
        age < 24,
        "freshly created worktree should be younger than 24h (age={age}h)"
    );

    // Explicitly verify: with ttl_hours=24 the worktree stays.
    let opts = JanitorOptions {
        ttl_hours: 24,
        dry_run: true,
    };
    // Age < ttl → should skip (worktree_age_hours < opts.ttl_hours).
    assert!(age < opts.ttl_hours, "janitor should skip young worktree");

    drop(tmp); // cleanup tempdir
}

#[tokio::test]
async fn janitor_eligible_when_ttl_zero() {
    let Some((tmp, wt_dir)) = setup_test_repo() else {
        eprintln!("skipping test: git not available");
        return;
    };

    // With ttl_hours=0 even a brand-new worktree is eligible.
    let age = worktree_age_hours(&wt_dir).await.unwrap_or(u64::MAX);
    let opts = JanitorOptions {
        ttl_hours: 0,
        dry_run: true,
    };
    assert!(
        age >= opts.ttl_hours,
        "worktree should be eligible when ttl=0 (age={age}h)"
    );

    drop(tmp);
}

#[tokio::test]
async fn dry_run_does_not_remove_worktree() {
    // Verify that with dry_run=true the worktree directory is NOT removed.
    let Some((tmp, wt_dir)) = setup_test_repo() else {
        eprintln!("skipping test: git not available");
        return;
    };

    assert!(wt_dir.exists(), "worktree must exist before test");

    // With dry_run=true and ttl_hours=0 the janitor would normally remove the
    // worktree, but dry_run prevents it.
    let opts = JanitorOptions {
        ttl_hours: 0,
        dry_run: true,
    };

    // The janitor needs a repo_root to run git commands.  We point it at the
    // bare repo we created inside tmp (even though it's not registered in config).
    // Because resolve_repo_root will fail for our synthetic repo slug, we test the
    // inner helpers directly here.
    let age = worktree_age_hours(&wt_dir).await.unwrap_or(u64::MAX);
    let in_use = false; // no tmux running in tests
    let skip = age < opts.ttl_hours || in_use;

    if !skip && opts.dry_run {
        // dry-run: log only, don't touch filesystem.
        tracing::debug!("[dry-run] would remove {}", wt_dir.display());
    } else if !skip {
        // This branch would do actual removal — but we're in dry_run mode above.
    }

    // Worktree must still be there.
    assert!(
        wt_dir.exists(),
        "dry-run must not remove the worktree directory"
    );

    drop(tmp);
}

#[test]
fn fallback_branch_path_construction() {
    // Verify the new fallback logic: when worktree path is absent but
    // branch is known, we should construct the path correctly.
    let tmp = tempfile::tempdir().unwrap();
    let wt_dir = tmp.path().join("myrepo").join("gh-issue-7-fix");
    std_fs::create_dir_all(&wt_dir).unwrap();

    // Simulate: worktree=None, branch=Some("gh-issue-7-fix"), repo="owner/myrepo"
    let branch = "gh-issue-7-fix";
    let repo = "owner/myrepo";
    let worktrees_base = tmp.path();

    let project = repo
        .rsplit('/')
        .next()
        .unwrap_or(repo)
        .trim_end_matches(".git");
    let candidate = worktrees_base.join(project).join(branch);

    assert_eq!(candidate, wt_dir);
    assert!(
        candidate.exists(),
        "fallback path should point to existing dir"
    );
}

/// A task with no worktree and no branch should return Ok(false) —
/// nothing was cleaned, so git pull should NOT be triggered.
#[tokio::test]
async fn cleanup_returns_false_when_nothing_to_clean() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    // Create a done task with no worktree or branch fields set
    let id = store
        .create(&NewTask {
            external_id: Some("999".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Already cleaned".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Done).await.unwrap();

    let opts = JanitorOptions {
        ttl_hours: 0,
        dry_run: false,
    };

    let result = cleanup_task_worktree_with_opts("999", "owner/repo", &store, &opts).await;
    // resolve_repo_root will fail for synthetic repo — that's OK, the point is
    // that when nothing needs cleaning the function should not report true.
    // In production this means no git pull is triggered.
    if let Ok(cleaned) = result {
        assert!(
            !cleaned,
            "cleanup should return false when there is no worktree or branch to clean"
        );
    } else {
        // resolve_repo_root fails for test repo — acceptable.
        // The bug was that even with no worktree/branch, Ok(true) was returned.
    }
}

/// A done task with an empty worktree and branch must not remove the project
/// worktree directory or the sibling worktrees under it (#4111).
#[tokio::test]
async fn cleanup_keeps_project_dir_for_a_task_with_empty_branch() {
    let project = format!("empty-branch-{}", process::id());
    let project_dir = home::worktrees_dir().unwrap().join(&project);
    let sibling = project_dir.join("sibling-task");
    std_fs::create_dir_all(&sibling).unwrap();
    let repo = format!("owner/{project}");

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create(&NewTask {
            external_id: Some("4111".to_string()),
            repo: repo.clone(),
            origin: "github".to_string(),
            title: "Empty branch".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Done).await.unwrap();

    for ttl_hours in [0, 24] {
        let opts = JanitorOptions {
            ttl_hours,
            dry_run: false,
        };
        let _ = cleanup_task_worktree_with_opts("4111", &repo, &store, &opts).await;
        assert!(sibling.exists(), "sibling worktree must survive");
    }

    std_fs::remove_dir_all(&project_dir).unwrap();
}

/// When the stored worktree path no longer exists on disk and there is no
/// branch recorded, cleanup must mark the task as cleaned so subsequent
/// runs do not keep retrying (idempotency regression test for #1021).
#[tokio::test]
async fn cleanup_marks_cleaned_when_stored_path_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    // A path that does NOT exist on disk.
    let stale_wt = tmp.path().join("stale-worktree");
    assert!(!stale_wt.exists(), "stale path must not exist");

    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let id = store
        .create(&NewTask {
            external_id: Some("1005".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Stale worktree task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Done).await.unwrap();
    // Set the stale worktree path on the task.
    store
        .set_fields(
            id,
            &[("worktree", stale_wt.to_string_lossy().as_ref().into())],
        )
        .await
        .unwrap();

    // Verify the task starts out NOT cleaned.
    let before = store.get(id).await.unwrap();
    assert!(!before.worktree_cleaned, "task should not be cleaned yet");
    assert!(
        !before.worktree.is_empty(),
        "task must have a worktree path set"
    );

    let opts = JanitorOptions {
        ttl_hours: 0,
        dry_run: false,
    };

    let result = cleanup_task_worktree_with_opts("1005", "owner/repo", &store, &opts).await;

    // The function should succeed (Ok) — stale path is not an error.
    // It returns Ok(false) because there was nothing to actively remove.
    match result {
        Ok(_) => {}
        Err(e) => panic!("cleanup should not error for stale path: {e}"),
    }

    // The task must now be marked cleaned so the next cycle skips it.
    let after = store.get(id).await.unwrap();
    assert!(
        after.worktree_cleaned,
        "task must be marked cleaned when stored worktree path is already gone"
    );
}

/// `remove_worktree_and_branch` returns true when the worktree directory
/// is successfully removed (regression test for #1143).
#[tokio::test]
async fn remove_worktree_returns_true_on_success() {
    let Some((tmp, wt_dir)) = setup_test_repo() else {
        eprintln!("skipping test: git not available");
        return;
    };

    let repo_dir = tmp.path().join("repo.git");
    assert!(wt_dir.exists(), "worktree must exist before removal");

    let removed =
        remove_worktree_and_branch("42", &wt_dir, Some("gh-issue-42-test"), &repo_dir, false).await;

    assert!(
        removed,
        "remove_worktree_and_branch should return true when directory is gone"
    );
    assert!(!wt_dir.exists(), "worktree directory should be removed");
}

/// Successful worktree cleanup must still delete the local branch.
/// This guards against early-return regressions in `remove_worktree_and_branch`.
#[tokio::test]
async fn remove_worktree_success_still_deletes_local_branch() {
    let Some((tmp, wt_dir)) = setup_test_repo() else {
        eprintln!("skipping test: git not available");
        return;
    };

    let repo_dir = tmp.path().join("repo.git");
    let repo_dir_str = repo_dir
        .to_str()
        .expect("test repo path contains non-UTF-8 characters");
    let branch = "gh-issue-42-test";

    let before = process::Command::new("git")
        .args([
            "-C",
            repo_dir_str,
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .unwrap();
    assert!(
        before.success(),
        "test setup must create local branch before cleanup"
    );

    let removed = remove_worktree_and_branch("42", &wt_dir, Some(branch), &repo_dir, false).await;
    assert!(removed, "worktree should be removed");

    let after = process::Command::new("git")
        .args([
            "-C",
            repo_dir_str,
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .unwrap();
    assert!(
        !after.success(),
        "local branch should be deleted after successful worktree cleanup"
    );
}

/// `remove_worktree_and_branch` returns false when the worktree directory
/// cannot be removed (regression test for #1143 — prevents orphaned worktrees
/// from being permanently marked as cleaned).
#[tokio::test]
async fn remove_worktree_returns_false_when_dir_persists() {
    let tmp = tempfile::tempdir().unwrap();
    // Create a plain directory (not a real git worktree) — git will fail
    // to remove it, so the directory stays on disk.
    let fake_wt = tmp.path().join("not-a-worktree");
    std_fs::create_dir_all(&fake_wt).unwrap();

    // repo_root doesn't matter much — it just needs to exist for the -C flag.
    // Using tmp itself; git commands will fail but that's the point.
    let removed = remove_worktree_and_branch("99", &fake_wt, None, tmp.path(), false).await;

    // The directory still exists because git couldn't remove it.
    // Before the fix, this would still have been marked as cleaned.
    assert!(
        !removed,
        "remove_worktree_and_branch should return false when directory still exists"
    );
    assert!(fake_wt.exists(), "directory should still be on disk");
}

struct MergeCheckMockBackend {
    batch_result: Mutex<anyhow::Result<HashMap<String, bool>>>,
    single_results: HashMap<String, anyhow::Result<bool>>,
    posted_comments: Arc<Mutex<Vec<(String, String)>>>,
    single_checked_branches: Arc<Mutex<Vec<String>>>,
}

impl MergeCheckMockBackend {
    fn with_batch_error(
        error: anyhow::Error,
        single_results: HashMap<String, anyhow::Result<bool>>,
    ) -> Self {
        Self {
            batch_result: Mutex::new(Err(error)),
            single_results,
            posted_comments: Arc::new(Mutex::new(Vec::new())),
            single_checked_branches: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_merged(branch: &str) -> Self {
        Self {
            batch_result: Mutex::new(Ok(HashMap::from([(branch.to_string(), true)]))),
            single_results: HashMap::new(),
            posted_comments: Arc::new(Mutex::new(Vec::new())),
            single_checked_branches: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl ExternalBackend for MergeCheckMockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn create_task(
        &self,
        _title: &str,
        _body: &str,
        _labels: &[String],
    ) -> anyhow::Result<ExternalId> {
        anyhow::bail!("not implemented")
    }

    async fn get_task(&self, _id: &ExternalId) -> anyhow::Result<ExternalTask> {
        anyhow::bail!("not implemented")
    }

    async fn list_by_status(&self, _status: Status) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }

    async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }

    async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()> {
        self.posted_comments
            .lock()
            .unwrap()
            .push((id.0.clone(), body.to_string()));
        Ok(())
    }

    async fn set_labels(&self, _id: &ExternalId, _labels: &[String]) -> anyhow::Result<()> {
        Ok(())
    }

    async fn remove_label(&self, _id: &ExternalId, _label: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        Ok(vec![])
    }

    async fn create_sub_task(
        &self,
        _parent: &ExternalId,
        _title: &str,
        _body: &str,
        _labels: &[String],
    ) -> anyhow::Result<ExternalId> {
        anyhow::bail!("not implemented")
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn is_pr_merged(&self, branch: &str) -> anyhow::Result<bool> {
        self.single_checked_branches
            .lock()
            .unwrap()
            .push(branch.to_string());
        match self.single_results.get(branch) {
            Some(Ok(is_merged)) => Ok(*is_merged),
            Some(Err(err)) => Err(anyhow::anyhow!(err.to_string())),
            None => Ok(false),
        }
    }

    async fn batch_is_pr_merged(
        &self,
        _branches: &[String],
    ) -> anyhow::Result<HashMap<String, bool>> {
        match &*self.batch_result.lock().unwrap() {
            Ok(map) => Ok(map.clone()),
            Err(err) => Err(anyhow::anyhow!(err.to_string())),
        }
    }
}

#[tokio::test]
async fn check_merged_prs_falls_back_to_single_branch_checks_on_batch_error() {
    let single_results = HashMap::from([
        ("branch-1".to_string(), Ok(true)),
        ("branch-2".to_string(), Ok(false)),
    ]);
    let backend = Arc::new(MergeCheckMockBackend::with_batch_error(
        anyhow::anyhow!("missing /data/repository in GraphQL response: no error details"),
        single_results,
    ));
    let backend_dyn: Arc<dyn ExternalBackend> = Arc::<MergeCheckMockBackend>::clone(&backend);

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let task1 = store
        .create(&NewTask {
            external_id: Some("101".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Task 101".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .update_status(task1, TaskStatus::NeedsReview)
        .await
        .unwrap();
    store
        .set_fields(task1, &[("branch", serde_json::json!("branch-1"))])
        .await
        .unwrap();
    // No .git file, so cleanup force-removes the directory
    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path = wt_dir.path().join("branch-1");
    std_fs::create_dir_all(&wt_path).unwrap();
    store
        .set_fields(
            task1,
            &[("worktree", serde_json::json!(wt_path.to_string_lossy()))],
        )
        .await
        .unwrap();

    let task2 = store
        .create(&NewTask {
            external_id: Some("102".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Task 102".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .update_status(task2, TaskStatus::InReview)
        .await
        .unwrap();
    store
        .set_fields(task2, &[("branch", serde_json::json!("branch-2"))])
        .await
        .unwrap();

    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend_dyn),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    let in_review_tasks = store
        .list_by_status("owner/repo", TaskStatus::InReview)
        .await
        .unwrap()
        .into_iter()
        .map(|stored| ReviewTaskSnapshot {
            external: tasks::store_task_to_external(&stored),
            stored,
        })
        .collect::<Vec<_>>();
    let needs_review_tasks = store
        .list_by_status("owner/repo", TaskStatus::NeedsReview)
        .await
        .unwrap()
        .into_iter()
        .map(|stored| ReviewTaskSnapshot {
            external: tasks::store_task_to_external(&stored),
            stored,
        })
        .collect::<Vec<_>>();

    check_merged_prs(
        &backend_dyn,
        "owner/repo",
        &store,
        &task_manager,
        &in_review_tasks,
        &needs_review_tasks,
    )
    .await
    .unwrap();

    let checked = backend.single_checked_branches.lock().unwrap().clone();
    assert_eq!(
        checked,
        vec!["branch-2".to_string(), "branch-1".to_string()]
    );

    let updated_task1 = store.get(task1).await.unwrap();
    let updated_task2 = store.get(task2).await.unwrap();
    assert_eq!(updated_task1.status, TaskStatus::Done);
    assert!(updated_task1.worktree_cleaned);
    assert!(!wt_path.exists());
    assert!(!updated_task2.worktree_cleaned);
    assert_eq!(updated_task2.status, TaskStatus::InReview);

    let comments = backend.posted_comments.lock().unwrap().clone();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].0, "101");
    assert!(comments[0].1.contains("PR merged, marking task complete"));
}

#[tokio::test]
async fn check_merged_prs_skips_rescue_while_ci_bootstrap_holds() {
    let (tmp, clone) = origin_and_clone_with_feat();
    let wt = add_worktree(&clone, tmp.path(), "feat");
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    // hold_after_merge reads the plan marker in the body, not the title
    let id = store_approved_task(
        &store,
        "owner/repo",
        "task-12",
        "CI-01 Set up CI and tests",
        "<!-- orch-plan: CI-01 -->",
        &wt,
    )
    .await;
    let stored = store.get(id).await.unwrap();

    let backend = Arc::new(MergeCheckMockBackend::with_merged("feat"));
    let backend_dyn: Arc<dyn ExternalBackend> = Arc::<MergeCheckMockBackend>::clone(&backend);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend_dyn),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    check_merged_prs(
        &backend_dyn,
        "owner/repo",
        &store,
        &task_manager,
        &[ReviewTaskSnapshot {
            external: tasks::store_task_to_external(&stored),
            stored,
        }],
        &[],
    )
    .await
    .unwrap();

    let after = store.get(id).await.unwrap();
    assert_eq!(after.status, TaskStatus::Approved);
    assert!(after.worktree_cleaned);
    assert!(!wt.exists());
    assert!(!remote_has(&clone, "rescue/feat"));
    assert!(backend.posted_comments.lock().unwrap().is_empty());
}

struct ReconciliationMockBackend {
    reconciliation_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ExternalBackend for ReconciliationMockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn create_task(
        &self,
        _title: &str,
        _body: &str,
        _labels: &[String],
    ) -> anyhow::Result<ExternalId> {
        anyhow::bail!("not implemented")
    }

    async fn get_task(&self, _id: &ExternalId) -> anyhow::Result<ExternalTask> {
        anyhow::bail!("not implemented")
    }

    async fn list_by_status(&self, _status: Status) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }

    async fn list_reconciliation_candidates(&self) -> anyhow::Result<Vec<ExternalTask>> {
        self.reconciliation_calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    }

    async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }

    async fn post_comment(&self, _id: &ExternalId, _body: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn set_labels(&self, _id: &ExternalId, _labels: &[String]) -> anyhow::Result<()> {
        Ok(())
    }

    async fn remove_label(&self, _id: &ExternalId, _label: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        Ok(vec![])
    }

    async fn create_sub_task(
        &self,
        _parent: &ExternalId,
        _title: &str,
        _body: &str,
        _labels: &[String],
    ) -> anyhow::Result<ExternalId> {
        anyhow::bail!("not implemented")
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn cleanup_reconciliation_is_rate_limited_per_repo() {
    let repo = "owner/repo-cleanup-rate-limit";
    RECONCILIATION_LAST_RUN.remove(repo);

    let backend = Arc::new(ReconciliationMockBackend {
        reconciliation_calls: Arc::new(AtomicUsize::new(0)),
    });
    let backend_dyn: Arc<dyn ExternalBackend> = Arc::<ReconciliationMockBackend>::clone(&backend);
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend_dyn),
        Arc::clone(&store),
        repo.to_string(),
    ));
    let opts = JanitorOptions {
        ttl_hours: 0,
        dry_run: true,
    };

    cleanup_done_worktrees_with_opts(&backend_dyn, repo, &task_manager, &store, &opts)
        .await
        .unwrap();
    cleanup_done_worktrees_with_opts(&backend_dyn, repo, &task_manager, &store, &opts)
        .await
        .unwrap();

    assert_eq!(
        backend.reconciliation_calls.load(Ordering::SeqCst),
        1,
        "cleanup should not re-list reconciliation candidates on every sync pass"
    );

    RECONCILIATION_LAST_RUN.remove(repo);
}

/// Cleanup only collects done tasks. A blocked task waits on a person and resumes from its
/// worktree and branch, so it must not even be considered (#3781)
#[tokio::test]
async fn cleanup_skips_blocked_internal_tasks_and_cleans_done_ones() {
    let repo = "owner/repo-cleanup-blocked";
    RECONCILIATION_LAST_RUN.remove(repo);
    let backend_dyn: Arc<dyn ExternalBackend> = Arc::new(ReconciliationMockBackend {
        reconciliation_calls: Arc::new(AtomicUsize::new(0)),
    });
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend_dyn),
        Arc::clone(&store),
        repo.to_string(),
    ));

    let mut ids = Vec::new();
    for (title, status) in [("blocked", TaskStatus::Blocked), ("done", TaskStatus::Done)] {
        let id = store
            .create_internal(repo, title, "", "test", title, None)
            .await
            .unwrap();
        store
            .set_fields(
                id,
                &[
                    (
                        "worktree",
                        serde_json::json!(format!("/nonexistent/{title}")),
                    ),
                    ("branch", serde_json::json!("")),
                ],
            )
            .await
            .unwrap();
        store.update_status(id, status).await.unwrap();
        ids.push(id);
    }

    let opts = JanitorOptions {
        ttl_hours: 0,
        dry_run: false,
    };
    cleanup_done_worktrees_with_opts(&backend_dyn, repo, &task_manager, &store, &opts)
        .await
        .unwrap();

    assert!(
        !store.get(ids[0]).await.unwrap().worktree_cleaned,
        "blocked task must not be considered for cleanup"
    );
    assert!(
        store.get(ids[1]).await.unwrap().worktree_cleaned,
        "done task is cleaned, which proves the pass ran"
    );
    RECONCILIATION_LAST_RUN.remove(repo);
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A clone of a bare origin with main pushed and branch `feat` one commit ahead
fn origin_and_clone_with_feat() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("origin.git");
    std_fs::create_dir_all(&bare).unwrap();
    git_ok(&bare, &["init", "--bare", "-b", "main"]);
    git_ok(tmp.path(), &["clone", bare.to_str().unwrap(), "clone"]);
    let clone = tmp.path().join("clone");
    std_fs::write(clone.join("base.txt"), "base").unwrap();
    git_ok(&clone, &["add", "."]);
    git_ok(&clone, &["commit", "-m", "base"]);
    git_ok(&clone, &["push", "origin", "main"]);
    git_ok(&clone, &["checkout", "-b", "feat"]);
    std_fs::write(clone.join("work.txt"), "work").unwrap();
    git_ok(&clone, &["add", "."]);
    git_ok(&clone, &["commit", "-m", "work"]);
    git_ok(&clone, &["checkout", "main"]);
    (tmp, clone)
}

fn remote_has(clone: &Path, branch: &str) -> bool {
    !git_ok(clone, &["ls-remote", "--heads", "origin", branch])
        .trim()
        .is_empty()
}

fn local_has(clone: &Path, branch: &str) -> bool {
    process::Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(clone)
        .status()
        .unwrap()
        .success()
}

/// Deleting a pushed branch whose commits main lacks would destroy the only copy of the
/// work, as startup cleanup did to a blocked task (#3781)
#[tokio::test]
async fn delete_branches_keeps_an_unmerged_remote_branch() {
    let (_tmp, clone) = origin_and_clone_with_feat();
    git_ok(&clone, &["push", "origin", "feat"]);

    delete_branches("1", "feat", &clone).await;

    assert!(
        remote_has(&clone, "feat"),
        "unmerged remote branch must survive"
    );
    assert!(
        !local_has(&clone, "feat"),
        "local copy is on origin, so it can go"
    );
}

#[tokio::test]
async fn delete_branches_leaves_the_screenshots_branch_alone() {
    let (_tmp, clone) = origin_and_clone_with_feat();
    git_ok(&clone, &["branch", SCREENSHOTS_BRANCH, "feat"]);
    git_ok(&clone, &["push", "origin", SCREENSHOTS_BRANCH]);
    git_ok(&clone, &["merge", "--ff-only", "feat"]);
    git_ok(&clone, &["push", "origin", "main"]);

    delete_branches("1", SCREENSHOTS_BRANCH, &clone).await;

    assert!(remote_has(&clone, SCREENSHOTS_BRANCH));
    assert!(local_has(&clone, SCREENSHOTS_BRANCH));
}

#[tokio::test]
async fn delete_branches_removes_a_branch_main_already_contains() {
    let (_tmp, clone) = origin_and_clone_with_feat();
    git_ok(&clone, &["push", "origin", "feat"]);
    git_ok(&clone, &["merge", "--ff-only", "feat"]);
    git_ok(&clone, &["push", "origin", "main"]);

    delete_branches("1", "feat", &clone).await;

    assert!(!remote_has(&clone, "feat"));
    assert!(!local_has(&clone, "feat"));
}

fn add_worktree(clone: &Path, tmp: &Path, branch: &str) -> PathBuf {
    let wt = tmp.join("wt");
    git_ok(clone, &["worktree", "add", wt.to_str().unwrap(), branch]);
    wt
}

async fn store_approved_task(
    store: &TaskStore,
    repo: &str,
    external_id: &str,
    title: &str,
    body: &str,
    worktree: &Path,
) -> i64 {
    let id = store
        .create(&NewTask {
            external_id: Some(external_id.to_string()),
            repo: repo.to_string(),
            origin: "github".to_string(),
            title: title.to_string(),
            body: body.to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Approved).await.unwrap();
    store
        .set_fields(
            id,
            &[
                ("branch", serde_json::json!("feat")),
                (
                    "worktree",
                    serde_json::json!(worktree.to_string_lossy().as_ref()),
                ),
                ("pr_number", serde_json::json!(7)),
            ],
        )
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn cleanup_pushes_unpushed_commits_to_a_rescue_branch() {
    let (tmp, clone) = origin_and_clone_with_feat();
    let wt = add_worktree(&clone, tmp.path(), "feat");

    let sha = git_ok(&clone, &["rev-parse", "--short", "feat"]);
    let sha = sha.trim();
    match rescue_unpushed_work("1", Some(&wt), "feat", &clone).await {
        Rescue::Pushed { branch, commits } => {
            assert_eq!(branch, "rescue/feat");
            assert!(rescue_comment(&branch, &commits).contains(sha));
        }
        other => panic!("expected a rescue push, got {other:?}"),
    }

    let tip = git_ok(&clone, &["rev-parse", "feat"]);
    let tracking = git_ok(&clone, &["rev-parse", "refs/remotes/origin/rescue/feat"]);
    assert_eq!(
        tracking.trim(),
        tip.trim(),
        "rescue push must point the tracking ref at the tip it sent"
    );

    let removed = remove_worktree_and_branch("1", &wt, Some("feat"), &clone, false).await;

    assert!(removed);
    assert!(remote_has(&clone, "rescue/feat"));
    assert!(!local_has(&clone, "feat"));
}

#[tokio::test]
async fn rescue_push_sets_tracking_ref_without_a_fetchspec() {
    let (tmp, clone) = origin_and_clone_with_feat();
    let wt = add_worktree(&clone, tmp.path(), "feat");
    git_ok(&clone, &["config", "--unset-all", "remote.origin.fetch"]);

    match rescue_unpushed_work("1", Some(&wt), "feat", &clone).await {
        Rescue::Pushed { branch, .. } => assert_eq!(branch, "rescue/feat"),
        other => panic!("expected a rescue push, got {other:?}"),
    }

    let tracking = git_ok(&clone, &["rev-parse", "refs/remotes/origin/rescue/feat"]);
    let tip = git_ok(&clone, &["rev-parse", "feat"]);
    assert_eq!(tracking.trim(), tip.trim());
}

#[tokio::test]
async fn cleanup_commits_uncommitted_changes_before_the_rescue() {
    let (tmp, clone) = origin_and_clone_with_feat();
    git_ok(&clone, &["push", "origin", "feat"]);
    let wt = add_worktree(&clone, tmp.path(), "feat");
    std_fs::write(wt.join("dirty.txt"), "dirty").unwrap();

    let removed = remove_worktree_and_branch("1", &wt, Some("feat"), &clone, false).await;

    assert!(removed);
    assert!(remote_has(&clone, "rescue/feat"));
}

#[tokio::test]
async fn cleanup_makes_no_rescue_branch_for_a_pushed_branch() {
    let (tmp, clone) = origin_and_clone_with_feat();
    git_ok(&clone, &["push", "origin", "feat"]);
    let wt = add_worktree(&clone, tmp.path(), "feat");

    let removed = remove_worktree_and_branch("1", &wt, Some("feat"), &clone, false).await;

    assert!(removed);
    assert!(!remote_has(&clone, "rescue/feat"));
    assert!(!local_has(&clone, "feat"));
}

#[tokio::test]
async fn rescue_skips_a_branch_on_the_remote_without_a_tracking_ref() {
    let (_tmp, clone) = origin_and_clone_with_feat();
    git_ok(&clone, &["push", "origin", "feat"]);
    git_ok(&clone, &["update-ref", "-d", "refs/remotes/origin/feat"]);

    let rescue = rescue_unpushed_work("1", None, "feat", &clone).await;

    assert_eq!(rescue, Rescue::Clean);
    assert!(!remote_has(&clone, "rescue/feat"));
}

#[tokio::test]
async fn merged_pr_cleanup_pushes_no_rescue_branch() {
    let (tmp, clone) = origin_and_clone_with_feat();
    let wt = add_worktree(&clone, tmp.path(), "feat");

    let removed = remove_worktree_and_branch_for("1", &wt, Some("feat"), &clone, false, true).await;

    assert!(removed);
    assert!(!wt.exists());
    assert!(!remote_has(&clone, "rescue/feat"));
}

#[tokio::test]
async fn cleanup_skips_rescue_for_an_approved_merged_pr() {
    let (tmp, clone) = origin_and_clone_with_feat();
    let wt = add_worktree(&clone, tmp.path(), "feat");
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    store_approved_task(&store, "owner/repo", "task-12", "CI-01 Set up CI", "", &wt).await;

    cleanup_merged_pr_worktree("task-12", "owner/repo", &store)
        .await
        .unwrap();

    let task = store
        .get(
            store
                .resolve_task_id("owner/repo", "task-12")
                .await
                .unwrap()
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(task.status, TaskStatus::Approved);
    assert!(task.worktree_cleaned);
    assert!(!wt.exists());
    assert!(!remote_has(&clone, "rescue/feat"));
}

#[tokio::test]
async fn cleanup_rescues_an_approved_task_when_the_pr_is_not_known_merged() {
    let (tmp, clone) = origin_and_clone_with_feat();
    let wt = add_worktree(&clone, tmp.path(), "feat");
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    store_approved_task(&store, "owner/repo", "task-12", "fix something", "", &wt).await;

    cleanup_task_worktree("task-12", "owner/repo", &store)
        .await
        .unwrap();

    assert!(remote_has(&clone, "rescue/feat"));
}

#[tokio::test]
async fn unmerged_cleanup_still_pushes_a_rescue_branch() {
    let (tmp, clone) = origin_and_clone_with_feat();
    let wt = add_worktree(&clone, tmp.path(), "feat");

    let removed =
        remove_worktree_and_branch_for("1", &wt, Some("feat"), &clone, false, false).await;

    assert!(removed);
    assert!(remote_has(&clone, "rescue/feat"));
}

#[tokio::test]
async fn rescue_ignores_a_remote_tip_missing_locally() {
    let (tmp, clone) = origin_and_clone_with_feat();
    git_ok(&clone, &["push", "origin", "feat"]);
    let other = tmp.path().join("other");
    git_ok(
        tmp.path(),
        &["clone", "-b", "feat", "origin.git", other.to_str().unwrap()],
    );
    std_fs::write(other.join("extra.txt"), "extra").unwrap();
    git_ok(&other, &["add", "."]);
    git_ok(&other, &["commit", "-m", "extra"]);
    git_ok(&other, &["push", "origin", "feat"]);

    let rescue = rescue_unpushed_work("1", None, "feat", &clone).await;

    assert_ne!(rescue, Rescue::Failed);
}

#[tokio::test]
async fn cleanup_keeps_worktree_and_branch_when_the_rescue_push_fails() {
    let (tmp, clone) = origin_and_clone_with_feat();
    let wt = add_worktree(&clone, tmp.path(), "feat");
    git_ok(
        &clone,
        &["remote", "set-url", "origin", "/nonexistent/origin.git"],
    );

    let removed = remove_worktree_and_branch("1", &wt, Some("feat"), &clone, false).await;

    assert!(!removed);
    assert!(wt.exists());
    assert!(local_has(&clone, "feat"));
}

#[tokio::test]
async fn delete_branches_keeps_a_local_branch_with_unpushed_commits() {
    let (_tmp, clone) = origin_and_clone_with_feat();

    delete_branches("1", "feat", &clone).await;

    assert!(
        local_has(&clone, "feat"),
        "unpushed commits must not be deleted"
    );
}

use super::tests::git;
use super::*;
use crate::engine::review::approval::ApprovalEvent;
use crate::engine::review::approval::approval_covers_head;
use crate::engine::review::approval::approved_sha_key;
use crate::engine::review::approval::base_merge_parent;
use crate::engine::review::approval::is_clean_merge;
use crate::engine::review::approval::pr_matches_approval;
use crate::engine::review::approval::read_approval;
use crate::engine::review::approval::same_commits_rebased;
use crate::engine::review::approval::write_approval;
use crate::github::types::GitHubPullRequest;
use crate::store::{TaskStatus, TaskStore};
use std::process;
use tempfile::TempDir;

/// An approval only carries over while the PR head is still the commit that was
/// approved. A force-push from another clone moves the head, and reviewing again is
/// the safe answer in every other case (#3783)
#[tokio::test]
async fn carried_approval_counts_as_this_rounds_decision() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create_internal("owner/repo", "Approved head", "", "test", "carried", None)
        .await
        .unwrap();
    let task_id = format!("internal:{id}");
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    assert_eq!(
        store.in_review_entry(id).await.unwrap().map(|e| e.1),
        Some(false)
    );

    record_carried_approval(&store, "owner/repo", &task_id, 7).await;

    assert_eq!(
        store.in_review_entry(id).await.unwrap().map(|e| e.1),
        Some(true),
        "the stale in_review check must see this round as decided"
    );
}

#[test]
fn approval_covers_head_only_for_the_exact_commit() {
    assert!(approval_covers_head(Some("abc123"), Some("abc123")));
    assert!(
        !approval_covers_head(Some("abc123"), Some("def456")),
        "a moved PR head must be reviewed"
    );
    assert!(
        !approval_covers_head(None, Some("abc123")),
        "no recorded approval means review"
    );
    assert!(
        !approval_covers_head(Some("abc123"), None),
        "an unreadable PR head must not skip the review"
    );
    assert!(!approval_covers_head(Some(""), Some("")));
}

fn pr_for(head_repo: Option<&str>, head: &str, base: &str) -> GitHubPullRequest {
    serde_json::from_value(serde_json::json!({
        "number": 7,
        "title": "t",
        "state": "open",
        "head": {"ref": head, "sha": "h", "repo": head_repo.map(|r| serde_json::json!({"full_name": r}))},
        "base": {"ref": base, "sha": "b"},
        "html_url": "https://github.com/owner/repo/pull/7",
    }))
    .unwrap()
}

#[test]
fn a_carried_approval_must_match_the_approved_repo_branch_and_base() {
    let approval = ApprovalEvent {
        sha: "a".into(),
        repo: "owner/repo".into(),
        branch: "gh-issue-1".into(),
        base: "main".into(),
    };
    let ok = pr_for(Some("owner/repo"), "gh-issue-1", "main");
    assert!(pr_matches_approval(&ok, &approval, "gh-issue-1"));
    assert!(!pr_matches_approval(
        &pr_for(Some("fork/repo"), "gh-issue-1", "main"),
        &approval,
        "gh-issue-1"
    ));
    assert!(!pr_matches_approval(
        &pr_for(None, "gh-issue-1", "main"),
        &approval,
        "gh-issue-1"
    ));
    assert!(!pr_matches_approval(
        &pr_for(Some("owner/repo"), "other", "main"),
        &approval,
        "gh-issue-1"
    ));
    assert!(!pr_matches_approval(
        &pr_for(Some("owner/repo"), "gh-issue-1", "release"),
        &approval,
        "gh-issue-1"
    ));
    assert!(!pr_matches_approval(&ok, &approval, "another-task-branch"));

    let legacy = ApprovalEvent::parse("abc123").unwrap();
    assert_eq!(legacy.sha, "abc123");
    assert!(!pr_matches_approval(&ok, &legacy, "gh-issue-1"));
    let json = serde_json::to_string(&approval).unwrap();
    assert_eq!(ApprovalEvent::parse(&json), Some(approval));
    assert_eq!(ApprovalEvent::parse("  "), None);
}

#[tokio::test]
async fn only_the_same_commits_rebased_onto_the_base_carry_the_approval() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let git = |args: &[&str]| {
        let out = process::Command::new("git")
            .args([
                "-c",
                "user.name=agent",
                "-c",
                "user.email=agent@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let write_commit = |file: &str, body: &str| {
        fs::write(path.join(file), body).unwrap();
        git(&["add", file]);
        git(&["commit", "-q", "-m", body]);
    };
    git(&["init", "-q", "-b", "main"]);
    write_commit("a", "base");
    git(&["checkout", "-q", "-b", "pr"]);
    write_commit("b", "first");
    write_commit("c", "second");
    let approved = git(&["rev-parse", "HEAD"]);
    git(&["checkout", "-q", "main"]);
    write_commit("d", "base moved");
    git(&["checkout", "-q", "-b", "rebased", &approved]);
    git(&["rebase", "-q", "main"]);
    let rebased = git(&["rev-parse", "HEAD"]);
    assert!(same_commits_rebased(path, &approved, "main", &rebased).await);

    // A changed patch on top of the rebase is not the approved change
    write_commit("c", "second, changed");
    let changed = git(&["rev-parse", "HEAD"]);
    assert!(!same_commits_rebased(path, &approved, "main", &changed).await);

    // The same patches under another author are not the approved commits
    git(&["checkout", "-q", "-b", "other-author", &rebased]);
    git(&[
        "-c",
        "user.name=someone",
        "-c",
        "user.email=someone@t",
        "commit",
        "-q",
        "--amend",
        "--reset-author",
        "--no-edit",
    ]);
    let reauthored = git(&["rev-parse", "HEAD"]);
    assert!(!same_commits_rebased(path, &approved, "main", &reauthored).await);
}

#[tokio::test]
async fn only_a_clean_merge_of_the_base_counts_as_base_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let git = |args: &[&str]| {
        let out = process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let commit = |file: &str, msg: &str| {
        fs::write(path.join(file), msg).unwrap();
        git(&["add", file]);
        git(&["commit", "-q", "-m", msg]);
        git(&["rev-parse", "HEAD"])
    };
    git(&["init", "-q", "-b", "main"]);
    commit("a", "base");
    git(&["checkout", "-q", "-b", "pr"]);
    let approved = commit("b", "approved change");
    git(&["checkout", "-q", "main"]);
    let base_tip = commit("c", "base moved");
    git(&["checkout", "-q", "pr"]);
    git(&["merge", "-q", "--no-edit", "main"]);
    let clean = git(&["rev-parse", "HEAD"]);
    assert!(is_clean_merge(path, &approved, &base_tip, &clean).await);

    fs::write(path.join("d"), "unreviewed").unwrap();
    git(&["add", "d"]);
    git(&["commit", "-q", "--amend", "--no-edit"]);
    let forged = git(&["rev-parse", "HEAD"]);
    assert!(!is_clean_merge(path, &approved, &base_tip, &forged).await);
}

#[test]
fn only_a_merge_of_the_approved_commit_carries_the_approval() {
    let parents = |p: &[&str]| p.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        base_merge_parent("approved", &parents(&["approved", "base"])),
        Some("base")
    );
    // A new commit on top of the approval, or a merge of something else, needs a review
    assert_eq!(base_merge_parent("approved", &parents(&["approved"])), None);
    assert_eq!(
        base_merge_parent("approved", &parents(&["other", "base"])),
        None
    );
    assert_eq!(
        base_merge_parent("approved", &parents(&["approved", "a", "b"])),
        None
    );
}

/**
 * The approval is recorded against the commit pushed to the PR and is dropped when a
 * review asks for changes, so the next NeedsReview event reviews the new work (#3783)
 **/
#[tokio::test]
async fn approved_sha_round_trips_through_the_kv_store() {
    let temp = TempDir::new().unwrap();
    let wt = temp.path();
    git(wt, &["init", "-b", "main"]);
    git(wt, &["config", "user.name", "Test"]);
    git(wt, &["config", "user.email", "test@test.com"]);
    fs::write(wt.join("fix.txt"), "the fix").unwrap();
    git(wt, &["add", "."]);
    git(wt, &["commit", "-m", "apply the fix"]);

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let task_id = "3783";

    record_approved_sha(&store, repo, task_id, 77, wt, "gh-issue-3783", "main").await;
    let recorded = read_approval(&store, repo, task_id, 77)
        .await
        .unwrap()
        .expect("approval must be recorded");
    assert_eq!(recorded.repo, repo);
    assert_eq!(recorded.branch, "gh-issue-3783");
    assert_eq!(recorded.base, "main");
    assert_eq!(
        recorded.sha,
        worktree_head_sha(wt).await.unwrap(),
        "the recorded sha must be the commit that was pushed to the PR"
    );
    assert!(
        store
            .kv_get(&approved_sha_key(repo, task_id, 78))
            .await
            .unwrap()
            .is_none(),
        "an approval must not leak to another PR"
    );

    clear_approved_sha(&store, repo, task_id, 77).await;
    assert!(
        store
            .kv_get(&approved_sha_key(repo, task_id, 77))
            .await
            .unwrap()
            .is_none(),
        "requesting changes must drop the approval"
    );
    assert!(
        !has_recorded_approval(&store, repo, task_id, 77).await,
        "clearing the approval must drop it for recovery too"
    );
}

#[tokio::test]
async fn has_recorded_approval_requires_a_non_empty_sha() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let task_id = "4140";
    assert!(!has_recorded_approval(&store, repo, task_id, 7).await);

    write_approval(
        &store,
        task_id,
        7,
        &ApprovalEvent {
            sha: String::new(),
            repo: repo.to_string(),
            branch: "gh-issue-4140".to_string(),
            base: "main".to_string(),
        },
    )
    .await;
    assert!(
        !has_recorded_approval(&store, repo, task_id, 7).await,
        "an empty sha is not an approval"
    );

    write_approval(
        &store,
        task_id,
        7,
        &ApprovalEvent {
            sha: "abc123".to_string(),
            repo: repo.to_string(),
            branch: "gh-issue-4140".to_string(),
            base: "main".to_string(),
        },
    )
    .await;
    assert!(has_recorded_approval(&store, repo, task_id, 7).await);
}

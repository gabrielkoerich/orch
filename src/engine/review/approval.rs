//! Review approvals: the decision, its record, carrying it to a new head, and posting its status

//! Approval records and carrying an approval to a new head

use crate::engine::git;
use crate::engine::review::mode;
use crate::engine::review::mode::ReviewMode;
use crate::github::app;
use crate::github::app::AppAuth;
use crate::github::app::CheckRunState;
use crate::github::http::BaseMergeRefusal;
use crate::github::types::ReviewIssue;

use crate::engine::git::worktree;
use crate::github::http::GhHttp;
use crate::github::types::GitHubPullRequest;
use crate::store::TaskStore;
use crate::store::opt_store_get_task;
use crate::store::store_log_activity;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time;

/// KV key holding the commit an approving review was already written for
pub(crate) fn approved_sha_key(repo: &str, task_id: &str, pr_number: u64) -> String {
    format!("review_approved_sha:{repo}#{task_id}#{pr_number}")
}

/// What an approving review approved: the commit and the PR it was on. A carried approval must
/// match every field
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ApprovalEvent {
    pub(super) sha: String,
    pub(super) repo: String,
    pub(super) branch: String,
    pub(super) base: String,
}

impl ApprovalEvent {
    /// A record from before events held only the sha, it approves that head and never carries
    pub(super) fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        serde_json::from_str(raw).ok().or_else(|| {
            Some(Self {
                sha: raw.to_string(),
                repo: String::new(),
                branch: String::new(),
                base: String::new(),
            })
        })
    }
}

pub(crate) async fn read_approval(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    pr_number: u64,
) -> anyhow::Result<Option<ApprovalEvent>> {
    Ok(store
        .kv_get(&approved_sha_key(repo, task_id, pr_number))
        .await?
        .as_deref()
        .and_then(ApprovalEvent::parse))
}

/// True when an approving review is on record for this PR
pub(crate) async fn has_recorded_approval(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    pr_number: u64,
) -> bool {
    matches!(
        read_approval(store, repo, task_id, pr_number).await,
        Ok(Some(event)) if !event.sha.is_empty()
    )
}

pub(crate) async fn write_approval(
    store: &Arc<TaskStore>,
    task_id: &str,
    pr_number: u64,
    event: &ApprovalEvent,
) {
    let key = approved_sha_key(&event.repo, task_id, pr_number);
    let value = serde_json::to_string(event).unwrap_or_default();
    if let Err(e) = store.kv_set(&key, &value).await {
        tracing::warn!(task_id, err = %e, "failed to record the approval");
    }
}

/// Record that the commit just pushed to the PR carries an approving review, so a later
/// NeedsReview event on the same commit skips the review agent
pub(crate) async fn record_approved_sha(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    pr_number: u64,
    dir: &Path,
    branch: &str,
    base: &str,
) {
    let Some(sha) = worktree_head_sha(dir).await else {
        return;
    };
    let event = ApprovalEvent {
        sha,
        repo: repo.to_string(),
        branch: branch.to_string(),
        base: base.to_string(),
    };
    write_approval(store, task_id, pr_number, &event).await;
}

/// Drop a recorded approval so the next NeedsReview event runs a real review
pub(crate) async fn clear_approved_sha(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    pr_number: u64,
) {
    if let Err(e) = store
        .kv_delete(&approved_sha_key(repo, task_id, pr_number))
        .await
    {
        tracing::warn!(task_id, err = %e, "failed to clear approved commit sha");
    }
}

/// Records the earlier approval as this review round's decision. Without it the stale
/// in_review check sees a round with no decision and resets the task every minute
pub(crate) async fn record_carried_approval(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    pr_number: u64,
) {
    store_log_activity(
        &Some(Arc::clone(store)),
        repo,
        task_id,
        "review_decision",
        None,
        None,
        None::<&str>,
        None::<&str>,
        Some(&serde_json::json!({
            "decision": "approve",
            "pr_number": pr_number,
            "reason": "head_already_approved",
        })),
    )
    .await;
}

/**
 * True when the PR's current head is the commit a previous review approved.
 *
 * Compared against the live PR head, not the worktree: a force-push from another clone
 * moves the head while the worktree stays on the approved commit, and skipping the
 * review there would merge code nobody reviewed. Any lookup failure returns false, so
 * the fallback is to review again (#3783)
 **/
pub(crate) async fn pr_head_already_approved(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    pr_number: u64,
) -> bool {
    let approved = match read_approval(store, repo, task_id, pr_number).await {
        Ok(event) => event.map(|e| e.sha),
        Err(e) => {
            tracing::warn!(task_id, err = %e, "failed to read approved commit sha");
            return false;
        }
    };
    if approved.is_none() {
        return false;
    }
    let gh = match GhHttp::new() {
        Ok(gh) => gh,
        Err(e) => {
            tracing::warn!(task_id, err = %e, "cannot check PR head against recorded approval");
            return false;
        }
    };
    let pr_head = match gh.get_pr(repo, pr_number).await {
        Ok(pr) => Some(pr.head.sha),
        Err(e) => {
            tracing::warn!(task_id, pr_number, err = %e, "failed to read PR head sha");
            None
        }
    };
    approval_covers_head(approved.as_deref(), pr_head.as_deref())
}

/// The task, PR and approval a carried approval must all agree on
struct CarryContext {
    approval: ApprovalEvent,
    worktree: PathBuf,
}

/**
 * Checks shared by both ways an approval moves to a new head: an approval event is on record,
 * and the PR still matches it. The head lives in the approved repository (not a fork) on the
 * approved branch, which is the task's branch, it targets the approved base, and the task
 * worktree tracks that branch and base. `None` means the approval stays where it was
 **/
async fn carry_context(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    pr: &GitHubPullRequest,
    pr_number: u64,
) -> Option<CarryContext> {
    let approval = read_approval(store, repo, task_id, pr_number)
        .await
        .ok()
        .flatten()?;
    let task = opt_store_get_task(&Some(Arc::clone(store)), repo, task_id).await?;
    if !pr_matches_approval(pr, &approval, &task.branch) || task.worktree.is_empty() {
        tracing::warn!(
            task_id,
            pr_number,
            "the PR does not match the approved repository, branch and base, approval not carried"
        );
        return None;
    }
    let worktree = PathBuf::from(&task.worktree);
    if git::git_ops::get_current_branch(&worktree).await != approval.branch
        || worktree::detect_default_branch(&worktree).await != approval.base
    {
        tracing::warn!(
            task_id,
            pr_number,
            "the task worktree does not track the approved branch and base, approval not carried"
        );
        return None;
    }
    Some(CarryContext { approval, worktree })
}

/// The PR's head repository, head branch and base are the approved ones, and the head branch is
/// the task's. A record without these fields (written before events) never matches
pub(crate) fn pr_matches_approval(
    pr: &GitHubPullRequest,
    approval: &ApprovalEvent,
    task_branch: &str,
) -> bool {
    let same_repo = !approval.repo.is_empty()
        && pr
            .head
            .repo
            .as_ref()
            .is_some_and(|r| r.full_name.eq_ignore_ascii_case(&approval.repo));
    same_repo
        && !approval.branch.is_empty()
        && pr.head.ref_ == approval.branch
        && approval.branch == task_branch
        && !approval.base.is_empty()
        && pr.base.ref_ == approval.base
}

/**
 * Carry an approval to a head that only merged the base branch in, as GitHub's update-branch
 * does. On top of `carry_context`: the head is a merge whose first parent is the approved commit
 * and whose second parent the base branch already holds, GitHub made it (committer `web-flow`)
 * for a repository collaborator, and its tree is exactly the clean merge of the two, recomputed
 * in the task worktree. Anything else keeps the approval where it was and the head gets a review
 **/
pub(crate) async fn carry_approval_across_base_update(
    store: &Arc<TaskStore>,
    gh: &GhHttp,
    repo: &str,
    task_id: &str,
    pr: &GitHubPullRequest,
    pr_number: u64,
) {
    let head_sha = pr.head.sha.as_str();
    let Ok(Some(recorded)) = read_approval(store, repo, task_id, pr_number).await else {
        return;
    };
    if recorded.sha == head_sha {
        return;
    }
    let Some(ctx) = carry_context(store, repo, task_id, pr, pr_number).await else {
        return;
    };
    let base_parent = match gh
        .github_base_merge_parent(repo, head_sha, &ctx.approval.sha, &pr.base.ref_)
        .await
    {
        Ok(Ok(parent)) => parent,
        Ok(Err(BaseMergeRefusal::NotByGithub)) => {
            tracing::warn!(
                task_id,
                pr_number,
                head_sha,
                "the base merge was not made by GitHub, approval not carried"
            );
            return;
        }
        Ok(Err(BaseMergeRefusal::AuthorNotCollaborator(author))) => {
            tracing::warn!(
                task_id,
                pr_number,
                author,
                "the base merge author is not a collaborator, approval not carried"
            );
            return;
        }
        Ok(Err(_)) => return,
        Err(e) => {
            tracing::warn!(task_id, err = %e, "cannot read the PR head commit to carry the approval");
            return;
        }
    };
    let _ = git::git_ops::git()
        .await
        .args(["fetch", "origin"])
        .current_dir(&ctx.worktree)
        .output()
        .await;
    if !is_clean_merge(&ctx.worktree, &ctx.approval.sha, &base_parent, head_sha).await {
        tracing::warn!(
            task_id,
            pr_number,
            head_sha,
            "the PR head is not a clean merge of the base, approval not carried"
        );
        return;
    }

    publish_carried_approval(store, gh, repo, task_id, pr_number, &ctx.approval, head_sha).await;
}

/**
 * After orch rebased the approved head onto the base, move the approval to the rebased head. On
 * top of `carry_context`: the rebase started from the approved commit, the new head holds the same
 * commits with the same patches and authors in the same order on top of the base tip, and GitHub
 * shows the new head as the PR head
 **/
pub(crate) async fn carry_approval_after_rebase(
    store: &Arc<TaskStore>,
    gh: &GhHttp,
    repo: &str,
    task_id: &str,
    pr: &GitHubPullRequest,
    pr_number: u64,
) {
    let Some(ctx) = carry_context(store, repo, task_id, pr, pr_number).await else {
        return;
    };
    let rebased_from = pr.head.sha.as_str();
    if ctx.approval.sha != rebased_from {
        return;
    }
    let Some(new_head) = worktree_head_sha(&ctx.worktree).await else {
        return;
    };
    let base = format!("origin/{}", pr.base.ref_);
    if !same_commits_rebased(&ctx.worktree, rebased_from, &base, &new_head).await {
        tracing::warn!(
            task_id,
            pr_number,
            "the rebased commits differ from the approved ones, approval not carried"
        );
        return;
    }
    let mut pushed = false;
    for _ in 0..3 {
        if gh
            .get_pr(repo, pr_number)
            .await
            .is_ok_and(|p| p.head.sha == new_head)
        {
            pushed = true;
            break;
        }
        time::sleep(Duration::from_secs(2)).await;
    }
    if !pushed {
        tracing::warn!(
            task_id,
            pr_number,
            "GitHub does not show the rebased head yet, approval not carried"
        );
        return;
    }

    publish_carried_approval(
        store,
        gh,
        repo,
        task_id,
        pr_number,
        &ctx.approval,
        &new_head,
    )
    .await;
}

/// The commits from the merge base of `old_head` and `base` up to `old_head`, replayed onto `base`
/// as `new_head`: same count, same patch ids, same authors, same order, and `new_head` sits on `base`
pub(crate) async fn same_commits_rebased(
    dir: &Path,
    old_head: &str,
    base: &str,
    new_head: &str,
) -> bool {
    let Some(fork_point) = git_output(dir, &["merge-base", old_head, base]).await else {
        return false;
    };
    let on_base = git_output(dir, &["merge-base", "--is-ancestor", base, new_head])
        .await
        .is_some();
    let old = commit_fingerprints(dir, &format!("{fork_point}..{old_head}")).await;
    let new = commit_fingerprints(dir, &format!("{base}..{new_head}")).await;
    on_base && old.is_some() && !old.as_ref().is_some_and(Vec::is_empty) && old == new
}

/// `(patch id, author)` for each commit in `range`, oldest first. `None` for a merge commit or
/// any git failure
async fn commit_fingerprints(dir: &Path, range: &str) -> Option<Vec<(String, String)>> {
    let log = git_output(
        dir,
        &["log", "--reverse", "--format=%H %P%x00%an <%ae>", range],
    )
    .await?;
    let mut prints = Vec::new();
    for line in log.lines().filter(|l| !l.is_empty()) {
        let (shas, author) = line.split_once('\0')?;
        let mut shas = shas.split(' ');
        let sha = shas.next()?;
        if shas.count() != 1 {
            return None;
        }
        prints.push((patch_id(dir, sha).await?, author.to_string()));
    }
    Some(prints)
}

/// `git patch-id --stable` of one commit, identical for the same change on another base
async fn patch_id(dir: &Path, sha: &str) -> Option<String> {
    let diff = git_output(dir, &["show", "--format=", sha]).await?;
    let mut child = Command::new("git")
        .args(["patch-id", "--stable"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    stdin.write_all(diff.as_bytes()).await.ok()?;
    stdin.write_all(b"\n").await.ok()?;
    drop(stdin);
    let out = child.wait_with_output().await.ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(String::from)
}

/// Record `head_sha` as approved and pass `orch/review` on it: the App check run in app mode, the
/// commit status in comment mode. The App review stays on the PR, a second one would repeat it
async fn publish_carried_approval(
    store: &Arc<TaskStore>,
    gh: &GhHttp,
    repo: &str,
    task_id: &str,
    pr_number: u64,
    approval: &ApprovalEvent,
    head_sha: &str,
) {
    let carried = ApprovalEvent {
        sha: head_sha.to_string(),
        ..approval.clone()
    };
    write_approval(store, task_id, pr_number, &carried).await;
    let note = "Approval carried across a base branch update";
    let app = review_app(repo).await;
    post_review_decision(
        app.as_deref(),
        gh,
        repo,
        Some(head_sha),
        &ReviewDecision::Approve { issues: vec![] },
        note,
    )
    .await;
    tracing::info!(
        task_id,
        pr_number,
        head_sha,
        "approval carried across a base branch update"
    );
}

/// Trimmed stdout of a git command, `None` when it fails
async fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// True when `head` holds exactly the tree of a conflict-free merge of `approved` and `base_parent`
pub(crate) async fn is_clean_merge(
    dir: &Path,
    approved: &str,
    base_parent: &str,
    head: &str,
) -> bool {
    let merged = git_output(dir, &["merge-tree", "--write-tree", approved, base_parent]).await;
    let head_tree = git_output(dir, &["rev-parse", &format!("{head}^{{tree}}")]).await;
    match (merged, head_tree) {
        (Some(merged), Some(head_tree)) => merged.lines().next() == Some(head_tree.as_str()),
        _ => false,
    }
}

/// Whether a recorded approval still applies to the PR's current head commit
pub(crate) fn approval_covers_head(approved: Option<&str>, pr_head: Option<&str>) -> bool {
    match (approved, pr_head) {
        (Some(approved), Some(head)) => !approved.is_empty() && approved == head,
        _ => false,
    }
}

/// Review agent decision result.
#[derive(Debug, Clone)]
pub(crate) enum ReviewDecision {
    /// Review approved, PR can be merged.
    Approve { issues: Vec<ReviewIssue> },
    /// Changes requested, PR needs fixes.
    RequestChanges {
        notes: String,
        issues: Vec<ReviewIssue>,
    },
    /// Review agent failed or crashed (reason stored for logging).
    Failed(String),
    /// Unrecoverable failure — task should be blocked for human intervention.
    Blocked(String),
    /// No PR exists — nothing to review, task marked done directly.
    Skipped,
    /// No PR and no commits — task re-routed for retry (no_code_reroutes counter must not be reset).
    Rerouted,
}

/// Read `HEAD` of a worktree. `None` when the worktree is gone or not a repository
pub(crate) async fn worktree_head_sha(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

pub(crate) const REVIEW_STATUS_CONTEXT: &str = "orch/review";

/// Commit status state and description for a review decision. Other decisions post nothing.
pub(crate) fn review_status(decision: &ReviewDecision) -> Option<(&'static str, &'static str)> {
    match decision {
        ReviewDecision::Approve { .. } => Some(("success", "Approved by automated review")),
        ReviewDecision::RequestChanges { .. } => {
            Some(("failure", "Automated review requested changes"))
        }
        _ => None,
    }
}

/// Post `orch/review` on the SHA the review started from. A new push moves the PR head, so an
/// old decision never covers a new commit.
pub(crate) async fn post_review_status(
    gh: &GhHttp,
    repo: &str,
    sha: Option<&str>,
    state: &str,
    description: &str,
) {
    let Some(sha) = sha else {
        return;
    };
    if let Err(e) = gh
        .post_commit_status(repo, sha, state, REVIEW_STATUS_CONTEXT, description)
        .await
    {
        tracing::warn!(sha, error = %e, "failed to post orch/review commit status");
    }
}

/// The App to review `repo` with, `None` in comment mode or where the App is not installed
pub(crate) async fn review_app(repo: &str) -> Option<Arc<AppAuth>> {
    let mode = mode::for_repo(repo);
    review_app_for(mode, app::shared(), repo).await
}

pub(crate) async fn review_app_for(
    mode: ReviewMode,
    app: Option<Arc<AppAuth>>,
    repo: &str,
) -> Option<Arc<AppAuth>> {
    if mode != ReviewMode::App {
        return None;
    }

    let app = app?;
    match app.token_for_repo(repo).await {
        Ok(Some(_)) => Some(app),
        Ok(None) => {
            tracing::info!(
                repo,
                "review.mode is app but the App is not installed here, reviewing with a comment"
            );
            None
        }
        Err(e) => {
            tracing::warn!(repo, error = %e, "App installation lookup failed, reviewing with a comment");
            None
        }
    }
}

pub(crate) async fn post_review_decision(
    app: Option<&AppAuth>,
    gh: &GhHttp,
    repo: &str,
    sha: Option<&str>,
    decision: &ReviewDecision,
    notes: &str,
) {
    let Some((state, description)) = review_status(decision) else {
        return;
    };

    match (app, sha) {
        (Some(app), Some(sha)) => {
            let approved = state == "success";
            let completed = CheckRunState::Completed {
                approved,
                summary: if notes.is_empty() { description } else { notes },
            };
            if let Err(e) = app.post_check_run(repo, sha, completed).await {
                tracing::warn!(sha, error = %e, "failed to complete the orch/review check run");
            }
        }
        _ => post_review_status(gh, repo, sha, state, description).await,
    }
}

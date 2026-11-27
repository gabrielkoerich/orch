use super::{ReviewContext, ReviewDecision, worktree_head_sha};
/// Maximum consecutive review agent failures before the task is blocked
/// for human intervention. Exported so `tick` and `sync` use the same threshold.
use crate::backends::ExternalTask;
use crate::engine::attribution_footer;
use crate::engine::runner::response::ReviewIssue;
use crate::github::app;
use crate::github::app::AppAuth;
use crate::github::app::CheckRunState;
use crate::github::app::InlineComment;
use crate::github::app::ReviewEvent;
use crate::github::http::GhHttp;
use crate::review_mode;
use crate::review_mode::ReviewMode;
use std::fmt::Write as _;
use std::sync::Arc;

pub(super) fn append_review_issues(body: &mut String, issues: &[ReviewIssue]) {
    if issues.is_empty() {
        return;
    }
    body.push_str("\n**Issues Found:**\n");
    for issue in issues {
        let _ = writeln!(
            body,
            "- `{}` line {}: {} [{}]",
            issue.file,
            issue
                .line
                .map_or_else(|| "?".to_string(), |line| line.to_string()),
            issue.description,
            issue.severity
        );
    }
}

pub(super) fn build_pr_review_comment(
    decision: &ReviewDecision,
    review_notes_for_comment: &str,
) -> String {
    match decision {
        ReviewDecision::Approve { issues } => {
            let mut body =
                format!("## Automated Review \u{2014} Approve\n\n{review_notes_for_comment}");
            if !issues.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            append_review_issues(&mut body, issues);
            body
        }
        ReviewDecision::RequestChanges { notes, issues } => {
            let mut body = format!("## Automated Review \u{2014} Changes Requested\n\n{notes}\n");
            append_review_issues(&mut body, issues);
            body
        }
        _ => String::new(),
    }
}

pub(super) const REVIEW_STATUS_CONTEXT: &str = "orch/review";

/// Commit status state and description for a review decision. Other decisions post nothing.
pub(super) fn review_status(decision: &ReviewDecision) -> Option<(&'static str, &'static str)> {
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
pub(super) async fn post_review_status(
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

pub(super) async fn post_review_comment(
    task: &ExternalTask,
    repo: &str,
    ctx: &ReviewContext,
    decision: &ReviewDecision,
    review_notes_for_comment: &str,
) -> anyhow::Result<()> {
    let gh = match GhHttp::new() {
        Ok(gh) => gh,
        Err(e) => {
            tracing::warn!(
                task_id = task.id.0,
                error = %e,
                "failed to create GH client for review comment"
            );
            return Ok(());
        }
    };
    let mut pr_comment = build_pr_review_comment(decision, review_notes_for_comment);
    if ctx.same_agent_fallback && !pr_comment.is_empty() {
        pr_comment.push_str(
            "\n> Note: no other agent was available, so this PR was reviewed by the same agent in a new session.\n",
        );
    }

    // The agent may have rebased or committed a fix, pushed just before this, so the head read
    // before the review is stale and an App review on it would be refused or ignored
    let head_sha = worktree_head_sha(&ctx.worktree_path)
        .await
        .or_else(|| ctx.head_sha.clone());

    let app = review_app(repo).await;
    post_review_decision(
        app.as_deref(),
        &gh,
        repo,
        head_sha.as_deref(),
        decision,
        review_notes_for_comment,
    )
    .await;

    if !pr_comment.is_empty() {
        let footer =
            attribution_footer("Reviewed", &ctx.review_agent, Some(ctx.review_model_str()));
        let pr_comment_with_footer = format!("{pr_comment}{footer}");
        if let Err(e) = publish_review(
            app.as_deref(),
            &gh,
            repo,
            ctx.pr_number,
            head_sha.as_deref(),
            decision,
            &pr_comment_with_footer,
        )
        .await
        {
            tracing::warn!(
                task_id = task.id.0,
                pr_number = ctx.pr_number,
                error = %e,
                "failed to post automated review comment on PR"
            );
        }
    }

    Ok(())
}

/// The App to review `repo` with, `None` in comment mode or where the App is not installed
pub(super) async fn review_app(repo: &str) -> Option<Arc<AppAuth>> {
    let mode = review_mode::for_repo(repo);
    review_app_for(mode, app::shared(), repo).await
}

pub(super) async fn review_app_for(
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

/// `orch/review` as an App check run in app mode, as a commit status in comment mode
pub(super) async fn post_review_started(
    app: Option<&AppAuth>,
    gh: &GhHttp,
    repo: &str,
    sha: Option<&str>,
) {
    match (app, sha) {
        (Some(app), Some(sha)) => {
            if let Err(e) = app
                .post_check_run(repo, sha, CheckRunState::InProgress)
                .await
            {
                tracing::warn!(sha, error = %e, "failed to post the orch/review check run");
            }
        }
        _ => post_review_status(gh, repo, sha, "pending", "Automated review running").await,
    }
}

pub(super) async fn post_review_decision(
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

/// Submit the decision as an App review on the sha the agent read, with the issues on diff lines
/// as inline comments. The `## Automated Review` comment goes out only without an App or when
/// GitHub refuses the review, so the decision always lands somewhere
pub(super) async fn publish_review(
    app: Option<&AppAuth>,
    gh: &GhHttp,
    repo: &str,
    pr_number: u64,
    head_sha: Option<&str>,
    decision: &ReviewDecision,
    comment: &str,
) -> anyhow::Result<()> {
    let event = match decision {
        ReviewDecision::Approve { .. } => Some(ReviewEvent::Approve),
        ReviewDecision::RequestChanges { .. } => Some(ReviewEvent::RequestChanges),
        _ => None,
    };
    if let (Some(app), Some(sha), Some(event)) = (app, head_sha, event) {
        let inline = inline_comments(app, repo, pr_number, decision).await;
        match app
            .submit_review(repo, pr_number, event, comment, sha, &inline)
            .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(pr_number, sha, error = %e, "failed to submit App review, posting the decision as a comment");
            }
        }
    }
    gh.add_comment(repo, &pr_number.to_string(), comment).await
}

/// Issues on a changed line become inline comments, the rest stay in the review body, which
/// already lists every issue. GitHub refuses a review whose comment points outside the diff
pub(super) async fn inline_comments(
    app: &AppAuth,
    repo: &str,
    pr_number: u64,
    decision: &ReviewDecision,
) -> Vec<InlineComment> {
    let ReviewDecision::RequestChanges { issues, .. } = decision else {
        return Vec::new();
    };

    if issues.is_empty() {
        return Vec::new();
    }

    match app.pull_request_hunks(repo, pr_number).await {
        Ok(hunks) => app::split_inline(issues, &hunks).0,
        Err(e) => {
            tracing::warn!(pr_number, error = %e, "could not read the PR diff, issues stay in the review body");
            Vec::new()
        }
    }
}

//! PR body and PR creation

use super::{PrCreateError, PrCreateResult, format_task_ref};
use crate::github::http::GhHttp;
use crate::plan;
use regex::Regex;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::process::Command;
use tokio::time;

/// Drop sentences and list items about pushing or opening the PR
/// Agents report they could not push, orch already pushed, so those lines read as false on GitHub
fn strip_push_and_pr_creation(text: &str) -> String {
    let mut kept_lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if kept_lines.last().is_some_and(|l| !l.is_empty()) {
                kept_lines.push(String::new());
            }
            continue;
        }

        let indent_len = line.len() - line.trim_start().len();
        let indent = &line[..indent_len];
        let trimmed = line.trim();
        let (marker, rest) = match list_marker(trimmed) {
            Some((m, r)) => (m, r),
            None => ("", trimmed),
        };

        let kept: Vec<&str> = split_statements(rest)
            .into_iter()
            .filter(|s| !is_push_or_pr_creation_statement(s))
            .collect();
        if kept.is_empty() {
            continue;
        }
        kept_lines.push(format!("{indent}{marker}{}", kept.join(" ")));
    }
    while kept_lines.last().is_some_and(String::is_empty) {
        kept_lines.pop();
    }
    kept_lines.join("\n")
}

fn split_statements(text: &str) -> Vec<&str> {
    static SEP: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\.{2,}|(?:[.!?]))\s+").expect("valid statement separator"));
    let mut statements = Vec::new();
    let mut last = 0;
    for cap in SEP.captures_iter(text) {
        let full = cap.get(0).expect("full separator match");
        let punct = cap.get(1).expect("punctuation capture");
        let ellipsis = punct.as_str().len() >= 2 && punct.as_str().bytes().all(|b| b == b'.');
        let end = if ellipsis { punct.start() } else { punct.end() };
        let stmt = text[last..end].trim();
        if !stmt.is_empty() {
            statements.push(stmt);
        }
        last = full.end();
    }
    let rest = text[last..].trim();
    if !rest.is_empty() {
        statements.push(rest);
    }
    statements
}

fn list_marker(line: &str) -> Option<(&str, &str)> {
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some((prefix, rest));
        }
    }
    let dot = line.find(". ")?;
    if !line[..dot].is_empty() && line[..dot].bytes().all(|b| b.is_ascii_digit()) {
        Some((&line[..dot + 2], &line[dot + 2..]))
    } else {
        None
    }
}

fn is_push_or_pr_creation_statement(text: &str) -> bool {
    static PAT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)",
            r"\bgit\s+push\b",
            r"|\bgh\s+pr\s+create\b",
            r"|\bgh\s+pr\s+open\b",
            r"|\bpush\s+(was\s+)?(denied|blocked|rejected)\b",
            r"|\bpush\s+permission\b",
            r"|\bcouldn['’]?t\s+push\b",
            r"|\bcould\s+not\s+push\b",
            r"|\bcan['’]?t\s+push\b",
            r"|\bcannot\s+push\b",
            r"|\bdidn['’]?t\s+push\b",
            r"|\bdid\s+not\s+push\b",
            r"|\bnothing\s+is\s+on\s+the\s+remote\b",
            r"|\bnot\s+on\s+the\s+remote\b",
            r"|\bopen(?:ing)?\s+(?:the|a)\s+pr\b",
            r"|\bcreat(?:e|ing)\s+(?:the|a)\s+pr\b",
            r"|\bopen(?:ing)?\s+(?:the|a)\s+pull\s+request\b",
            r"|\bcreat(?:e|ing)\s+(?:the|a)\s+pull\s+request\b",
        ))
        .expect("valid push/pr-creation regex")
    });
    PAT.is_match(text)
}

fn cleaned_pr_items(items: &[String]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| {
            let cleaned = strip_push_and_pr_creation(item);
            (!cleaned.is_empty()).then_some(cleaned)
        })
        .collect()
}

/// Format the GitHub PR description from the agent summary, stripping push and PR-creation statements
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is one section of the PR body"
)]
pub(super) fn build_pr_body(
    title: &str,
    summary: &str,
    accomplished: &[String],
    remaining: &[String],
    files: &[String],
    task_id: &str,
    agent: &str,
    model: Option<&str>,
) -> String {
    let cleaned_summary = strip_push_and_pr_creation(summary);
    let summary_text = if cleaned_summary.is_empty() {
        title
    } else {
        cleaned_summary.as_str()
    };
    let accomplished = cleaned_pr_items(accomplished);
    let remaining = cleaned_pr_items(remaining);

    let mut body = format!("## Summary\n\n{summary_text}");

    if !accomplished.is_empty() {
        body.push_str("\n\n### What was done\n\n");
        for item in &accomplished {
            let _ = writeln!(body, "- {item}");
        }
    }

    if !remaining.is_empty() {
        body.push_str("\n\n### Remaining\n\n");
        for item in &remaining {
            let _ = writeln!(body, "- {item}");
        }
    }

    if !files.is_empty() {
        body.push_str("\n\n### Files changed\n\n");
        for file in files {
            let _ = writeln!(body, "- `{file}`");
        }
    }

    // CI-01 must stay open after merge until the default-branch ruleset exists
    let bootstrap = plan::is_ci_bootstrap_id(title.split_whitespace().next().unwrap_or(title));
    if !bootstrap
        && !task_id.starts_with("internal:")
        && let Ok(issue_num) = task_id.parse::<u64>()
    {
        let _ = write!(body, "\n\nCloses #{issue_num}");
    }

    let model_str = model.map(|m| format!(" using `{m}`")).unwrap_or_default();
    let task_ref = format_task_ref(task_id);
    let _ = write!(
        body,
        "\n\n---\n*Task {task_ref} · Created by {agent}[bot] via [Orch](https://github.com/gabrielkoerich/orch){model_str}*"
    );
    body
}

/// Create a PR if one doesn't already exist.
///
/// Uses [`GhHttp`] exclusively for GitHub API operations. No `gh` CLI
/// fallback is used, ensuring consistent behavior across environments.
///
/// # Returns
/// - `Ok(url)` - PR URL (newly created or already existed)
/// - `Err(PrCreateError)` - API error or other failure
#[expect(
    clippy::too_many_arguments,
    reason = "the PR needs its branch, body sections, attribution and target together"
)]
pub async fn create_pr_if_needed(
    dir: &Path,
    branch: &str,
    title: &str,
    summary: &str,
    accomplished: &[String],
    remaining: &[String],
    files: &[String],
    task_id: &str,
    agent: &str,
    model: Option<&str>,
    repo: &str,
    base_branch: &str,
) -> PrCreateResult<String> {
    // Retries for transient 5xx errors on PR creation below
    const MAX_RETRIES: u32 = 3;
    let gh = GhHttp::new()?;

    // Check if PR already exists using GhHttp API
    match gh.get_pr_number(repo, branch).await {
        Ok(Some(pr_number)) => {
            tracing::info!(task_id, pr = pr_number, "PR already exists");
            // Append attribution footer if the agent created the PR without one.
            append_pr_footer_if_missing(&gh, repo, pr_number, task_id, agent, model).await;
            // Fetch the existing PR to get its URL so the caller can store pr_number.
            let pr = match gh.get_pr(repo, pr_number).await {
                Ok(pr) => pr,
                Err(e) => {
                    tracing::warn!(task_id, error = %e, "get_pr API call failed after detecting existing PR");
                    return Err(PrCreateError::ApiError(e));
                }
            };
            return Ok(pr.html_url);
        }
        Ok(None) => {
            // No PR exists, proceed to create one
        }
        Err(e) => {
            tracing::warn!(task_id, error = %e, "get_pr_number API call failed");
            return Err(PrCreateError::ApiError(e));
        }
    }

    // When the agent didn't self-report files, derive them from git.
    let git_files: Vec<String>;
    let effective_files: &[String] = if files.is_empty() {
        match Command::new("git")
            .args([
                "diff",
                &format!("origin/{base_branch}...HEAD"),
                "--name-only",
            ])
            .current_dir(dir)
            .output()
            .await
        {
            Ok(output) if output.status.success() => {
                git_files = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(ToString::to_string)
                    .collect();
                &git_files
            }
            _ => files,
        }
    } else {
        files
    };

    let body = build_pr_body(
        title,
        summary,
        accomplished,
        remaining,
        effective_files,
        task_id,
        agent,
        model,
    );

    // Always use the short task title for the PR title (summary goes in body)
    let pr_title = title;

    // Create PR using GhHttp API with exponential backoff retry for transient 5xx errors.
    let mut last_error: Option<anyhow::Error> = None;
    let mut created_url: Option<String> = None;
    let mut had_transient_error = false;
    let mut effective_base = base_branch.to_string();
    let mut tried_api_fallback = false;

    for attempt in 1..=MAX_RETRIES {
        match gh
            .create_pr(repo, pr_title, &body, branch, &effective_base)
            .await
        {
            Ok(u) => {
                created_url = Some(u);
                break;
            }
            Err(e) => {
                let err_str = format!("{e}");
                if is_transient_github_error(&err_str) {
                    had_transient_error = true;
                    if attempt < MAX_RETRIES {
                        let delay = Duration::from_secs(2u64.pow(attempt));
                        tracing::warn!(
                            task_id,
                            attempt,
                            delay_secs = delay.as_secs(),
                            error = %e,
                            "transient GitHub API error during PR creation — retrying"
                        );
                        time::sleep(delay).await;
                    }
                } else if is_invalid_base_error(&err_str) && !tried_api_fallback {
                    // Local base branch detection was wrong (e.g. "remotes/origin/main").
                    // Query GitHub API for the canonical default branch and retry once.
                    tried_api_fallback = true;
                    tracing::warn!(
                        task_id,
                        base_branch = %effective_base,
                        error = %e,
                        "PR creation failed with invalid base — fetching default branch from GitHub API"
                    );
                    match gh.get_default_branch(repo).await {
                        Ok(api_default) if api_default != effective_base => {
                            tracing::info!(
                                task_id,
                                old_base = %effective_base,
                                new_base = %api_default,
                                "retrying PR creation with GitHub API default branch"
                            );
                            effective_base = api_default;
                            continue;
                        }
                        Ok(api_default) => {
                            tracing::warn!(
                                task_id,
                                base_branch = %api_default,
                                "GitHub API default branch matches local — no fallback possible"
                            );
                        }
                        Err(api_err) => {
                            tracing::warn!(
                                task_id,
                                error = %api_err,
                                "failed to fetch default branch from GitHub API"
                            );
                        }
                    }
                }
                last_error = Some(e);
            }
        }
    }

    let url = if let Some(u) = created_url {
        u
    } else {
        let e = last_error.unwrap_or_else(|| {
            anyhow::anyhow!("PR creation failed after retries (no error captured)")
        });
        let err_str = format!("{e}");
        // For transient 5xx errors, GitHub may have created the PR despite returning
        // an error (e.g. 502 after the write succeeded). Re-check for an existing PR
        // before propagating the failure so we don't orphan the PR from the task.
        // Also check when the final error is non-transient but a prior attempt had a
        // transient error — the PR may have been created on that earlier attempt (e.g.
        // attempt 1 returns 502, attempt 2 returns 422 "already exists").
        if is_transient_github_error(&err_str) || had_transient_error {
            tracing::warn!(
                task_id,
                error = %e,
                "transient GitHub API error after all retries — checking if PR was actually created"
            );
            match gh.get_pr_number(repo, branch).await {
                Ok(Some(pr_number)) => {
                    tracing::info!(
                        task_id,
                        pr = pr_number,
                        "PR was created despite transient errors — recovering"
                    );
                    append_pr_footer_if_missing(&gh, repo, pr_number, task_id, agent, model).await;
                    match gh.get_pr(repo, pr_number).await {
                        Ok(pr) => pr.html_url,
                        Err(get_err) => {
                            tracing::warn!(
                                task_id,
                                error = %get_err,
                                "failed to fetch PR URL after transient-error recovery"
                            );
                            return Err(PrCreateError::ApiError(e));
                        }
                    }
                }
                Ok(None) => {
                    tracing::warn!(
                        task_id,
                        "PR was not created after all retries — propagating original error"
                    );
                    return Err(PrCreateError::ApiError(e));
                }
                Err(check_err) => {
                    tracing::warn!(
                        task_id,
                        error = %check_err,
                        "failed to verify PR existence after transient error retries — propagating original error"
                    );
                    return Err(PrCreateError::ApiError(e));
                }
            }
        } else {
            return Err(PrCreateError::ApiError(e));
        }
    };

    tracing::info!(task_id, pr_url = %url, "created PR via GhHttp API");

    Ok(url)
}

/// Returns true if the error string indicates a transient GitHub API failure
/// (HTTP 5xx, network errors, DNS failures, TLS issues, rate limits), where
/// the operation may succeed on retry.
pub(crate) fn is_transient_github_error(err_str: &str) -> bool {
    // Treat transport/send failures and explicit GhHttp server-error messages
    // as transient. Older callers relied on parsing the "failed (NNN)" pattern
    // to detect 5xx — keep that, but also recognize the newer wrappers.
    if err_str.contains("HTTP send failed") {
        return true;
    }
    if err_str.contains("GitHub API server error") {
        return true;
    }

    // Check for HTTP 5xx status codes
    if extract_github_http_status(err_str).is_some_and(|s| (500..600).contains(&s)) {
        return true;
    }

    // Check for common network-level transient errors
    let lower = err_str.to_lowercase();

    // Connection and transport errors
    if lower.contains("connection reset")
        || lower.contains("broken pipe")
        || lower.contains("connection refused")
        || lower.contains("connection closed")
        || lower.contains("transport error")
        || lower.contains("failed to connect")
        || lower.contains("failed to connect to github.com port")
        || lower.contains("curl: (7) failed to connect")
        || lower.contains("couldn't connect to server")
        || lower.contains("could not connect to server")
    {
        return true;
    }

    // Timeout errors
    if lower.contains("timeout") {
        return true;
    }

    // DNS resolution errors
    if lower.contains("dns error")
        || lower.contains("resolve")
        || lower.contains("name resolution")
        || lower.contains("no such host")
    {
        return true;
    }

    // TLS/SSL errors
    if lower.contains("tls handshake")
        || lower.contains("certificate")
        || lower.contains("ssl error")
    {
        return true;
    }

    // Network unreachable errors
    if lower.contains("network is unreachable")
        || lower.contains("host unreachable")
        || lower.contains("temporary failure")
    {
        return true;
    }

    // EOF errors (connection dropped mid-request)
    if lower.contains("unexpected eof") || lower.contains("unexpected end of file") {
        return true;
    }

    // Internal circuit-breaker state (orch's own GitHub 5xx protection)
    if lower.contains("circuit-breaker") || lower.contains("circuit breaker") {
        return true;
    }

    // Rate limit errors (transient with cooldown/retry)
    if lower.contains("rate limit") || lower.contains("too many requests") {
        return true;
    }

    false
}

/// Returns true when the error indicates the PR base branch is invalid.
///
/// GitHub returns HTTP 422 with `PullRequest.base invalid` when the base
/// branch name is not a valid ref in the repository. This is a terminal
/// configuration error — retrying with the same base will never succeed.
pub(crate) fn is_invalid_base_error(err_str: &str) -> bool {
    let lower = err_str.to_lowercase();
    lower.contains("422") && lower.contains("base") && lower.contains("invalid")
}

/// Returns true when an error looks like a transient *GitHub API/CLI* failure.
///
/// This is stricter than [`is_transient_github_error`]: in addition to matching
/// a transient transport pattern, it requires GitHub-specific context in the
/// message. This prevents generic agent/runtime transport failures (for
/// example "broken pipe" from a model stream disconnect) from being treated as
/// GitHub retry signals in review reroute logic.
pub(crate) fn is_transient_github_api_error(err_str: &str) -> bool {
    if !is_transient_github_error(err_str) {
        return false;
    }

    if extract_github_http_status(err_str).is_some() {
        return true;
    }

    let lower = err_str.to_lowercase();
    lower.contains("github")
        || lower.contains("api.github.com")
        || lower.contains("gh api")
        || lower.contains("gh pr")
        || lower.contains("gh error")
        // `circuit-breaker` is orch's internal GitHub 5xx protection, set
        // exclusively by GhHttp.  It can never originate from agent/runtime
        // failures, so treating it as a GitHub-context marker is safe even
        // when upstream wrapping strips the "GitHub API" prefix.
        || lower.contains("circuit-breaker")
        || lower.contains("circuit breaker")
}

/// Extract the HTTP status code from a GhHttp error string.
///
/// GhHttp formats errors as: `"GitHub API POST https://... failed (NNN): body"`
/// This parses the numeric status code from that format.
pub(super) fn extract_github_http_status(err_str: &str) -> Option<u16> {
    let marker = "failed (";
    let start = err_str.find(marker)? + marker.len();
    let rest = &err_str[start..];
    let end = rest.find(')')?;
    rest[..end].parse().ok()
}

/// Append the Orch attribution footer to an existing PR body if not already present.
///
/// Called when the agent created the PR directly (bypassing orch's
/// `create_pr_if_needed`), so the footer was never added by `build_pr_body`.
async fn append_pr_footer_if_missing(
    gh: &GhHttp,
    repo: &str,
    pr_number: u64,
    task_id: &str,
    agent: &str,
    model: Option<&str>,
) {
    let pr = match gh.get_pr(repo, pr_number).await {
        Ok(pr) => pr,
        Err(e) => {
            tracing::debug!(task_id, error = %e, "failed to fetch PR for footer check");
            return;
        }
    };

    let body = pr.body.unwrap_or_default();
    if body.contains("via [Orch]") {
        return; // Footer already present
    }

    let model_str = model.map(|m| format!(" using `{m}`")).unwrap_or_default();
    let task_ref = format_task_ref(task_id);
    let footer = format!(
        "\n\n---\n*Task {task_ref} · Created by {agent}[bot] via [Orch](https://github.com/gabrielkoerich/orch){model_str}*"
    );
    let new_body = format!("{body}{footer}");

    match gh.update_pr_body(repo, pr_number, &new_body).await {
        Err(e) => {
            tracing::warn!(task_id, error = %e, "failed to append footer to PR (non-fatal)");
        }
        _ => {
            tracing::info!(
                task_id,
                pr = pr_number,
                "appended attribution footer to agent-created PR"
            );
        }
    }
}

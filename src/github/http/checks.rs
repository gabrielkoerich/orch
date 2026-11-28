//! Status check, ruleset, check run and workflow calls

use super::{GITHUB_API, GhHttp, has_status};
use crate::github::app;
use crate::github::types;
use crate::github::types::is_own_check;
use crate::github::types::{GitHubAnnotation, GitHubCheckRun, RequiredStatusCheck};
use reqwest::header;
use std::collections::HashSet;
use std::fmt::Debug;
use std::path::Path;
use urlencoding;

impl GhHttp {
    /// Post a commit status. `state` is one of pending, success, failure, error.
    pub async fn post_commit_status(
        &self,
        repo: &str,
        sha: &str,
        state: &str,
        context: &str,
        description: &str,
    ) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/statuses/{sha}");
        let payload = serde_json::json!({
            "state": state,
            "context": context,
            "description": description,
        });
        self.post_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Required status checks for a branch from classic protection and from rulesets
    pub async fn get_required_status_check_contexts(
        &self,
        repo: &str,
        branch: &str,
    ) -> anyhow::Result<Vec<RequiredStatusCheck>> {
        let branch = urlencoding::encode(branch);
        let classic_url = format!(
            "{}/repos/{repo}/branches/{branch}/protection",
            self.api_root()
        );
        let rules_url = format!("{}/repos/{repo}/rules/branches/{branch}", self.api_root());

        let classic_fut = self.get_json::<serde_json::Value>(&classic_url);
        let rules_fut = self.get_all_pages::<serde_json::Value>(&rules_url, &[("per_page", "100")]);
        let (classic_resp, rules_resp) = tokio::join!(classic_fut, rules_fut);

        let classic = match classic_resp {
            Ok(value) => parse_classic_required_status_checks(&value),
            Err(e) => {
                if is_no_branch_protection_error(&e.to_string()) {
                    Vec::new()
                } else {
                    return Err(e);
                }
            }
        };

        let ruleset = match rules_resp {
            Ok(rules) => parse_ruleset_required_status_checks(&rules),
            Err(e) => {
                if has_status(&e.to_string(), 404) {
                    Vec::new()
                } else {
                    return Err(e);
                }
            }
        };

        Ok(merge_required_status_checks(classic, ruleset))
    }

    /// Get commit status contexts for a SHA.
    pub async fn get_commit_status_contexts(
        &self,
        repo: &str,
        sha: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let url = format!(
            "{}/repos/{repo}/commits/{sha}/status?per_page=100",
            self.api_root()
        );
        // The /commits/{sha}/status endpoint returns a wrapper object:
        // { "state": "pending", "total_count": N, "statuses": [...] }
        // Use get_all_pages_from_wrapper to follow pagination and extract the
        // `statuses` array across pages so we don't silently truncate at 100.
        let statuses: Vec<serde_json::Value> =
            self.get_all_pages_from_wrapper(&url, "statuses").await?;

        Ok(statuses
            .into_iter()
            .filter_map(|status| {
                let context = status.get("context").and_then(|v| v.as_str())?;
                let state = status.get("state").and_then(|v| v.as_str())?;
                Some((context.to_string(), state.to_string()))
            })
            .collect())
    }

    pub(super) fn combined_status_state(
        total: u64,
        failing: u64,
        pending: u64,
        has_workflows: bool,
        mergeable_state: Option<&str>,
    ) -> String {
        if failing > 0 {
            "failure".to_string()
        } else if pending > 0 {
            "pending".to_string()
        } else if total == 0 && has_workflows {
            // No check runs yet. If GitHub already reports mergeable_state="clean"
            // the PR is ready (e.g. all workflows use paths-ignore and skipped this
            // commit). Treating it as pending would stall auto-merge forever.
            if mergeable_state == Some("clean") {
                "success".to_string()
            } else {
                "pending".to_string()
            }
        } else {
            "success".to_string()
        }
    }

    /// Return `true` when the repository has at least one GitHub Actions workflow.
    ///
    /// This is used to distinguish repos that genuinely have no CI configured from
    /// repos where CI exists but GitHub has not surfaced any runs yet.
    pub async fn has_workflows(&self, repo: &str) -> anyhow::Result<bool> {
        let url = format!("{GITHUB_API}/repos/{repo}/actions/workflows?per_page=1");
        let resp = self.get_json::<serde_json::Value>(&url).await;
        match resp {
            Ok(value) => Ok(value
                .get("workflows")
                .and_then(|v| v.as_array())
                .is_some_and(|workflows| !workflows.is_empty())),
            Err(e) => {
                let err = e.to_string();
                if has_status(&err, 404) {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// True when `.github/workflows` on `branch` has at least one YAML file
    pub async fn has_workflow_on_branch(&self, repo: &str, branch: &str) -> anyhow::Result<bool> {
        let branch = urlencoding::encode(branch);
        let url = format!(
            "{}/repos/{repo}/contents/.github/workflows?ref={branch}",
            self.api_root()
        );
        match self.get_json::<serde_json::Value>(&url).await {
            Ok(value) => Ok(value.as_array().is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry
                        .get("name")
                        .and_then(|name| name.as_str())
                        .is_some_and(|name| {
                            Path::new(name)
                                .extension()
                                .and_then(|ext| ext.to_str())
                                .is_some_and(|ext| {
                                    ext.eq_ignore_ascii_case("yml")
                                        || ext.eq_ignore_ascii_case("yaml")
                                })
                        })
                })
            })),
            Err(e) => {
                let err = e.to_string();
                if has_status(&err, 404) {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// True when `path` exists on `branch`
    pub async fn has_file_on_branch(
        &self,
        repo: &str,
        path: &str,
        branch: &str,
    ) -> anyhow::Result<bool> {
        let path = path
            .split('/')
            .map(|segment| urlencoding::encode(segment).into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let branch = urlencoding::encode(branch);
        let url = format!(
            "{}/repos/{repo}/contents/{path}?ref={branch}",
            self.api_root()
        );
        match self.get_json::<serde_json::Value>(&url).await {
            Ok(_) => Ok(true),
            Err(e) => {
                let err = e.to_string();
                if has_status(&err, 404) {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Create a repository ruleset that requires `checks` on the default branch
    pub async fn create_required_check_ruleset(
        &self,
        repo: &str,
        checks: &[RequiredStatusCheck],
    ) -> anyhow::Result<()> {
        let url = format!("{}/repos/{repo}/rulesets", self.api_root());
        let payload = required_check_ruleset_payload(checks);
        self.post_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Id and required checks of the repository ruleset named `name`, if it exists
    pub async fn get_named_ruleset_required_checks(
        &self,
        repo: &str,
        name: &str,
    ) -> anyhow::Result<Option<(i64, Vec<RequiredStatusCheck>)>> {
        let url = format!("{}/repos/{repo}/rulesets", self.api_root());
        let rulesets: Vec<serde_json::Value> =
            self.get_all_pages(&url, &[("per_page", "100")]).await?;
        let Some(id) = rulesets.iter().find_map(|ruleset| {
            if ruleset.get("name").and_then(|n| n.as_str()) != Some(name) {
                return None;
            }
            ruleset.get("id").and_then(serde_json::Value::as_i64)
        }) else {
            return Ok(None);
        };
        let url = format!("{}/repos/{repo}/rulesets/{id}", self.api_root());
        let value: serde_json::Value = self.get_json(&url).await?;
        let rules = value
            .get("rules")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(Some((id, parse_ruleset_required_status_checks(&rules))))
    }

    /// Replace the required checks on an existing repository ruleset
    pub async fn update_required_check_ruleset(
        &self,
        repo: &str,
        ruleset_id: i64,
        checks: &[RequiredStatusCheck],
    ) -> anyhow::Result<()> {
        let url = format!("{}/repos/{repo}/rulesets/{ruleset_id}", self.api_root());
        let payload = required_check_ruleset_payload(checks);
        self.put_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Get check runs for a PR head SHA.
    pub async fn get_check_runs(
        &self,
        repo: &str,
        sha: &str,
    ) -> anyhow::Result<Vec<GitHubCheckRun>> {
        // Use `filter=latest` so GitHub returns only the most recent check run
        // per check name. Without this, the API returns all runs (oldest
        // first) and callers that pick the first match may observe a stale
        // failed run when a newer run with the same name has succeeded.
        //
        // Use `per_page=100` and manual pagination via `get_all_pages_from_wrapper`
        // because this endpoint returns a *wrapper object* — `{ "total_count": N,
        // "check_runs": [...] }` — rather than a raw JSON array.  `get_all_pages`
        // cannot be used here because it calls `serde_json::from_str::<Vec<T>>`
        // which would fail on an object body.
        let url = format!(
            "{}/repos/{repo}/commits/{sha}/check-runs?filter=latest&per_page=100",
            self.api_root()
        );
        self.get_all_pages_from_wrapper(&url, "check_runs").await
    }

    /// Get annotations for a specific check run.
    ///
    /// Returns the list of annotations attached to the check run, including
    /// billing failure messages ("The job was not started because recent account
    /// payments have failed …") that GitHub surfaces as annotations rather than
    /// as a distinct CI conclusion value.
    pub async fn get_check_run_annotations(
        &self,
        repo: &str,
        check_run_id: u64,
    ) -> anyhow::Result<Vec<GitHubAnnotation>> {
        let url = format!("{GITHUB_API}/repos/{repo}/check-runs/{check_run_id}/annotations");
        self.get_all_pages(&url, &[("per_page", "100")]).await
    }

    /// Last `lines` lines of the log of an Actions job. A check run id is the job id
    pub async fn get_job_log_tail(
        &self,
        repo: &str,
        job_id: u64,
        lines: usize,
    ) -> anyhow::Result<String> {
        let url = format!("{GITHUB_API}/repos/{repo}/actions/jobs/{job_id}/logs");
        let bytes = self.get_bytes(&url).await?;
        let text = String::from_utf8_lossy(&bytes);
        let all: Vec<&str> = text.lines().collect();
        let start = all.len().saturating_sub(lines);
        Ok(all[start..].join("\n"))
    }

    /// Get combined CI check status for a git ref
    ///
    /// Counts come from [`Self::get_check_runs`], which requests `filter=latest`
    /// and follows pagination, so a failure or in-progress run past the first page still counts
    pub async fn get_combined_status(
        &self,
        repo: &str,
        git_ref: &str,
        has_workflows: bool,
        mergeable_state: Option<&str>,
    ) -> anyhow::Result<(String, u64, u64, u64, u64)> {
        let runs = self.get_check_runs(repo, git_ref).await?;
        let (total, passing, failing, pending) = Self::count_ci_runs(&runs);
        let state =
            Self::combined_status_state(total, failing, pending, has_workflows, mergeable_state);

        Ok((state, total, passing, failing, pending))
    }

    /// Count CI runs as (total, passing, failing, pending), skipping the review's own checks
    pub(super) fn count_ci_runs(runs: &[types::GitHubCheckRun]) -> (u64, u64, u64, u64) {
        let mut total = 0u64;
        let mut passing = 0u64;
        let mut failing = 0u64;
        let mut pending = 0u64;

        for run in runs.iter().filter(|r| !is_own_check(&r.name)) {
            total += 1;
            if !run.is_finished() {
                pending += 1;
            } else if run.is_passed() {
                passing += 1;
            } else {
                failing += 1;
            }
        }

        (total, passing, failing, pending)
    }

    /// Rerun the latest failed workflow run for a given workflow name and branch.
    ///
    /// Finds the most recent failed (or cancelled) run of `workflow_name` on
    /// `branch` triggered by a `push` event, then calls the GitHub Actions
    /// rerun endpoint so the **same check run** on the commit flips green.
    pub async fn rerun_failed_workflow(
        &self,
        repo: &str,
        workflow_name: &str,
        branch: &str,
    ) -> anyhow::Result<()> {
        // List recent runs for this branch, filtered to push events
        let url = format!(
            "{GITHUB_API}/repos/{repo}/actions/runs?branch={branch}&event=push&per_page=10"
        );
        let resp: serde_json::Value = self.get_json(&url).await?;
        let runs = resp["workflow_runs"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("no workflow_runs array in response"))?;

        // Find the latest failed/cancelled run matching the workflow name
        let run_id = runs
            .iter()
            .find(|r| {
                r["name"].as_str() == Some(workflow_name)
                    && matches!(r["conclusion"].as_str(), Some("failure" | "cancelled"))
            })
            .and_then(|r| r["id"].as_u64())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no failed run found for workflow '{workflow_name}' on branch '{branch}'"
                )
            })?;

        // POST rerun — no body needed
        let rerun_url = format!("{GITHUB_API}/repos/{repo}/actions/runs/{run_id}/rerun");
        let auth = self.auth_header(&rerun_url).await?;
        let make_req = || {
            Ok(self
                .client
                .post(&rerun_url)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &body);
            anyhow::bail!("GitHub API POST rerun {rerun_url} failed ({status}): {body}");
        }

        Ok(())
    }

    /// Check the latest automated review comment on a PR.
    pub async fn get_automated_review_status(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> anyhow::Result<Option<String>> {
        let comments = self.list_comments(repo, &pr_number.to_string()).await?;

        // Find the most recent automated review comment authored by a collaborator or the App
        let app_login = app::bot_login().await;
        let mut newest: Option<&types::GitHubComment> = None;
        for c in &comments {
            if !c.body.starts_with("## Automated Review") {
                continue;
            }
            let trusted = match app_login.as_deref() {
                Some(app) if app::same_login(&c.user.login, app) => Ok(true),
                _ => self.is_collaborator(repo, &c.user.login).await,
            };
            match trusted {
                Ok(true) => match newest {
                    None => newest = Some(c),
                    Some(prev) => {
                        if c.created_at > prev.created_at {
                            newest = Some(c);
                        }
                    }
                },
                Ok(false) => {
                    tracing::warn!(
                        user = %c.user.login,
                        pr_number,
                        "ignoring automated review comment from non-collaborator"
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        user = %c.user.login,
                        error = %e,
                        "failed to check collaborator status, skipping comment"
                    );
                }
            }
        }

        match newest {
            Some(c) => {
                let first_line = c.body.lines().next().unwrap_or("");
                if first_line.contains("Automated Review \u{2014} Approve") {
                    Ok(Some("approve".to_string()))
                } else if first_line.contains("Automated Review \u{2014} Changes Requested") {
                    Ok(Some("changes_requested".to_string()))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }
}

/// GitHub returns for private repos on the Free plan
pub(super) fn is_no_branch_protection_error(err: &str) -> bool {
    has_status(err, 404)
        || (err.contains("403")
            && err.contains("Upgrade to GitHub Pro or make this repository public"))
}

pub(crate) const REQUIRED_CHECK_RULESET_NAME: &str = "orch required checks";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RulesetCreateKind {
    CheckNeverRun,
    NameTaken,
    Refused,
    Retryable,
    Other,
}

pub(crate) fn required_check_ruleset_payload(checks: &[RequiredStatusCheck]) -> serde_json::Value {
    let required: Vec<serde_json::Value> = checks
        .iter()
        .map(|check| {
            if let Some(id) = check.integration_id {
                serde_json::json!({ "context": check.context, "integration_id": id })
            } else {
                serde_json::json!({ "context": check.context })
            }
        })
        .collect();
    serde_json::json!({
        "name": REQUIRED_CHECK_RULESET_NAME,
        "target": "branch",
        "enforcement": "active",
        "conditions": {
            "ref_name": {
                "include": ["~DEFAULT_BRANCH"],
                "exclude": []
            }
        },
        "rules": [{
            "type": "required_status_checks",
            "parameters": {
                "strict_required_status_checks_policy": true,
                "do_not_enforce_on_create": false,
                "required_status_checks": required
            }
        }]
    })
}

pub(crate) fn classify_ruleset_create_error(err: &str) -> RulesetCreateKind {
    let lower = err.to_ascii_lowercase();
    if lower.contains("already been taken") || lower.contains("name already exists") {
        return RulesetCreateKind::NameTaken;
    }
    if lower.contains("not a valid check")
        || lower.contains("required to be present is not found")
        || (lower.contains("422") && lower.contains("status check") && lower.contains("not found"))
    {
        return RulesetCreateKind::CheckNeverRun;
    }
    if lower.contains("(500")
        || lower.contains("(502")
        || lower.contains("(503")
        || lower.contains("(504")
        || lower.contains("(429")
    {
        return RulesetCreateKind::Retryable;
    }
    // "(401" / "(403" is the status token, a 422 body can contain those digits
    if lower.contains("(403")
        || lower.contains("(401")
        || lower.contains("upgrade to github pro")
        || lower.contains("not accessible")
    {
        return RulesetCreateKind::Refused;
    }
    // timeout and transport errors never include an HTTP status
    if !has_http_status_token(&lower) {
        return RulesetCreateKind::Retryable;
    }
    RulesetCreateKind::Other
}

fn has_http_status_token(err: &str) -> bool {
    err.match_indices('(').any(|(i, _)| {
        err.as_bytes()
            .get(i + 1..i + 4)
            .is_some_and(|digits| digits.iter().all(u8::is_ascii_digit))
    })
}

/// Classic protection prefers `checks` (includes `app_id`) and falls back to `contexts`
pub(super) fn parse_classic_required_status_checks(
    value: &serde_json::Value,
) -> Vec<RequiredStatusCheck> {
    if let Some(checks) = value
        .pointer("/required_status_checks/checks")
        .and_then(|v| v.as_array())
    {
        let parsed: Vec<RequiredStatusCheck> =
            checks.iter().filter_map(parse_classic_check).collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }

    value
        .pointer("/required_status_checks/contexts")
        .and_then(|v| v.as_array())
        .map(|contexts| {
            contexts
                .iter()
                .filter_map(|context| {
                    context.as_str().map(|s| RequiredStatusCheck {
                        context: s.to_string(),
                        integration_id: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_classic_check(check: &serde_json::Value) -> Option<RequiredStatusCheck> {
    let context = check.get("context")?.as_str()?.to_string();
    let integration_id = check
        .get("app_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|&id| id > 0);
    Some(RequiredStatusCheck {
        context,
        integration_id,
    })
}

/// Collect `required_status_checks` entries from every active `required_status_checks` rule
pub(super) fn parse_ruleset_required_status_checks(
    rules: &[serde_json::Value],
) -> Vec<RequiredStatusCheck> {
    let mut out = Vec::new();
    for rule in rules {
        if rule.get("type").and_then(|t| t.as_str()) != Some("required_status_checks") {
            continue;
        }
        let Some(checks) = rule
            .pointer("/parameters/required_status_checks")
            .or_else(|| rule.get("required_status_checks"))
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        for check in checks {
            let Some(context) = check.get("context").and_then(|v| v.as_str()) else {
                continue;
            };
            let integration_id = check
                .get("integration_id")
                .and_then(serde_json::Value::as_i64)
                .filter(|&id| id > 0);
            out.push(RequiredStatusCheck {
                context: context.to_string(),
                integration_id,
            });
        }
    }
    out
}

pub(super) fn merge_required_status_checks(
    mut checks: Vec<RequiredStatusCheck>,
    extra: Vec<RequiredStatusCheck>,
) -> Vec<RequiredStatusCheck> {
    checks.extend(extra);
    let mut seen = HashSet::new();
    checks.retain(|c| seen.insert((c.context.clone(), c.integration_id)));
    checks
}

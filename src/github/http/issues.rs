use super::{
    BlockedByPayload, GITHUB_API, GhHttp, READ_TIMEOUT, has_status, is_forbidden_rate_limit,
    page_cache_etag, page_cache_hit, page_cache_key, page_cache_store, parse_link_next,
    parse_positive_issue_number, redact_posted_body, sanitize_github_issue_numbers,
};
use crate::backends::BlockedByFetch;
use crate::backends::BlockedByIssue;
use crate::github::types::{GitHubComment, GitHubIssue};
use anyhow::Context;
use reqwest::{Response, StatusCode, header};
use std::collections::HashMap;
use urlencoding;

impl GhHttp {
    /// Verify authentication by fetching the current user.
    ///
    /// Returns detailed error messages for common auth failures to help
    /// users diagnose configuration issues at startup.
    pub async fn auth_status(&self) -> anyhow::Result<()> {
        match self
            .get_json::<serde_json::Value>(&format!("{GITHUB_API}/user"))
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let err_str = e.to_string();

                // Provide helpful error messages for common auth failures
                if err_str.contains("401") || err_str.contains("Bad credentials") {
                    anyhow::bail!(
                        "GitHub authentication failed: Invalid or expired token. \
                         Check your GH_TOKEN/GITHUB_TOKEN environment variables, \
                         or gh.auth configuration in ~/.orch/config.yml. \
                         Run `orch auth check` to verify your setup."
                    );
                }

                if err_str.contains("403") && err_str.contains("rate limit") {
                    anyhow::bail!(
                        "GitHub API rate limited. Wait before retrying, \
                         or check your token's rate limit quota."
                    );
                }

                Err(e)
            }
        }
    }

    /// Create a GitHub issue.
    pub async fn create_issue(
        &self,
        repo: &str,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> anyhow::Result<GitHubIssue> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "labels": labels,
        });
        self.post_json(&url, &payload).await
    }

    /// Replace an issue's title, body and labels.
    ///
    /// Labels replace the full set. Callers pass labels they want to keep
    pub async fn update_issue(
        &self,
        repo: &str,
        number: u64,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> anyhow::Result<GitHubIssue> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}");
        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "labels": labels,
        });
        let text = self.patch_json_raw(&url, &payload).await?;
        serde_json::from_str(&text)
            .with_context(|| format!("failed to parse issue update response: {text}"))
    }

    /// List issues that block `issue_number`.
    ///
    /// `GET /repos/{owner}/{repo}/issues/{issue_number}/dependencies/blocked_by`.
    /// GA since 2025-08-21, no preview media type. This call sends
    /// `X-GitHub-Api-Version: 2026-03-10` and `Accept: application/vnd.github+json`.
    /// Each page is sent with `If-None-Match` when a prior ETag is known, and a 304
    /// replays the stored body. 404 and 403 (except rate limit) are `Unavailable`.
    /// Transport errors and 5xx are `Failed`, so the caller does not drop stored links
    pub async fn list_blocked_by(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> anyhow::Result<BlockedByFetch> {
        let mut next = Some(format!(
            "{}/repos/{repo}/issues/{issue_number}/dependencies/blocked_by?per_page=100",
            self.api_root()
        ));
        let mut found = Vec::new();
        while let Some(url) = next.take() {
            let cache_key = page_cache_key::<BlockedByPayload>(&url, None);
            let mut cached_etag = cache_key.as_deref().and_then(page_cache_etag);
            let (body, page_next) = loop {
                let resp = match self
                    .dependency_request(&url, reqwest::Method::GET, None, cached_etag.as_deref())
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        let message = err.to_string();
                        // `send_with_retries` words a send failure with this marker
                        if message.contains("(transport error)") {
                            return Ok(BlockedByFetch::Unreachable(message));
                        }
                        return Ok(BlockedByFetch::Failed(message));
                    }
                };
                let status = resp.status();
                if status == StatusCode::NOT_MODIFIED {
                    if let Some((body, next_link)) = cache_key.as_deref().and_then(page_cache_hit) {
                        Self::record_success();
                        break (body, next_link);
                    }
                    // The entry is normally there, since If-None-Match is only sent
                    // when it is, but the size cap can clear the map in between.
                    // Dropping the ETag makes the retry unconditional, so the loop
                    // cannot see a second 304
                    cached_etag = None;
                    continue;
                }
                if status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN {
                    let body = resp.text().await.unwrap_or_default();
                    if is_forbidden_rate_limit(status, &body) {
                        Self::maybe_record_rate_limit_from_body(status, &body);
                        return Ok(BlockedByFetch::Failed(body));
                    }
                    return Ok(BlockedByFetch::Unavailable);
                }
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    Self::maybe_record_rate_limit_from_body(status, &body);
                    return Ok(BlockedByFetch::Failed(format!("{status}: {body}")));
                }
                Self::record_success();
                let page_next = parse_link_next(resp.headers());
                let etag = resp
                    .headers()
                    .get(header::ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(ToString::to_string);
                let body = match resp.text().await {
                    Ok(body) => body,
                    Err(err) => {
                        return Ok(BlockedByFetch::Failed(format!(
                            "blocked_by body read failed: {err}"
                        )));
                    }
                };
                // Cache only a body that parses, or a 304 would replay it forever
                if serde_json::from_str::<Vec<BlockedByPayload>>(&body).is_ok()
                    && let (Some(key), Some(etag)) = (cache_key.clone(), etag)
                {
                    page_cache_store(key, etag, body.clone(), page_next.clone());
                }
                break (body, page_next);
            };
            let page: Vec<BlockedByPayload> = match serde_json::from_str(&body) {
                Ok(page) => page,
                Err(err) => {
                    return Ok(BlockedByFetch::Failed(format!(
                        "blocked_by parse failed: {err}"
                    )));
                }
            };
            for item in page {
                if item.number <= 0 {
                    continue;
                }
                let item_repo = item
                    .repository
                    .as_ref()
                    .map(|r| r.full_name.clone())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| repo.to_string());
                found.push(BlockedByIssue {
                    github_id: item.id,
                    repo: item_repo,
                    number: item.number,
                    state: item.state,
                    state_reason: item.state_reason,
                });
            }
            next = page_next;
        }
        Ok(BlockedByFetch::Native(found))
    }

    /// `POST /repos/{owner}/{repo}/issues/{issue_number}/dependencies/blocked_by`
    /// with `{"issue_id": blocking_issue_id}`. Same API version as `list_blocked_by`.
    /// 201 is success. A body that says the link already exists is also success
    pub async fn add_blocked_by(
        &self,
        repo: &str,
        issue_number: u64,
        blocking_issue_id: i64,
    ) -> anyhow::Result<()> {
        let url =
            format!("{GITHUB_API}/repos/{repo}/issues/{issue_number}/dependencies/blocked_by");
        let resp = self
            .dependency_request(
                &url,
                reqwest::Method::POST,
                Some(serde_json::json!({ "issue_id": blocking_issue_id })),
                None,
            )
            .await?;
        let status = resp.status();
        if status.is_success() {
            Self::record_success();
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        if body.to_ascii_lowercase().contains("already") {
            return Ok(());
        }
        Self::maybe_record_rate_limit_from_body(status, &body);
        anyhow::bail!("add blocked_by failed ({status}): {body}")
    }

    /// Remove a blocked-by link. `issue_id` in the path is the blocking issue's database id
    pub async fn remove_blocked_by(
        &self,
        repo: &str,
        number: u64,
        blocking_issue_id: u64,
    ) -> anyhow::Result<()> {
        let url = format!(
            "{GITHUB_API}/repos/{repo}/issues/{number}/dependencies/blocked_by/{blocking_issue_id}"
        );
        self.delete(&url).await?;
        Ok(())
    }

    /// GET or POST an issue-dependency URL with API version 2026-03-10
    pub(super) async fn dependency_request(
        &self,
        url: &str,
        method: reqwest::Method,
        body: Option<serde_json::Value>,
        if_none_match: Option<&str>,
    ) -> anyhow::Result<Response> {
        Self::check_backoff(url)?;
        let auth = self.auth_header(url).await?;
        let make_req = || {
            let mut req = self
                .client
                .request(method.clone(), url)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2026-03-10");
            if method == reqwest::Method::GET {
                req = req.timeout(READ_TIMEOUT);
            }
            if let Some(etag) = if_none_match {
                req = req.header(header::IF_NONE_MATCH, etag);
            }
            if let Some(ref payload) = body {
                req = req.json(payload);
            }
            Ok(req)
        };
        self.send_with_retries(make_req, false).await
    }

    /// Get a single issue.
    pub async fn get_issue(&self, repo: &str, number: &str) -> anyhow::Result<GitHubIssue> {
        self.get_json(&format!("{}/repos/{repo}/issues/{number}", self.api_root()))
            .await
    }

    /// Check whether an issue number refers to a pull request.
    pub async fn is_pull_request(&self, repo: &str, number: &str) -> bool {
        self.get_issue(repo, number)
            .await
            .is_ok_and(|i| i.pull_request.is_some())
    }

    /// List issues filtered by a label (paginated).
    pub async fn list_issues(&self, repo: &str, label: &str) -> anyhow::Result<Vec<GitHubIssue>> {
        self.list_issues_with_state(repo, label, "open").await
    }

    /// List issues filtered by a label and issue state (paginated).
    ///
    /// `state` is passed directly to the GitHub API: `"open"`, `"closed"`, or `"all"`.
    pub async fn list_issues_with_state(
        &self,
        repo: &str,
        label: &str,
        state: &str,
    ) -> anyhow::Result<Vec<GitHubIssue>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        // An empty label lists every issue
        let mut query = vec![("state", state), ("per_page", "100")];
        if !label.is_empty() {
            query.push(("labels", label));
        }
        let all: Vec<GitHubIssue> = self.get_all_pages(&url, &query).await?;
        // GitHub /issues API returns PRs too — filter them out
        Ok(all
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .collect())
    }

    /// List closed issues updated since `since` (ISO 8601), filtered by label.
    ///
    /// Used for dedup checks — prevents re-creating issues that were recently closed.
    pub async fn list_issues_closed_since(
        &self,
        repo: &str,
        label: &str,
        since: &str,
    ) -> anyhow::Result<Vec<GitHubIssue>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        let all: Vec<GitHubIssue> = self
            .get_all_pages(
                &url,
                &[
                    ("labels", label),
                    ("state", "closed"),
                    ("since", since),
                    ("per_page", "100"),
                ],
            )
            .await?;
        Ok(all
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .collect())
    }

    /// List closed issues updated since `since` (ISO 8601), no label filter.
    pub async fn list_closed_issues_since(
        &self,
        repo: &str,
        since: &str,
    ) -> anyhow::Result<Vec<GitHubIssue>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        let all: Vec<GitHubIssue> = self
            .get_all_pages(
                &url,
                &[("state", "closed"), ("since", since), ("per_page", "100")],
            )
            .await?;
        Ok(all
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .collect())
    }

    /// List all open issues (no label filter, paginated).
    ///
    /// The URL carries no `since`, so it repeats across calls and `get_all_pages` can send
    /// it conditionally. GitHub answers 304 while nothing changed, which is free quota
    pub async fn list_all_open_issues(&self, repo: &str) -> anyhow::Result<Vec<GitHubIssue>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        let params = [("state", "open"), ("per_page", "100")];
        let all: Vec<GitHubIssue> = self.get_all_pages(&url, &params).await?;
        // GitHub /issues API returns PRs too — filter them out
        Ok(all
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .collect())
    }

    /// List open pull requests (paginated) using the REST pulls endpoint.
    /// Returns raw JSON values for flexibility in callers.
    pub async fn list_open_pulls(&self, repo: &str) -> anyhow::Result<Vec<serde_json::Value>> {
        let url = format!("{GITHUB_API}/repos/{repo}/pulls");
        let all: Vec<serde_json::Value> = self
            .get_all_pages(&url, &[("state", "open"), ("per_page", "100")])
            .await?;
        Ok(all)
    }

    /// List all issues (open and closed, no label filter, paginated).
    ///
    /// This is more efficient than making separate calls for open and closed.
    pub async fn list_all_issues(&self, repo: &str) -> anyhow::Result<Vec<GitHubIssue>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        let all: Vec<GitHubIssue> = self
            .get_all_pages(&url, &[("state", "all"), ("per_page", "100")])
            .await?;
        // GitHub /issues API returns PRs too — filter them out
        Ok(all
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .collect())
    }

    /// Add labels to an issue.
    pub async fn add_labels(
        &self,
        repo: &str,
        number: &str,
        labels: &[String],
    ) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}/labels");
        let payload = serde_json::json!({ "labels": labels });
        self.post_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Ensure a label exists on the repo, creating it if needed.
    pub async fn ensure_label(
        &self,
        repo: &str,
        name: &str,
        color: &str,
        description: &str,
    ) -> anyhow::Result<()> {
        let encoded = urlencoding::encode(name);
        let get_url = format!("{GITHUB_API}/repos/{repo}/labels/{encoded}");

        // Check if label exists
        match self.get_bytes(&get_url).await {
            Ok(_) => return Ok(()),
            Err(e) if has_status(&e.to_string(), 404) => {
                tracing::debug!(repo, name, "ensure_label: label not found, creating");
            }
            Err(e) => {
                tracing::warn!(repo, name, err = %e, "ensure_label: GET failed, attempting create");
            }
        }

        // Create the label
        let create_url = format!("{GITHUB_API}/repos/{repo}/labels");
        let payload = serde_json::json!({
            "name": name,
            "color": color,
            "description": description,
        });

        match self.post_json_raw(&create_url, &payload).await {
            Ok(_) => {
                Self::record_success();
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("422")
                    || msg.contains("already_exists")
                    || msg.contains("Unprocessable")
                {
                    tracing::debug!(repo, name, "ensure_label: label already exists (race)");
                    return Ok(());
                }
                // Tolerate creation failures (matches old behavior).
                tracing::warn!(repo, name, err = %msg, "ensure_label: create failed, continuing");
            }
        }
        Ok(())
    }

    /// Remove a label from an issue.
    pub async fn remove_label(&self, repo: &str, number: &str, label: &str) -> anyhow::Result<()> {
        let encoded = urlencoding::encode(label);
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}/labels/{encoded}");
        match self.delete(&url).await {
            Ok(_) => Ok(()),
            Err(e) if has_status(&e.to_string(), 404) => {
                tracing::debug!(repo, number, label, "label already removed (404)");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Add a comment to an issue.
    pub async fn add_comment(&self, repo: &str, number: &str, body: &str) -> anyhow::Result<()> {
        let body = redact_posted_body(body);
        let url = format!("{}/repos/{repo}/issues/{number}/comments", self.api_root());
        let payload = serde_json::json!({ "body": body });
        self.post_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Add a reaction to an issue.
    ///
    /// Supported content values: "+1", "-1", "laugh", "confused", "heart", "hooray", "rocket", "eyes"
    pub async fn add_reaction(
        &self,
        repo: &str,
        number: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}/reactions");
        let payload = serde_json::json!({ "content": content });
        self.post_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Add a reaction to an issue comment.
    ///
    /// Supported content values: "+1", "-1", "laugh", "confused", "heart", "hooray", "rocket", "eyes"
    pub async fn add_comment_reaction(
        &self,
        repo: &str,
        comment_id: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/comments/{comment_id}/reactions");
        let payload = serde_json::json!({ "content": content });
        self.post_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// List comments on an issue.
    pub async fn list_comments(
        &self,
        repo: &str,
        number: &str,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}/comments");
        self.get_all_pages(&url, &[("per_page", "100")]).await
    }

    /// Get issue/PR comments since a given timestamp (paginated).
    pub async fn get_mentions(
        &self,
        repo: &str,
        since: &str,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        let url = format!("{}/repos/{repo}/issues/comments", self.api_root());
        self.get_all_pages(&url, &[("since", since), ("per_page", "100")])
            .await
    }

    /// Get all comments for a specific issue number.
    pub async fn get_issue_comments(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        let url = format!(
            "{}/repos/{repo}/issues/{issue_number}/comments",
            self.api_root()
        );
        self.get_all_pages(&url, &[("per_page", "100")]).await
    }

    /// Get the current authenticated username (cached after first call).
    pub async fn get_whoami(&self) -> anyhow::Result<String> {
        if let Some(cached) = self.cached_username.get() {
            return Ok(cached.clone());
        }

        // An installation token cannot read `GET /user`, and the owner is the identity mentions name
        let mut owner = self.clone();
        owner.owner_only = true;
        let user: serde_json::Value = owner.get_json(&format!("{GITHUB_API}/user")).await?;
        let username = user
            .get("login")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("failed to get current user"))?;

        let _ = self.cached_username.set(username.clone());
        Ok(username)
    }

    /// Batch-fetch state and labels for multiple GitHub issues in a single GraphQL request.
    ///
    /// Returns a map of issue number (as string) → `(state, labels)` where state is
    /// lowercase ("open" / "closed") to match the REST API convention used elsewhere.
    pub async fn batch_get_issue_states(
        &self,
        repo: &str,
        issue_numbers: &[u64],
        caller_context: &str,
    ) -> anyhow::Result<HashMap<String, (String, Vec<String>)>> {
        // GitHub GraphQL complexity limit: chunk at 50 to avoid slow/rejected queries.
        const CHUNK_SIZE: usize = 50;

        if issue_numbers.is_empty() {
            return Ok(HashMap::new());
        }

        let (valid_issue_numbers, invalid_issue_numbers) =
            sanitize_github_issue_numbers(issue_numbers);
        if !invalid_issue_numbers.is_empty() {
            tracing::warn!(
                repo,
                caller_context,
                invalid_issue_numbers = ?invalid_issue_numbers,
                total_supplied = issue_numbers.len(),
                "dropping invalid GitHub issue numbers before batch issue-state GraphQL query"
            );
        }
        if valid_issue_numbers.is_empty() {
            return Ok(HashMap::new());
        }

        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {repo}"))?;

        let mut result = HashMap::new();

        for chunk in valid_issue_numbers.chunks(CHUNK_SIZE) {
            let aliases: String = chunk
                .iter()
                .map(|n| {
                    format!("issue{n}: issue(number: {n}) {{ state labels(first: 100) {{ nodes {{ name }} }} }}")
                })
                .collect::<Vec<_>>()
                .join(" ");

            let query = format!(
                r"query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {aliases} }} }}"
            );

            let resp = self
                .graphql_with_vars(&query, serde_json::json!({ "owner": owner, "name": name }))
                .await?;
            let repo_data = resp
                .pointer("/data/repository")
                .ok_or_else(|| anyhow::anyhow!("missing /data/repository in GraphQL response"))?;

            for n in chunk {
                if let Some(issue) = repo_data.get(format!("issue{n}")) {
                    let state = issue
                        .get("state")
                        .and_then(|v| v.as_str())
                        .unwrap_or("UNKNOWN")
                        .to_lowercase();
                    let labels: Vec<String> = issue
                        .pointer("/labels/nodes")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|l| {
                                    l.get("name").and_then(|nm| nm.as_str()).map(String::from)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    result.insert(n.to_string(), (state, labels));
                }
            }
        }
        Ok(result)
    }

    /// Close a GitHub issue.
    pub async fn close_issue(&self, repo: &str, number: &str) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}");
        let payload = serde_json::json!({ "state": "closed" });
        self.patch_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Reopen a closed GitHub issue.
    pub async fn reopen_issue(&self, repo: &str, number: &str) -> anyhow::Result<()> {
        let url = format!("{}/repos/{repo}/issues/{number}", self.api_root());
        let payload = serde_json::json!({ "state": "open" });
        self.patch_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Check if a user is a collaborator.
    pub async fn is_collaborator(&self, repo: &str, username: &str) -> anyhow::Result<bool> {
        let url = format!("{GITHUB_API}/repos/{repo}/collaborators/{username}");
        Self::check_backoff(&url)?;
        let auth = self.auth_header(&url).await?;
        let make_req = || {
            Ok(self
                .client
                .get(&url)
                .timeout(READ_TIMEOUT)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
        let status = resp.status();
        if status == StatusCode::NO_CONTENT {
            Ok(true)
        } else if status == StatusCode::NOT_FOUND {
            Ok(false)
        } else {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API collaborator check failed ({status}): {body}");
        }
    }

    /// Add a sub-issue relationship via GraphQL.
    pub async fn add_sub_issue(
        &self,
        parent_node_id: &str,
        child_node_id: &str,
    ) -> anyhow::Result<()> {
        let query = format!(
            r#"mutation {{
                addSubIssue(input: {{issueId: "{parent_node_id}", subIssueId: "{child_node_id}"}}) {{
                    issue {{ number }}
                    subIssue {{ number }}
                }}
            }}"#
        );
        self.graphql_with_headers(&query, &["GraphQL-Features:sub_issues"])
            .await?;
        Ok(())
    }

    /// Link an issue to a branch using GraphQL.
    ///
    /// Creates a "Development" sidebar link in GitHub that connects the issue
    /// to the branch, similar to `gh issue develop`.
    ///
    /// # Arguments
    /// * `repo` - Repository in "owner/repo" format
    /// * `issue_number` - The issue number to link
    /// * `branch` - The branch name to link the issue to
    ///
    /// Returns the linked branch ID on success.
    pub async fn link_issue_to_branch(
        &self,
        repo: &str,
        issue_number: u64,
        branch: &str,
    ) -> anyhow::Result<String> {
        let parts: Vec<&str> = repo.split('/').collect();
        if parts.len() != 2 {
            anyhow::bail!("invalid repo format: expected 'owner/repo', got '{repo}'");
        }
        let (owner, repo_name) = (parts[0], parts[1]);

        // First, get the repository and issue node IDs
        let query = r"query($owner: String!, $name: String!, $number: Int!) {
                repository(owner: $owner, name: $name) {
                    id
                    issue(number: $number) {
                        id
                    }
                }
            }";
        let vars = serde_json::json!({
            "owner": owner,
            "name": repo_name,
            "number": issue_number,
        });

        let result = self.graphql_with_vars(query, vars).await?;

        let repo_id = result
            .pointer("/data/repository/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("failed to get repository node ID"))?;

        let issue_id = result
            .pointer("/data/repository/issue/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("failed to get issue node ID"))?;

        // Fetch branch ref to get the commit OID required by createLinkedBranch
        let branch_query = r"query($owner: String!, $name: String!, $ref: String!) {
                repository(owner: $owner, name: $name) {
                    ref(qualifiedName: $ref) {
                        target {
                            oid
                        }
                    }
                }
            }";
        let branch_vars = serde_json::json!({
            "owner": owner,
            "name": repo_name,
            "ref": format!("refs/heads/{}", branch),
        });

        let branch_result = self.graphql_with_vars(branch_query, branch_vars).await?;
        let branch_oid = branch_result
            .pointer("/data/repository/ref/target/oid")
            .and_then(|v| v.as_str());

        // If branch doesn't exist yet, use the default branch OID.
        // createLinkedBranch will create the branch at that OID AND link it.
        let branch_oid = if let Some(oid) = branch_oid {
            oid.to_string()
        } else {
            // Get default branch OID as the base for the new branch
            let default_query = r"query($owner: String!, $name: String!) {
                    repository(owner: $owner, name: $name) {
                        defaultBranchRef {
                            target {
                                oid
                            }
                        }
                    }
                }";
            let default_vars = serde_json::json!({
                "owner": owner,
                "name": repo_name,
            });
            let default_result = self.graphql_with_vars(default_query, default_vars).await?;
            match default_result
                .pointer("/data/repository/defaultBranchRef/target/oid")
                .and_then(|v| v.as_str())
            {
                Some(oid) => {
                    tracing::debug!(
                        repo,
                        issue_number,
                        branch,
                        "branch not found, using default branch OID to create and link"
                    );
                    oid.to_string()
                }
                None => {
                    return Ok(format!("unlinked:{branch}"));
                }
            }
        };

        // Use createLinkedBranch mutation to link the issue to the existing branch.
        // Required fields: issueId, repositoryId, name (branch name), oid (commit SHA).
        let mutation = r"mutation($issueId: ID!, $repositoryId: ID!, $name: String!, $oid: GitObjectID!) {
                createLinkedBranch(input: {
                    issueId: $issueId
                    repositoryId: $repositoryId
                    name: $name
                    oid: $oid
                }) {
                    linkedBranch {
                        id
                        ref {
                            name
                        }
                    }
                }
            }";
        let mutation_vars = serde_json::json!({
            "issueId": issue_id,
            "repositoryId": repo_id,
            "name": branch,
            "oid": branch_oid,
        });

        let link_result = self.graphql_with_vars(mutation, mutation_vars).await;

        match link_result {
            Ok(result) => {
                let linked_branch_id = result
                    .pointer("/data/createLinkedBranch/linkedBranch/id")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                if let Some(id) = linked_branch_id {
                    tracing::info!(
                        repo,
                        issue_number,
                        branch,
                        "linked issue to branch via GraphQL API"
                    );
                    Ok(id)
                } else {
                    // Branch may already be linked - check for errors
                    let errors = result
                        .get("errors")
                        .and_then(|e| e.as_array())
                        .cloned()
                        .unwrap_or_default();

                    if errors.iter().any(|e| {
                        e.get("message")
                            .and_then(|m| m.as_str())
                            .is_some_and(|m| m.contains("already"))
                    }) {
                        tracing::debug!(
                            repo,
                            issue_number,
                            branch,
                            "issue already linked to branch"
                        );
                        Ok(format!("already_linked:{branch}"))
                    } else {
                        anyhow::bail!("failed to link issue to branch: {result:?}")
                    }
                }
            }
            Err(e) => {
                // Check if it's a "already exists" type error
                let err_str = format!("{e}");
                if err_str.contains("already") || err_str.contains("existing") {
                    tracing::debug!(
                        repo,
                        issue_number,
                        branch,
                        "issue already linked to branch (error check)"
                    );
                    Ok(format!("already_linked:{branch}"))
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Get sub-issues via GraphQL (paginated).
    pub async fn get_sub_issues(&self, repo: &str, number: &str) -> anyhow::Result<Vec<u64>> {
        let parts: Vec<&str> = repo.split('/').collect();
        if parts.len() != 2 {
            anyhow::bail!("invalid repo format: expected 'owner/repo', got '{repo}'");
        }
        let (owner, repo_name) = (parts[0], parts[1]);
        let number = match parse_positive_issue_number(number) {
            Ok(number) => number,
            Err(e) => {
                tracing::debug!(repo, issue = number, ?e, "skipping sub-issues lookup");
                return Ok(Vec::new());
            }
        };

        let mut all_numbers: Vec<u64> = Vec::new();
        let mut cursor: Option<String> = None;
        let page_size = 100;
        let max_pages = 50;
        let mut page_count = 0;

        loop {
            page_count += 1;
            if page_count > max_pages {
                tracing::warn!(
                    repo,
                    number,
                    "get_sub_issues hit max page limit ({max_pages})"
                );
                break;
            }

            let (query, vars) = if let Some(ref c) = cursor {
                (
                    r"query($owner: String!, $name: String!, $number: Int!, $first: Int!, $after: String!) {
                    repository(owner: $owner, name: $name) {
                        issue(number: $number) {
                            subIssues(first: $first, after: $after) {
                                nodes { number }
                                pageInfo { hasNextPage endCursor }
                            }
                        }
                    }
                }",
                    serde_json::json!({
                        "owner": owner,
                        "name": repo_name,
                        "number": number,
                        "first": page_size,
                        "after": c,
                    }),
                )
            } else {
                (
                    r"query($owner: String!, $name: String!, $number: Int!, $first: Int!) {
                    repository(owner: $owner, name: $name) {
                        issue(number: $number) {
                            subIssues(first: $first) {
                                nodes { number }
                                pageInfo { hasNextPage endCursor }
                            }
                        }
                    }
                }",
                    serde_json::json!({
                        "owner": owner,
                        "name": repo_name,
                        "number": number,
                        "first": page_size,
                    }),
                )
            };

            let result = self
                .graphql_with_headers_and_vars(query, &["GraphQL-Features:sub_issues"], vars)
                .await?;

            let sub_issues_data = result
                .get("data")
                .and_then(|d| d.get("repository"))
                .and_then(|r| r.get("issue"))
                .and_then(|i| i.get("subIssues"));

            let nodes = sub_issues_data
                .and_then(|s| s.get("nodes"))
                .and_then(|n| n.as_array());

            match nodes {
                Some(nodes) => {
                    let numbers: Vec<u64> = nodes
                        .iter()
                        .filter_map(|n| n.get("number").and_then(serde_json::Value::as_u64))
                        .collect();
                    all_numbers.extend(numbers);
                }
                None => break,
            }

            let page_info = sub_issues_data.and_then(|s| s.get("pageInfo"));
            let has_next = page_info
                .and_then(|p| p.get("hasNextPage"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);

            if !has_next {
                break;
            }

            cursor = page_info
                .and_then(|p| p.get("endCursor"))
                .and_then(|c| c.as_str())
                .map(ToString::to_string);
        }

        Ok(all_numbers)
    }
}

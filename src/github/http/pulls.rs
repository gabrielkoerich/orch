use super::{GITHUB_API, GhHttp, is_full_commit_sha, merge_pr_body, redact_posted_body};
use crate::github::types::GitHubUser;
use crate::github::types::{GitHubComment, GitHubPullRequest, GitHubReview, GitHubReviewComment};
use reqwest::header;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fmt::Write as _;
use urlencoding;

impl GhHttp {
    /// Whether a merged PR exists whose head ref is `branch`.
    ///
    /// The closed-PR list can include a PR for a different branch, so this
    /// uses [`Self::get_closed_pr_state`], which skips a mismatched `head.ref`
    pub async fn is_pr_merged(&self, repo: &str, branch: &str) -> anyhow::Result<bool> {
        Ok(self
            .get_closed_pr_state(repo, branch)
            .await?
            .is_some_and(|(_, merged, _)| merged))
    }

    /// Get the state of a closed PR for the given branch.
    ///
    /// Returns `Some((merged, state))` if a closed PR exists for the branch,
    /// where `merged` indicates if it was merged and `state` is the PR state ("closed" or "merged").
    /// Returns `None` if no closed PR exists for the branch.
    /// Get the state of a closed PR for a given branch.
    ///
    /// Returns `Some((pr_number, merged, state))` if a closed PR is found that
    /// matches the expected branch. Returns `None` if no matching closed PR is found.
    ///
    /// Validates that the returned PR's head ref matches the expected branch to
    /// prevent incorrect merge decisions when multiple PRs exist for the same branch.
    pub async fn get_closed_pr_state(
        &self,
        repo: &str,
        branch: &str,
    ) -> anyhow::Result<Option<(u64, bool, String)>> {
        let owner = repo
            .split('/')
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {repo}"))?;
        let head = format!("{owner}:{branch}");

        let url = format!("{}/repos/{repo}/pulls", self.api_root());
        let prs: Vec<serde_json::Value> = self
            .get_with_query(
                &url,
                &[("head", &head), ("state", "closed"), ("per_page", "5")],
            )
            .await?;

        if prs.is_empty() {
            return Ok(None);
        }

        // Find the first PR where head ref actually matches our branch
        // This validates that we're working with the correct PR
        for pr in prs {
            let pr_branch = pr
                .get("head")
                .and_then(|h| h.get("ref"))
                .and_then(|r| r.as_str())
                .unwrap_or("");

            if pr_branch != branch {
                tracing::debug!(
                    repo,
                    branch,
                    pr_branch,
                    "skipping PR with mismatched head ref"
                );
                continue;
            }

            let pr_number = pr
                .get("number")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("PR missing number field"))?;

            let merged_at = pr.get("merged_at");
            let merged = merged_at.is_some_and(|v| !v.is_null());
            let state = pr
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("closed")
                .to_string();

            return Ok(Some((pr_number, merged, state)));
        }

        // No PR with matching head ref found
        Ok(None)
    }

    /// Batch-check whether the PR for each branch was merged via a single GraphQL query.
    ///
    /// Returns a map of branch → merged. Branches with no matching PR are omitted
    /// (callers should treat missing entries as not merged).
    pub async fn batch_is_pr_merged_by_branch(
        &self,
        repo: &str,
        branches: &[String],
    ) -> anyhow::Result<HashMap<String, bool>> {
        if branches.is_empty() {
            return Ok(HashMap::new());
        }

        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {repo}"))?;

        // Build one alias per branch using positional index to avoid GraphQL
        // alias restrictions on branch names that contain non-identifier chars.
        // Branch names are passed via variables ($b0, $b1, …) to avoid injection.
        let mut var_defs = String::new();
        let mut aliases = String::new();
        let mut vars = serde_json::json!({ "owner": owner, "name": name });
        for (i, branch) in branches.iter().enumerate() {
            let _ = write!(var_defs, ", $b{i}: String!");
            let _ = write!(
                aliases,
                r"b{i}: pullRequests(headRefName: $b{i}, states: [MERGED], last: 1) {{ nodes {{ merged }} }} "
            );
            vars[format!("b{i}")] = serde_json::Value::String(branch.clone());
        }

        let query = format!(
            r"query($owner: String!, $name: String!{var_defs}) {{
                repository(owner: $owner, name: $name) {{ {aliases} }}
            }}"
        );

        let resp = self.graphql_with_vars(&query, vars).await?;

        // Surface GraphQL partial errors before checking data structure
        if let Some(errors) = resp.get("errors").and_then(|e| e.as_array())
            && !errors.is_empty()
        {
            let messages: Vec<&str> = errors
                .iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect();
            tracing::warn!(
                repo,
                errors = ?messages,
                "GraphQL returned errors in batch PR merge check"
            );
        }

        Self::parse_batch_pr_merged_response(&resp, branches)
    }

    pub(super) fn parse_batch_pr_merged_response(
        resp: &serde_json::Value,
        branches: &[String],
    ) -> anyhow::Result<HashMap<String, bool>> {
        let repo_data = resp.pointer("/data/repository").ok_or_else(|| {
            // Check for errors array to provide a more helpful error message
            let error_msg = resp
                .get("errors")
                .and_then(|e| e.as_array())
                .and_then(|arr| arr.first())
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("no error details");
            anyhow::anyhow!("missing /data/repository in GraphQL response: {error_msg}")
        })?;

        let mut result = HashMap::new();
        for (i, branch) in branches.iter().enumerate() {
            let alias = format!("b{i}");
            let merged = repo_data
                .get(&alias)
                .and_then(|v| v.get("nodes"))
                .and_then(|nodes| nodes.as_array())
                .and_then(|arr| arr.first())
                .and_then(|node| node.get("merged"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            result.insert(branch.clone(), merged);
        }

        Ok(result)
    }

    /// Get PR number by branch name.
    pub async fn get_pr_number(&self, repo: &str, branch: &str) -> anyhow::Result<Option<u64>> {
        let owner = repo
            .split('/')
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {repo}"))?;
        let head = format!("{owner}:{branch}");

        let url = format!("{GITHUB_API}/repos/{repo}/pulls");
        let prs: Vec<serde_json::Value> = self
            .get_with_query(
                &url,
                &[("head", &head), ("state", "open"), ("per_page", "1")],
            )
            .await?;

        if prs.is_empty() {
            return Ok(None);
        }
        prs[0]
            .get("number")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("PR missing number field"))
            .map(Some)
    }

    /// Create a new pull request.
    ///
    /// # Arguments
    /// * `repo` - Repository in "owner/repo" format
    /// * `title` - PR title
    /// * `body` - PR body/description
    /// * `head` - Branch name to create PR from
    /// * `base` - Base branch to merge into (e.g., "main")
    ///
    /// Returns the created PR's URL on success.
    pub async fn create_pr(
        &self,
        repo: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> anyhow::Result<String> {
        #[derive(Serialize)]
        struct CreatePullRequest<'a> {
            title: &'a str,
            body: &'a str,
            head: &'a str,
            base: &'a str,
        }

        let body = redact_posted_body(body);
        let url = format!("{}/repos/{repo}/pulls", self.api_root());
        Self::check_backoff(&url)?;

        let payload = CreatePullRequest {
            title,
            body: &body,
            head,
            base,
        };

        let auth = self.auth_header(&url).await?;
        let make_req = || {
            Ok(self
                .client
                .post(&url)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header(header::USER_AGENT, "orch")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .json(&payload))
        };
        let response = self.send_with_retries(make_req, false).await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            Self::maybe_record_rate_limit_from_body(status, &text);
            anyhow::bail!("GitHub API POST {url} failed ({status}): {text}");
        }

        let pr: serde_json::Value = serde_json::from_str(&text)?;
        pr.get("html_url")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("failed to get PR URL from response: {pr:?}"))
    }

    /// Get the repository's default branch from the GitHub API.
    ///
    /// Used as a fallback when local git detection yields an invalid base
    /// branch for PR creation.
    pub async fn get_default_branch(&self, repo: &str) -> anyhow::Result<String> {
        let url = format!("{}/repos/{repo}", self.api_root());
        let resp: serde_json::Value = self.get_json(&url).await?;
        resp.get("default_branch")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("default_branch missing from repo response"))
    }

    /// Get PR details by PR number.
    /// Returns the full PR object including the `mergeable` field.
    pub async fn get_pr(&self, repo: &str, pr_number: u64) -> anyhow::Result<GitHubPullRequest> {
        let url = format!("{}/repos/{repo}/pulls/{pr_number}", self.api_root());
        self.get_json(&url).await
    }

    /// Fetch the merged/state for multiple PRs in a single GraphQL query.
    ///
    /// Returns a map of pr_number → (merged, state).
    /// PRs that fail to fetch are silently omitted from the result.
    pub async fn batch_get_pr_states(
        &self,
        repo: &str,
        pr_numbers: &[u64],
    ) -> anyhow::Result<HashMap<u64, (bool, String)>> {
        // GitHub GraphQL complexity limit: chunk at 50 to avoid slow/rejected queries.
        const CHUNK_SIZE: usize = 50;

        if pr_numbers.is_empty() {
            return Ok(HashMap::new());
        }

        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {repo}"))?;

        let mut result = HashMap::new();

        for chunk in pr_numbers.chunks(CHUNK_SIZE) {
            let aliases: String = chunk
                .iter()
                .map(|n| format!("pr{n}: pullRequest(number: {n}) {{ merged state }}"))
                .collect::<Vec<_>>()
                .join("\n  ");

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
                if let Some(pr) = repo_data.get(format!("pr{n}")) {
                    let merged = pr
                        .get("merged")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let state = pr
                        .get("state")
                        .and_then(|v| v.as_str())
                        .unwrap_or("UNKNOWN")
                        .to_lowercase();
                    result.insert(*n, (merged, state));
                }
            }
        }

        Ok(result)
    }

    /// SHA of the head commit on `branch`
    pub async fn get_branch_sha(&self, repo: &str, branch: &str) -> anyhow::Result<String> {
        let encoded = urlencoding::encode(branch);
        let url = format!("{}/repos/{repo}/branches/{encoded}", self.api_root());
        let resp: serde_json::Value = self.get_json(&url).await?;
        resp.pointer("/commit/sha")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("branch {branch} has no commit sha"))
    }

    /// Get reviews for a PR.
    pub async fn get_pr_reviews(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> anyhow::Result<Vec<GitHubReview>> {
        let url = format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}/reviews");
        self.get_all_pages(&url, &[("per_page", "100")]).await
    }

    /// Get the committer timestamp for a specific commit SHA.
    ///
    /// Returns an ISO-8601 string, e.g. `"2024-01-15T12:00:00Z"`.
    /// Used to determine whether the branch has been updated since a review was submitted.
    pub async fn get_commit_timestamp(&self, repo: &str, sha: &str) -> anyhow::Result<String> {
        let url = format!("{GITHUB_API}/repos/{repo}/commits/{sha}");
        let value: serde_json::Value = self.get_json(&url).await?;
        value
            .pointer("/commit/committer/date")
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .ok_or_else(|| anyhow::anyhow!("commit {sha} has no committer date"))
    }

    /// Update the body of a pull request.
    pub async fn update_pr_body(
        &self,
        repo: &str,
        pr_number: u64,
        body: &str,
    ) -> anyhow::Result<()> {
        let body = redact_posted_body(body);
        let url = format!("{}/repos/{repo}/pulls/{pr_number}", self.api_root());
        let payload = serde_json::json!({ "body": body });
        self.patch_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Close a pull request without merging it.
    pub async fn close_pr(&self, repo: &str, pr_number: u64) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}");
        let payload = serde_json::json!({ "state": "closed" });
        self.patch_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// How many commits the head is missing from `base` (GitHub's `behind_by`).
    ///
    /// Zero means the head already contains the current base tip, so its checks
    /// ran against the code the merge would produce.
    pub async fn compare_behind_by(
        &self,
        repo: &str,
        base: &str,
        head_sha: &str,
    ) -> anyhow::Result<u64> {
        let url = format!("{GITHUB_API}/repos/{repo}/compare/{base}...{head_sha}");
        let body: serde_json::Value = self.get_json(&url).await?;
        parse_behind_by(&body)
    }

    /// Parents, author and committer of a commit
    pub async fn commit_meta(&self, repo: &str, sha: &str) -> anyhow::Result<CommitMeta> {
        let url = format!("{GITHUB_API}/repos/{repo}/commits/{sha}");
        let value: serde_json::Value = self.get_json(&url).await?;
        Ok(parse_commit_meta(&value))
    }

    /// True when `branch` already contains the commit `sha`
    pub async fn branch_contains(
        &self,
        repo: &str,
        branch: &str,
        sha: &str,
    ) -> anyhow::Result<bool> {
        let url = format!("{GITHUB_API}/repos/{repo}/compare/{branch}...{sha}");
        let body: serde_json::Value = self.get_json(&url).await?;
        body.get("ahead_by")
            .and_then(serde_json::Value::as_u64)
            .map(|ahead| ahead == 0)
            .ok_or_else(|| anyhow::anyhow!("compare response has no numeric ahead_by field"))
    }

    /// Merge a PR using the REST API (squash merge).
    /// GitHub rejects it when the PR head is no longer `head_sha`, the commit CI was checked against
    pub async fn merge_pr(
        &self,
        repo: &str,
        pr_number: u64,
        delete_branch: bool,
        head_sha: &str,
    ) -> anyhow::Result<()> {
        if !is_full_commit_sha(head_sha) {
            anyhow::bail!("merge_pr requires the full head sha, got {head_sha:?}");
        }
        let url = format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}/merge");
        Self::check_backoff(&url)?;
        let payload = merge_pr_body(head_sha);
        let auth = self.auth_header(&url).await?;
        let make_req = || {
            Ok(self
                .client
                .put(&url)
                .json(&payload)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub PR merge failed ({status}): {body}");
        }
        Self::record_success();

        // Delete branch if requested
        if delete_branch {
            // Get the head ref (branch name) from the PR
            let pr: serde_json::Value = self
                .get_json(&format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}"))
                .await?;
            if let Some(ref_name) = pr.pointer("/head/ref").and_then(|v| v.as_str()) {
                let del_url = format!("{GITHUB_API}/repos/{repo}/git/refs/heads/{ref_name}");
                if let Err(e) = self.delete(&del_url).await {
                    let err_str = e.to_string();
                    if err_str.contains("Reference does not exist") {
                        tracing::debug!(
                            ref_name,
                            "branch already gone (GitHub auto-deleted on merge)"
                        );
                    } else {
                        tracing::warn!(ref_name, err = %e, "failed to delete branch after merge");
                    }
                }
            }
        }
        Ok(())
    }

    /// Update a PR's branch by merging the latest base branch into it
    /// (equivalent of the "Update branch" button on GitHub, or `gh pr
    /// update-branch`). Produces a new merge commit on the head branch,
    /// which re-triggers CI — used to give a PR a chance to pick up a fix
    /// that landed on the base branch after the PR's last CI run failed.
    pub async fn update_pr_branch(&self, repo: &str, pr_number: u64) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}/update-branch");
        Self::check_backoff(&url)?;
        let auth = self.auth_header(&url).await?;
        let make_req = || {
            Ok(self
                .client
                .put(&url)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &body);
            anyhow::bail!("GitHub PR update-branch failed ({status}): {body}");
        }
        Self::record_success();
        Ok(())
    }

    /// Enable auto-merge on a PR via GraphQL.
    ///
    /// GitHub will automatically merge the PR once all required checks pass.
    pub async fn enable_auto_merge(&self, repo: &str, pr_number: u64) -> anyhow::Result<()> {
        let parts: Vec<&str> = repo.split('/').collect();
        if parts.len() != 2 {
            anyhow::bail!("invalid repo format: {repo}");
        }
        let (owner, repo_name) = (parts[0], parts[1]);

        // First get the PR node ID
        let pr_query = r"query($owner: String!, $name: String!, $number: Int!) {
            repository(owner: $owner, name: $name) {
                pullRequest(number: $number) { id }
            }
        }";
        let body = self
            .graphql_with_vars(
                pr_query,
                serde_json::json!({
                    "owner": owner,
                    "name": repo_name,
                    "number": pr_number,
                }),
            )
            .await?;
        let pr_id = body
            .pointer("/data/repository/pullRequest/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("could not get PR node ID"))?
            .to_string();

        // Enable auto-merge with squash
        let mutation = r"mutation($prId: ID!) {
            enablePullRequestAutoMerge(input: {pullRequestId: $prId, mergeMethod: SQUASH}) {
                pullRequest { autoMergeRequest { enabledAt } }
            }
        }";
        let body = self
            .graphql_with_vars(mutation, serde_json::json!({ "prId": pr_id }))
            .await?;
        if let Some(errors) = body.get("errors") {
            anyhow::bail!("enable auto-merge GraphQL error: {errors}");
        }

        tracing::info!(repo, pr_number, "auto-merge enabled on PR");
        Ok(())
    }

    /// Fetch reviews, review comments, and issue comments for multiple PRs in a
    /// single GraphQL query instead of 4–5 REST calls per PR.
    ///
    /// Returns a map of `pr_number → PrReviewBatchData`.
    /// PRs missing from the response are omitted from the result.
    pub async fn batch_fetch_pr_review_data(
        &self,
        repo: &str,
        pr_numbers: &[u64],
    ) -> anyhow::Result<HashMap<u64, PrReviewBatchData>> {
        if pr_numbers.is_empty() {
            return Ok(HashMap::new());
        }

        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {repo}"))?;

        // Build one GraphQL alias per PR number.
        let aliases: String = pr_numbers
            .iter()
            .map(|&n| build_pr_review_alias(n))
            .collect::<Vec<_>>()
            .join("\n");

        let query = format!(
            r"query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {aliases} }} }}"
        );

        let resp = self
            .graphql_with_vars(&query, serde_json::json!({ "owner": owner, "name": name }))
            .await?;
        let repo_data = resp
            .pointer("/data/repository")
            .ok_or_else(|| anyhow::anyhow!("missing /data/repository in GraphQL response"))?;

        let mut result = HashMap::new();
        for &n in pr_numbers {
            let Some(pr_data) = repo_data.get(format!("pr{n}")) else {
                continue;
            };

            let merged = pr_data
                .get("merged")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let mergeable = match pr_data.get("mergeable").and_then(|v| v.as_str()) {
                Some("MERGEABLE") => Some(true),
                Some("CONFLICTING") => Some(false),
                _ => None, // UNKNOWN or missing
            };

            let mut reviews: Vec<GitHubReview> = pr_data
                .pointer("/reviews/nodes")
                .and_then(|v| v.as_array())
                .map(|nodes| nodes.iter().filter_map(parse_graphql_review).collect())
                .unwrap_or_default();

            if connection_has_next_page(pr_data.pointer("/reviews/pageInfo")) {
                tracing::warn!(
                    pr_number = n,
                    fetched = reviews.len(),
                    "batch review fetch truncated reviews connection at first page"
                );
            }

            // Ensure deterministic ordering by submitted timestamp (oldest -> newest).
            // GraphQL connections do not guarantee a stable sort unless explicitly
            // requested. Sort here so consumers can rely on chronological order.
            reviews.sort_by(|a, b| a.submitted_at.cmp(&b.submitted_at));

            let mut review_comments: Vec<GitHubReviewComment> = pr_data
                .pointer("/reviewThreads/nodes")
                .and_then(|v| v.as_array())
                .map(|threads| {
                    threads
                        .iter()
                        .filter_map(|thread| {
                            thread
                                .pointer("/comments/nodes")
                                .and_then(|v| v.as_array())
                                .map(|nodes| {
                                    nodes
                                        .iter()
                                        .filter_map(parse_graphql_review_comment)
                                        .collect::<Vec<_>>()
                                })
                        })
                        .flatten()
                        .collect()
                })
                .unwrap_or_default();

            if connection_has_next_page(pr_data.pointer("/reviewThreads/pageInfo")) {
                tracing::warn!(
                    pr_number = n,
                    fetched_threads = pr_data
                        .pointer("/reviewThreads/nodes")
                        .and_then(|v| v.as_array())
                        .map_or(0, Vec::len),
                    "batch review fetch truncated reviewThreads connection at first page"
                );
            }
            if pr_data
                .pointer("/reviewThreads/nodes")
                .and_then(|v| v.as_array())
                .is_some_and(|threads| {
                    threads.iter().any(|thread| {
                        connection_has_next_page(thread.pointer("/comments/pageInfo"))
                    })
                })
            {
                tracing::warn!(
                    pr_number = n,
                    "batch review fetch truncated one or more review thread comment connections"
                );
            }

            // Sort inline review comments by created_at (oldest -> newest) so
            // callers can process them deterministically.
            review_comments.sort_by(|a, b| a.created_at.cmp(&b.created_at));

            let mut issue_comments: Vec<GitHubComment> = pr_data
                .pointer("/comments/nodes")
                .and_then(|v| v.as_array())
                .map(|nodes| {
                    nodes
                        .iter()
                        .filter_map(parse_graphql_issue_comment)
                        .collect()
                })
                .unwrap_or_default();

            if let Some(cursor) = connection_end_cursor(pr_data.pointer("/comments/pageInfo")) {
                issue_comments.extend(
                    self.fetch_pr_issue_comments_after(owner, name, n, Some(cursor))
                        .await?,
                );
            }

            // Sort issue-level comments by created_at (oldest -> newest). The
            // review poll logic expects chronological ordering and may iterate
            // in reverse to find the latest comments — so ensure a consistent
            // ordering here regardless of GraphQL's default.
            issue_comments.sort_by(|a, b| a.created_at.cmp(&b.created_at));

            result.insert(
                n,
                PrReviewBatchData {
                    merged,
                    head_sha: pr_data
                        .get("headRefOid")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    mergeable,
                    reviews,
                    review_comments,
                    issue_comments,
                },
            );
        }

        Ok(result)
    }

    async fn fetch_pr_issue_comments_after(
        &self,
        owner: &str,
        name: &str,
        pr_number: u64,
        mut cursor: Option<String>,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        let mut comments = Vec::new();
        let mut page_count = 0;
        let max_pages = 50;

        while let Some(current_cursor) = cursor.take() {
            page_count += 1;
            if page_count > max_pages {
                tracing::warn!(
                    pr_number,
                    fetched = comments.len(),
                    "stopping PR issue comment pagination after max page limit"
                );
                break;
            }

            let (query, vars) =
                build_pr_issue_comments_page_query(owner, name, pr_number, &current_cursor);
            let resp = self.graphql_with_vars(&query, vars).await?;
            let comments_data = resp
                .pointer("/data/repository/pullRequest/comments")
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "missing /data/repository/pullRequest/comments in GraphQL response"
                    )
                })?;

            comments.extend(
                comments_data
                    .get("nodes")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(parse_graphql_issue_comment),
            );

            cursor = connection_end_cursor(comments_data.get("pageInfo"));
        }

        Ok(comments)
    }
}

/// Read `behind_by` out of a compare response.
///
/// A missing field is an error, not zero: treating an unreadable response as
/// "head is up to date" would let a stale PR merge
/// A commit's parents, first parent first, and the GitHub accounts behind it
#[derive(Debug, Default, PartialEq)]
pub struct CommitMeta {
    pub parents: Vec<String>,
    pub author_login: Option<String>,
    pub committer_login: Option<String>,
}

pub(super) fn parse_commit_meta(commit: &serde_json::Value) -> CommitMeta {
    let login = |field: &str| {
        commit
            .pointer(&format!("/{field}/login"))
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    CommitMeta {
        parents: commit
            .get("parents")
            .and_then(|p| p.as_array())
            .map(|parents| {
                parents
                    .iter()
                    .filter_map(|p| p.get("sha").and_then(|s| s.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        author_login: login("author"),
        committer_login: login("committer"),
    }
}

pub(super) fn parse_behind_by(body: &serde_json::Value) -> anyhow::Result<u64> {
    body.get("behind_by")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("compare response has no numeric behind_by field"))
}

/// Data fetched for one PR by [`GhHttp::batch_fetch_pr_review_data`].
#[derive(Debug, Clone, Default)]
pub struct PrReviewBatchData {
    /// True when the pull request was merged.
    pub merged: bool,
    /// Head commit of the pull request, empty when GitHub did not return it.
    pub head_sha: String,
    /// `Some(true)` = MERGEABLE, `Some(false)` = CONFLICTING, `None` = UNKNOWN.
    pub mergeable: Option<bool>,
    /// All reviews submitted on the PR.
    pub reviews: Vec<GitHubReview>,
    /// All inline review comments (from all review threads).
    pub review_comments: Vec<GitHubReviewComment>,
    /// All issue-level comments on the PR.
    pub issue_comments: Vec<GitHubComment>,
}

/// Build a GraphQL alias fragment for one pull request, fetching all data
/// needed by the review poll loop in a single batch request.
pub(super) fn build_pr_review_alias(n: u64) -> String {
    format!(
        r"pr{n}: pullRequest(number: {n}) {{
  merged
  mergeable
  headRefOid
  reviews(first: 50) {{
    nodes {{
      databaseId
      author {{ login }}
      body
      state
      url
      submittedAt
      commit {{ oid }}
    }}
    pageInfo {{ hasNextPage }}
  }}
  reviewThreads(first: 50) {{
    nodes {{
      comments(first: 20) {{
        nodes {{
          databaseId
          author {{ login }}
          body
          path
          line
          originalLine
          commit {{ oid }}
          originalCommit {{ oid }}
          url
          createdAt
          updatedAt
          replyTo {{ databaseId }}
          diffHunk
        }}
        pageInfo {{ hasNextPage }}
      }}
    }}
    pageInfo {{ hasNextPage }}
  }}
  comments(first: 50) {{
    nodes {{
      databaseId
      author {{ login }}
      body
      createdAt
      updatedAt
      url
    }}
    pageInfo {{ hasNextPage endCursor }}
  }}
}}"
    )
}

pub(super) fn build_pr_issue_comments_page_query(
    owner: &str,
    name: &str,
    pr_number: u64,
    after_cursor: &str,
) -> (String, serde_json::Value) {
    let query = r"query($owner: String!, $name: String!, $number: Int!, $after: String!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      comments(first: 100, after: $after) {
        nodes {
          databaseId
          author { login }
          body
          createdAt
          updatedAt
          url
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"
    .to_string();
    let vars = serde_json::json!({
        "owner": owner,
        "name": name,
        "number": pr_number,
        "after": after_cursor,
    });
    (query, vars)
}

fn connection_has_next_page(page_info: Option<&serde_json::Value>) -> bool {
    page_info
        .and_then(|v| v.get("hasNextPage"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

pub(super) fn connection_end_cursor(page_info: Option<&serde_json::Value>) -> Option<String> {
    if !connection_has_next_page(page_info) {
        return None;
    }

    page_info
        .and_then(|v| v.get("endCursor"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

fn parse_graphql_review(node: &serde_json::Value) -> Option<GitHubReview> {
    Some(GitHubReview {
        id: node.get("databaseId").and_then(serde_json::Value::as_u64)?,
        user: GitHubUser {
            login: node
                .pointer("/author/login")
                .and_then(|v| v.as_str())?
                .to_string(),
        },
        body: node.get("body").and_then(|v| v.as_str()).map(String::from),
        state: node.get("state").and_then(|v| v.as_str())?.to_string(),
        html_url: node.get("url").and_then(|v| v.as_str()).map(String::from),
        submitted_at: node
            .get("submittedAt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        commit_id: node
            .pointer("/commit/oid")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

fn parse_graphql_review_comment(node: &serde_json::Value) -> Option<GitHubReviewComment> {
    Some(GitHubReviewComment {
        id: node.get("databaseId").and_then(serde_json::Value::as_u64)?,
        user: GitHubUser {
            login: node
                .pointer("/author/login")
                .and_then(|v| v.as_str())?
                .to_string(),
        },
        body: node.get("body").and_then(|v| v.as_str())?.to_string(),
        path: node
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        line: node
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
        original_line: node
            .get("originalLine")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
        commit_id: node
            .pointer("/commit/oid")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        original_commit_id: node
            .pointer("/originalCommit/oid")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        html_url: node
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        created_at: node
            .get("createdAt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        updated_at: node
            .get("updatedAt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        in_reply_to_id: node
            .pointer("/replyTo/databaseId")
            .and_then(serde_json::Value::as_u64),
        diff_hunk: node
            .get("diffHunk")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

fn parse_graphql_issue_comment(node: &serde_json::Value) -> Option<GitHubComment> {
    Some(GitHubComment {
        id: node.get("databaseId").and_then(serde_json::Value::as_u64)?,
        body: node.get("body").and_then(|v| v.as_str())?.to_string(),
        user: GitHubUser {
            login: node
                .pointer("/author/login")
                .and_then(|v| v.as_str())?
                .to_string(),
        },
        created_at: node
            .get("createdAt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        updated_at: node
            .get("updatedAt")
            .and_then(|v| v.as_str())
            .map(String::from),
        html_url: node.get("url").and_then(|v| v.as_str()).map(String::from),
        issue_url: None,
        author_association: None,
    })
}

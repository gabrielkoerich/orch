//! `gh` CLI wrapper — structured args in, serde out.
//!
//! All GitHub API calls go through `gh api`. Auth is handled by `gh`.
//! We build the command args in Rust and deserialize the JSON output via serde.

use super::types::{GitHubComment, GitHubIssue};
use tokio::process::Command;

pub struct GhCli;

impl GhCli {
    pub fn new() -> Self {
        Self
    }

    /// Run `gh api` with args and return raw JSON bytes.
    async fn api(&self, args: &[&str]) -> anyhow::Result<Vec<u8>> {
        let output = Command::new("gh").arg("api").args(args).output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Ok(output.stdout)
    }

    /// Check `gh auth status`.
    pub async fn auth_status(&self) -> anyhow::Result<()> {
        let output = Command::new("gh").args(["auth", "status"]).output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh auth failed: {stderr}");
        }
        Ok(())
    }

    /// Create a GitHub issue.
    ///
    /// Uses `--input -` with a JSON payload for safe multiline body handling.
    pub async fn create_issue(
        &self,
        repo: &str,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> anyhow::Result<GitHubIssue> {
        let endpoint = format!("repos/{repo}/issues");
        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "labels": labels,
        });
        let output = Command::new("gh")
            .arg("api")
            .args([&endpoint, "-X", "POST", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        // Write payload to stdin
        let mut child = output;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    /// Get a single issue.
    pub async fn get_issue(&self, repo: &str, number: &str) -> anyhow::Result<GitHubIssue> {
        let endpoint = format!("repos/{repo}/issues/{number}");
        let json = self.api(&[&endpoint]).await?;
        Ok(serde_json::from_slice(&json)?)
    }

    /// List issues filtered by a label.
    ///
    /// Uses `--paginate` to fetch all pages (not just the first 100).
    pub async fn list_issues(&self, repo: &str, label: &str) -> anyhow::Result<Vec<GitHubIssue>> {
        let endpoint = format!("repos/{repo}/issues");
        let labels_field = format!("labels={label}");
        let output = Command::new("gh")
            .arg("api")
            .arg("--paginate")
            .args([
                &endpoint,
                "-X",
                "GET",
                "-f",
                &labels_field,
                "-f",
                "state=open",
                "-f",
                "per_page=100",
            ])
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    /// Add labels to an issue (appends to existing labels).
    ///
    /// Uses `--input -` with a JSON payload — the `-f labels[]=` form doesn't
    /// map correctly to the GitHub JSON API.
    #[allow(dead_code)] // available for backends, not all paths used yet
    pub async fn add_labels(
        &self,
        repo: &str,
        number: &str,
        labels: &[String],
    ) -> anyhow::Result<()> {
        let endpoint = format!("repos/{repo}/issues/{number}/labels");
        let payload = serde_json::json!({ "labels": labels });
        let mut child = Command::new("gh")
            .arg("api")
            .args([&endpoint, "-X", "POST", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Ok(())
    }

    /// Replace all labels on an issue atomically (PUT).
    ///
    /// This is a single API call — no window where labels are missing.
    pub async fn replace_labels(
        &self,
        repo: &str,
        number: &str,
        labels: &[String],
    ) -> anyhow::Result<()> {
        let endpoint = format!("repos/{repo}/issues/{number}/labels");
        let payload = serde_json::json!({ "labels": labels });
        let mut child = Command::new("gh")
            .arg("api")
            .args([&endpoint, "-X", "PUT", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Ok(())
    }

    /// Ensure a label exists on the repo, creating it if it does not.
    ///
    /// Mirrors the bash `_gh_ensure_label()`: GET the label; on 404 (or any
    /// error) POST to create it.  Failures on creation are tolerated — the
    /// caller is never blocked by a missing label (same as `|| true` in bash).
    pub async fn ensure_label(
        &self,
        repo: &str,
        name: &str,
        color: &str,
        description: &str,
    ) -> anyhow::Result<()> {
        let encoded = urlencoding::encode(name);
        let get_endpoint = format!("repos/{repo}/labels/{encoded}");

        // Check whether the label already exists.
        match self.api(&[&get_endpoint]).await {
            Ok(_) => return Ok(()), // label exists — nothing to do
            Err(e) if e.to_string().contains("404") => {
                tracing::debug!(repo, name, "ensure_label: label not found, creating");
            }
            Err(e) => {
                // Any other GET error — log and attempt creation anyway.
                tracing::warn!(repo, name, err = %e, "ensure_label: GET failed, attempting create");
            }
        }

        // Create the label.
        let create_endpoint = format!("repos/{repo}/labels");
        let payload = serde_json::json!({
            "name": name,
            "color": color,
            "description": description,
        });
        let mut child = Command::new("gh")
            .arg("api")
            .args([&create_endpoint, "-X", "POST", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // 422 means the label already exists (race condition) — not an error.
            if stderr.contains("422")
                || stderr.contains("already_exists")
                || stderr.contains("Unprocessable")
            {
                tracing::debug!(repo, name, "ensure_label: label already exists (race)");
                return Ok(());
            }
            // All other creation failures are tolerated (matches bash `|| true`).
            tracing::warn!(repo, name, stderr = %stderr, "ensure_label: create failed, continuing");
        }
        Ok(())
    }

    /// Remove a label from an issue.
    ///
    /// Returns Ok if the label was removed or didn't exist (404).
    /// Propagates all other errors to prevent silent state corruption.
    #[allow(dead_code)]
    pub async fn remove_label(&self, repo: &str, number: &str, label: &str) -> anyhow::Result<()> {
        let encoded = urlencoding::encode(label);
        let endpoint = format!("repos/{repo}/issues/{number}/labels/{encoded}");
        match self.api(&[&endpoint, "-X", "DELETE"]).await {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("404") => {
                tracing::debug!(repo, number, label, "label already removed (404)");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Add a comment to an issue.
    ///
    /// Uses `--input -` for safe multiline body handling.
    pub async fn add_comment(&self, repo: &str, number: &str, body: &str) -> anyhow::Result<()> {
        let endpoint = format!("repos/{repo}/issues/{number}/comments");
        let payload = serde_json::json!({ "body": body });
        let mut child = Command::new("gh")
            .arg("api")
            .args([&endpoint, "-X", "POST", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Ok(())
    }

    /// List comments on an issue.
    #[allow(dead_code)]
    pub async fn list_comments(
        &self,
        repo: &str,
        number: &str,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        let endpoint = format!("repos/{repo}/issues/{number}/comments");
        let json = self.api(&[&endpoint]).await?;
        Ok(serde_json::from_slice(&json)?)
    }
}

/// Return the hex color string for a given `status:*` label.
///
/// Matches the bash `_gh_status_color()` palette so labels created from Rust
/// look the same as those created by the shell orchestrator.
pub fn status_label_color(label: &str) -> &'static str {
    match label {
        "status:new" => "0e8a16",
        "status:routed" => "1d76db",
        "status:in_progress" => "fbca04",
        "status:done" => "0e8a16",
        "status:blocked" => "d73a4a",
        "status:in_review" => "0075ca",
        "status:needs_review" => "e4e669",
        _ => "c5def5",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_colors_match_bash_palette() {
        assert_eq!(status_label_color("status:new"), "0e8a16");
        assert_eq!(status_label_color("status:routed"), "1d76db");
        assert_eq!(status_label_color("status:in_progress"), "fbca04");
        assert_eq!(status_label_color("status:done"), "0e8a16");
        assert_eq!(status_label_color("status:blocked"), "d73a4a");
        assert_eq!(status_label_color("status:in_review"), "0075ca");
        assert_eq!(status_label_color("status:needs_review"), "e4e669");
    }

    #[test]
    fn unknown_label_gets_default_color() {
        assert_eq!(status_label_color("agent:claude"), "c5def5");
        assert_eq!(status_label_color("enhancement"), "c5def5");
        assert_eq!(status_label_color(""), "c5def5");
    }
}

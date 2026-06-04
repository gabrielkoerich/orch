//! Notification types — task completion events broadcast to all channels.
//!
//! When a task completes, the engine constructs a `TaskNotification` and
//! pushes it through the transport. A dispatcher in the engine loop sends
//! the notification to all registered channels (Telegram, Discord, etc.).
//!
//! The notification level (`all`, `errors_only`, `none`) controls which
//! events are broadcast. Configured via `notifications.level` in config.

use serde::{Deserialize, Serialize};

/// Notification level — controls which task events are broadcast.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    /// Broadcast review-active and terminal completions only: `done`,
    /// `in_review`, `blocked`, `failed`. Intermediate transitions (`new`,
    /// `routed`, `in_progress`, `needs_review`) are suppressed.
    All,
    /// Broadcast every status transition, including intermediate states.
    Verbose,
    /// Only broadcast errors (`blocked`, `failed`).
    ErrorsOnly,
    /// Disable notifications entirely.
    None,
}

impl NotificationLevel {
    /// Load from config key `notifications.level`, defaulting to `all`.
    pub fn from_config() -> Self {
        match crate::config::get("notifications.level") {
            Ok(val) => match val.as_str() {
                "all" => Self::All,
                "verbose" => Self::Verbose,
                "errors_only" => Self::ErrorsOnly,
                "none" => Self::None,
                _ => Self::All,
            },
            Err(_) => Self::All,
        }
    }

    /// Whether a notification with this status should be sent.
    ///
    /// `needs_review` is intentionally excluded from `All` and `ErrorsOnly`:
    /// it's the agent-handoff signal that always pairs with a follow-up
    /// `in_review` (or `blocked`/`failed`) within seconds, so notifying on it
    /// produces a redundant ping. The review-active states (`in_review`,
    /// `blocked`, `failed`) are the ones humans actually act on.
    pub fn should_notify(&self, status: &str) -> bool {
        match self {
            Self::All => matches!(status, "done" | "in_review" | "blocked" | "failed"),
            Self::Verbose => true,
            Self::ErrorsOnly => matches!(status, "blocked" | "failed"),
            Self::None => false,
        }
    }
}

/// A task completion notification to broadcast to all channels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNotification {
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub agent: String,
    pub duration_seconds: f64,
    pub summary: String,
    /// Repository (owner/repo) this task belongs to, for multi-project routing.
    pub repo: Option<String>,
    /// Optional override chat_id for Telegram (e.g. from job `notify_target`).
    /// When set, the notification is sent directly to this chat_id instead of
    /// the channel routing logic.
    #[serde(default)]
    pub notify_target: Option<String>,
    /// PR number if a pull request is associated with the task.
    #[serde(default)]
    pub pr_number: Option<String>,
}

/// Build the GitHub URL associated with this task, preferring the PR link
/// when available and falling back to the issue link for external tasks.
fn github_link(repo: Option<&str>, task_id: &str, pr_number: Option<&str>) -> Option<String> {
    let repo = repo?;
    if let Some(pr) = pr_number {
        return Some(format!("https://github.com/{repo}/pull/{pr}"));
    }
    // External task IDs are bare issue numbers; internal IDs start with "internal:".
    if !task_id.starts_with("internal:") && task_id.chars().all(|c| c.is_ascii_digit()) {
        return Some(format!("https://github.com/{repo}/issues/{task_id}"));
    }
    None
}

impl TaskNotification {
    /// Format for Telegram (HTML).
    ///
    /// Use HTML parse mode to avoid fragile Markdown escaping issues. Values are
    /// escaped for HTML entities (e.g. <, >, &) and simple tags are used for
    /// bold/monospace formatting.
    pub fn format_telegram(&self) -> String {
        let emoji = status_emoji(&self.status);
        let duration = format_duration(self.duration_seconds);
        let link_suffix = match github_link(
            self.repo.as_deref(),
            &self.task_id,
            self.pr_number.as_deref(),
        ) {
            Some(url) => format!("\n\n<a href=\"{}\">View on GitHub</a>", html_escape(&url)),
            None => String::new(),
        };

        format!(
            "{emoji} <b>{title}</b>\n\
             {status} · <code>{agent}</code> · {duration} · #{task_id}\n\
             \n\
             {summary}{link_suffix}",
            emoji = emoji,
            title = html_escape(&self.title),
            status = html_escape(&self.status),
            agent = html_escape(&self.agent),
            duration = duration,
            task_id = html_escape(&self.task_id),
            summary = html_escape(&self.summary),
            link_suffix = link_suffix,
        )
    }

    /// Format for Slack (mrkdwn).
    ///
    /// Slack uses its own "mrkdwn" dialect: *bold*, _italic_, `code`.
    pub fn format_slack(&self) -> String {
        let emoji = status_emoji(&self.status);
        let duration = format_duration(self.duration_seconds);
        let link_suffix = match github_link(
            self.repo.as_deref(),
            &self.task_id,
            self.pr_number.as_deref(),
        ) {
            Some(url) => format!("\n\n<{url}|View on GitHub>"),
            None => String::new(),
        };

        format!(
            "{emoji} *{title}*\n\
             {status} · `{agent}` · {duration} · #{task_id}\n\
             \n\
             {summary}{link_suffix}",
            emoji = emoji,
            title = self.title,
            status = self.status,
            agent = self.agent,
            duration = duration,
            task_id = self.task_id,
            summary = self.summary,
            link_suffix = link_suffix,
        )
    }

    /// Format for Discord (Markdown with bold instead of Telegram-style).
    pub fn format_discord(&self) -> String {
        let emoji = status_emoji(&self.status);
        let duration = format_duration(self.duration_seconds);
        let link_suffix = match github_link(
            self.repo.as_deref(),
            &self.task_id,
            self.pr_number.as_deref(),
        ) {
            Some(url) => format!("\n\n[View on GitHub]({url})"),
            None => String::new(),
        };

        format!(
            "{emoji} **{title}**\n\
             {status} · `{agent}` · {duration} · #{task_id}\n\
             \n\
             {summary}{link_suffix}",
            emoji = emoji,
            title = self.title,
            status = self.status,
            agent = self.agent,
            duration = duration,
            task_id = self.task_id,
            summary = self.summary,
            link_suffix = link_suffix,
        )
    }

    /// Format with project prefix (for General/subscribed channels).
    ///
    /// Prepends `[project_name]` to the channel-specific formatted message
    /// so recipients of multi-project channels can identify the source.
    #[allow(dead_code)]
    pub fn format_with_project(&self, channel: &str) -> String {
        let project_name = self
            .repo
            .as_deref()
            .and_then(|r| r.split('/').next_back())
            .unwrap_or("unknown");

        let base = match channel {
            "telegram" => self.format_telegram(),
            "discord" => self.format_discord(),
            "slack" => self.format_slack(),
            _ => self.format_telegram(), // fallback
        };

        let project_name = if channel == "telegram" {
            html_escape(project_name)
        } else {
            project_name.to_string()
        };
        format!("[{project_name}] {base}")
    }
}

fn status_emoji(status: &str) -> &'static str {
    match status {
        "done" => "✅",
        "in_review" => "🔍",
        "in_progress" => "🔄",
        "needs_review" => "⚠️",
        "blocked" => "🚫",
        "failed" => "❌",
        _ => "📋",
    }
}

/// Escape text for HTML to be sent in Telegram HTML parse_mode.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Format seconds into human-readable duration.
pub fn format_duration(seconds: f64) -> String {
    if seconds < 60.0 {
        format!("{:.0}s", seconds)
    } else if seconds < 3600.0 {
        format!("{:.0}m {:.0}s", (seconds / 60.0).floor(), seconds % 60.0)
    } else {
        let hours = (seconds / 3600.0).floor();
        let mins = ((seconds % 3600.0) / 60.0).floor();
        format!("{:.0}h {:.0}m", hours, mins)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_level_should_notify_all_active_review_only() {
        let level = NotificationLevel::All;
        // Active states that warrant a ping — should notify
        assert!(level.should_notify("done"));
        assert!(level.should_notify("in_review"));
        assert!(level.should_notify("blocked"));
        assert!(level.should_notify("failed"));
        // needs_review is suppressed: it's a redundant handoff signal that
        // always pairs with a follow-up in_review/blocked/failed event.
        assert!(!level.should_notify("needs_review"));
        // Other intermediate states — should NOT notify
        assert!(!level.should_notify("new"));
        assert!(!level.should_notify("routed"));
        assert!(!level.should_notify("in_progress"));
    }

    #[test]
    fn notification_level_should_notify_verbose_fires_for_all() {
        let level = NotificationLevel::Verbose;
        assert!(level.should_notify("done"));
        assert!(level.should_notify("needs_review"));
        assert!(level.should_notify("blocked"));
        assert!(level.should_notify("failed"));
        assert!(level.should_notify("new"));
        assert!(level.should_notify("routed"));
        assert!(level.should_notify("in_progress"));
        assert!(level.should_notify("in_review"));
    }

    #[test]
    fn notification_level_should_notify_errors_only() {
        let level = NotificationLevel::ErrorsOnly;
        assert!(!level.should_notify("done"));
        assert!(!level.should_notify("in_progress"));
        assert!(!level.should_notify("in_review"));
        assert!(!level.should_notify("needs_review"));
        assert!(level.should_notify("blocked"));
        assert!(level.should_notify("failed"));
    }

    #[test]
    fn notification_level_should_notify_none() {
        let level = NotificationLevel::None;
        assert!(!level.should_notify("done"));
        assert!(!level.should_notify("needs_review"));
        assert!(!level.should_notify("in_review"));
        assert!(!level.should_notify("blocked"));
    }

    #[test]
    fn notification_level_default_is_all() {
        // When config is missing, default to All
        let level = NotificationLevel::from_config();
        assert_eq!(level, NotificationLevel::All);
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(30.0), "30s");
        assert_eq!(format_duration(0.0), "0s");
        assert_eq!(format_duration(59.9), "60s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(90.0), "1m 30s");
        assert_eq!(format_duration(3599.0), "59m 59s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(3600.0), "1h 0m");
        assert_eq!(format_duration(7200.0), "2h 0m");
        assert_eq!(format_duration(5400.0), "1h 30m");
    }

    #[test]
    fn format_telegram_done() {
        let n = TaskNotification {
            task_id: "42".to_string(),
            title: "Fix auth bug".to_string(),
            status: "done".to_string(),
            agent: "claude".to_string(),
            duration_seconds: 120.0,
            summary: "Fixed the OAuth flow".to_string(),
            repo: Some("owner/my-project".to_string()),
            notify_target: None,
            pr_number: None,
        };
        let msg = n.format_telegram();
        assert!(msg.contains("✅"));
        // Title-first layout: title is the headline, metadata line carries the id.
        assert!(
            msg.contains("<b>Fix auth bug</b>"),
            "title should be the headline: {msg}"
        );
        assert!(msg.contains("#42"));
        assert!(msg.contains("done"));
        assert!(msg.contains("claude"));
        assert!(msg.contains("2m 0s"));
        assert!(msg.contains("Fixed the OAuth flow"));
    }

    #[test]
    fn format_telegram_escapes_markdown() {
        let n = TaskNotification {
            task_id: "1".to_string(),
            title: "Fix _underscores_ and *bold*".to_string(),
            status: "done".to_string(),
            agent: "claude".to_string(),
            duration_seconds: 10.0,
            summary: "Done".to_string(),
            repo: None,
            notify_target: None,
            pr_number: None,
        };
        let msg = n.format_telegram();
        // HTML mode: underscores and asterisks are literal characters; ensure
        // HTML escaping did not mangle them and that title is present.
        assert!(msg.contains("Fix _underscores_ and *bold*"));
        // Ensure HTML tags are present for bold title
        assert!(msg.contains("<b>Fix _underscores_ and *bold*</b>"));
    }

    #[test]
    fn format_telegram_escapes_status_with_underscores() {
        let n = TaskNotification {
            task_id: "5".to_string(),
            title: "Test task".to_string(),
            status: "needs_review".to_string(),
            agent: "claude".to_string(),
            duration_seconds: 30.0,
            summary: "Ready for review".to_string(),
            repo: None,
            notify_target: None,
            pr_number: None,
        };
        let msg = n.format_telegram();
        // In HTML mode underscores are literal; ensure status is present and
        // included without backslashes.
        assert!(msg.contains("needs_review"));
    }

    #[test]
    fn format_telegram_escapes_agent_with_underscores() {
        let n = TaskNotification {
            task_id: "6".to_string(),
            title: "Test".to_string(),
            status: "done".to_string(),
            agent: "my_agent".to_string(),
            duration_seconds: 15.0,
            summary: "Done".to_string(),
            repo: None,
            notify_target: None,
            pr_number: None,
        };
        let msg = n.format_telegram();
        // HTML mode: underscores are literal in text and agent is enclosed in
        // <code> tags.
        assert!(msg.contains("<code>my_agent</code>"));
    }

    #[test]
    fn format_slack_done() {
        let n = TaskNotification {
            task_id: "7".to_string(),
            title: "Refactor engine".to_string(),
            status: "done".to_string(),
            agent: "claude".to_string(),
            duration_seconds: 60.0,
            summary: "Decomposed mod.rs".to_string(),
            repo: Some("org/refactor-repo".to_string()),
            notify_target: None,
            pr_number: None,
        };
        let msg = n.format_slack();
        assert!(msg.contains("✅"));
        assert!(
            msg.contains("*Refactor engine*"),
            "title should be the headline: {msg}"
        );
        assert!(msg.contains("#7"));
        assert!(msg.contains("done"));
        assert!(msg.contains("`claude`"));
        assert!(msg.contains("1m 0s"));
        assert!(msg.contains("Decomposed mod.rs"));
    }

    #[test]
    fn format_discord_needs_review() {
        let n = TaskNotification {
            task_id: "99".to_string(),
            title: "Deploy service".to_string(),
            status: "needs_review".to_string(),
            agent: "codex".to_string(),
            duration_seconds: 1800.0,
            summary: "Timed out waiting for tests".to_string(),
            repo: Some("acme/deploy-svc".to_string()),
            notify_target: None,
            pr_number: None,
        };
        let msg = n.format_discord();
        assert!(msg.contains("⚠️"));
        assert!(
            msg.contains("**Deploy service**"),
            "title should be the headline: {msg}"
        );
        assert!(msg.contains("#99"));
        assert!(msg.contains("needs_review"));
        assert!(msg.contains("codex"));
        assert!(msg.contains("30m 0s"));
    }

    #[test]
    fn format_with_project_prepends_repo_name() {
        let n = TaskNotification {
            task_id: "10".to_string(),
            title: "Add feature".to_string(),
            status: "done".to_string(),
            agent: "claude".to_string(),
            duration_seconds: 45.0,
            summary: "Feature added".to_string(),
            repo: Some("acme/widgets".to_string()),
            notify_target: None,
            pr_number: None,
        };
        let msg = n.format_with_project("telegram");
        assert!(msg.starts_with("[widgets] "));
        assert!(msg.contains("#10"));
        assert!(msg.contains("<b>Add feature</b>"));
    }

    #[test]
    fn format_with_project_unknown_when_no_repo() {
        let n = TaskNotification {
            task_id: "11".to_string(),
            title: "Task".to_string(),
            status: "done".to_string(),
            agent: "claude".to_string(),
            duration_seconds: 5.0,
            summary: "Done".to_string(),
            repo: None,
            notify_target: None,
            pr_number: None,
        };
        let msg = n.format_with_project("discord");
        assert!(msg.starts_with("[unknown] "));
    }

    #[test]
    fn status_emoji_mapping() {
        assert_eq!(status_emoji("done"), "✅");
        assert_eq!(status_emoji("in_review"), "🔍");
        assert_eq!(status_emoji("in_progress"), "🔄");
        assert_eq!(status_emoji("needs_review"), "⚠️");
        assert_eq!(status_emoji("blocked"), "🚫");
        assert_eq!(status_emoji("failed"), "❌");
        assert_eq!(status_emoji("unknown"), "📋");
    }

    #[test]
    fn format_with_project_includes_repo_name() {
        let n = TaskNotification {
            task_id: "42".into(),
            title: "Fix bug".into(),
            status: "done".into(),
            agent: "claude".into(),
            duration_seconds: 90.0,
            summary: "Fixed it".into(),
            repo: Some("owner/myproject".into()),
            notify_target: None,
            pr_number: None,
        };
        let formatted = n.format_with_project("telegram");
        assert!(
            formatted.starts_with("[myproject]"),
            "should start with project name: {formatted}"
        );
        assert!(formatted.contains("Fix bug"));
    }

    #[test]
    fn format_with_project_no_repo_still_works() {
        let n = TaskNotification {
            task_id: "42".into(),
            title: "Fix bug".into(),
            status: "done".into(),
            agent: "claude".into(),
            duration_seconds: 90.0,
            summary: "Fixed it".into(),
            repo: None,
            notify_target: None,
            pr_number: None,
        };
        let formatted = n.format_with_project("telegram");
        assert!(
            formatted.starts_with("[unknown]"),
            "should fallback: {formatted}"
        );
    }

    #[test]
    fn title_appears_before_metadata_in_telegram() {
        let n = TaskNotification {
            task_id: "internal:151694".to_string(),
            title: "Morning briefing: daily focus plan".to_string(),
            status: "done".to_string(),
            agent: "opencode".to_string(),
            duration_seconds: 1821.0,
            summary: "Recap delivered".to_string(),
            repo: Some("owner/repo".to_string()),
            notify_target: None,
            pr_number: None,
        };
        let msg = n.format_telegram();
        let title_pos = msg.find("Morning briefing").expect("title present");
        let id_pos = msg.find("#internal:151694").expect("id present");
        assert!(
            title_pos < id_pos,
            "title must appear before task id: {msg}"
        );
    }

    #[test]
    fn title_appears_before_metadata_in_discord() {
        let n = TaskNotification {
            task_id: "internal:5".to_string(),
            title: "Deploy svc".to_string(),
            status: "done".to_string(),
            agent: "codex".to_string(),
            duration_seconds: 60.0,
            summary: "ok".to_string(),
            repo: Some("acme/svc".to_string()),
            notify_target: None,
            pr_number: None,
        };
        let msg = n.format_discord();
        let title_pos = msg.find("Deploy svc").expect("title present");
        let id_pos = msg.find("#internal:5").expect("id present");
        assert!(title_pos < id_pos, "title before id: {msg}");
    }

    #[test]
    fn format_with_project_uses_channel_format() {
        let n = TaskNotification {
            task_id: "5".into(),
            title: "Deploy".into(),
            status: "done".into(),
            agent: "codex".into(),
            duration_seconds: 60.0,
            summary: "Deployed".into(),
            repo: Some("acme/svc".into()),
            notify_target: None,
            pr_number: None,
        };
        let tg = n.format_with_project("telegram");
        let dc = n.format_with_project("discord");
        // Both start with project prefix
        assert!(tg.starts_with("[svc]"));
        assert!(dc.starts_with("[svc]"));
        // Discord uses ** for bold, telegram uses HTML <b> tags — title is the headline now.
        assert!(dc.contains("**Deploy**"));
        assert!(tg.contains("<b>Deploy</b>"));
        // Task id appears on the metadata line.
        assert!(tg.contains("#5"));
        assert!(dc.contains("#5"));
    }

    #[test]
    fn telegram_appends_pr_link_when_pr_number_present() {
        let n = TaskNotification {
            task_id: "internal:151696".to_string(),
            title: "Market intelligence".to_string(),
            status: "done".to_string(),
            agent: "claude".to_string(),
            duration_seconds: 39.0,
            summary: "Trending topics".to_string(),
            repo: Some("owner/repo".to_string()),
            notify_target: None,
            pr_number: Some("4242".to_string()),
        };
        let msg = n.format_telegram();
        assert!(
            msg.contains("https://github.com/owner/repo/pull/4242"),
            "expected PR link in {msg}"
        );
        assert!(msg.contains("View on GitHub"));
    }

    #[test]
    fn telegram_falls_back_to_issue_link_for_external_task_without_pr() {
        let n = TaskNotification {
            task_id: "1234".to_string(),
            title: "An issue".to_string(),
            status: "needs_review".to_string(),
            agent: "claude".to_string(),
            duration_seconds: 10.0,
            summary: "Awaiting review".to_string(),
            repo: Some("owner/repo".to_string()),
            notify_target: None,
            pr_number: None,
        };
        let msg = n.format_telegram();
        assert!(
            msg.contains("https://github.com/owner/repo/issues/1234"),
            "expected issue link in {msg}"
        );
    }

    #[test]
    fn no_link_for_internal_task_without_pr() {
        let n = TaskNotification {
            task_id: "internal:99".to_string(),
            title: "Internal task".to_string(),
            status: "done".to_string(),
            agent: "claude".to_string(),
            duration_seconds: 5.0,
            summary: "Done".to_string(),
            repo: Some("owner/repo".to_string()),
            notify_target: None,
            pr_number: None,
        };
        let msg = n.format_telegram();
        assert!(!msg.contains("github.com"), "should not link: {msg}");
    }

    #[test]
    fn discord_appends_pr_link() {
        let n = TaskNotification {
            task_id: "internal:7".to_string(),
            title: "T".to_string(),
            status: "done".to_string(),
            agent: "claude".to_string(),
            duration_seconds: 1.0,
            summary: "S".to_string(),
            repo: Some("owner/repo".to_string()),
            notify_target: None,
            pr_number: Some("9".to_string()),
        };
        let msg = n.format_discord();
        assert!(msg.contains("[View on GitHub](https://github.com/owner/repo/pull/9)"));
    }

    #[test]
    fn slack_appends_pr_link() {
        let n = TaskNotification {
            task_id: "internal:7".to_string(),
            title: "T".to_string(),
            status: "done".to_string(),
            agent: "claude".to_string(),
            duration_seconds: 1.0,
            summary: "S".to_string(),
            repo: Some("owner/repo".to_string()),
            notify_target: None,
            pr_number: Some("9".to_string()),
        };
        let msg = n.format_slack();
        assert!(msg.contains("<https://github.com/owner/repo/pull/9|View on GitHub>"));
    }
}

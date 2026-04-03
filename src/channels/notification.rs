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
    /// Broadcast terminal/meaningful completions only: done, needs_review, blocked, failed.
    /// Intermediate transitions (new, routed, in_progress, in_review) are suppressed.
    /// This restores pre-event-bus behavior.
    All,
    /// Broadcast every status transition, including intermediate states.
    Verbose,
    /// Only broadcast errors (needs_review, blocked, failed).
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
    pub fn should_notify(&self, status: &str) -> bool {
        match self {
            Self::All => matches!(status, "done" | "needs_review" | "blocked" | "failed"),
            Self::Verbose => true,
            Self::ErrorsOnly => {
                matches!(status, "needs_review" | "blocked" | "failed")
            }
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

        format!(
            "{emoji} <b>Task #{task_id}</b>: {status}\n\
             <b>{title}</b>\n\
             Agent: <code>{agent}</code> | Duration: {duration}\n\
             \n\
             {summary}",
            emoji = emoji,
            task_id = html_escape(&self.task_id),
            status = html_escape(&self.status),
            title = html_escape(&self.title),
            agent = html_escape(&self.agent),
            duration = duration,
            summary = html_escape(&self.summary),
        )
    }

    /// Format for Slack (mrkdwn).
    ///
    /// Slack uses its own "mrkdwn" dialect: *bold*, _italic_, `code`.
    pub fn format_slack(&self) -> String {
        let emoji = status_emoji(&self.status);
        let duration = format_duration(self.duration_seconds);

        format!(
            "{emoji} *Task #{task_id}*: {status}\n\
             *{title}*\n\
             Agent: `{agent}` | Duration: {duration}\n\
             \n\
             {summary}",
            emoji = emoji,
            task_id = self.task_id,
            status = self.status,
            title = self.title,
            agent = self.agent,
            duration = duration,
            summary = self.summary,
        )
    }

    /// Format for Discord (Markdown with bold instead of Telegram-style).
    pub fn format_discord(&self) -> String {
        let emoji = status_emoji(&self.status);
        let duration = format_duration(self.duration_seconds);

        format!(
            "{emoji} **Task #{task_id}**: {status}\n\
             **{title}**\n\
             Agent: `{agent}` | Duration: {duration}\n\
             \n\
             {summary}",
            emoji = emoji,
            task_id = self.task_id,
            status = self.status,
            title = self.title,
            agent = self.agent,
            duration = duration,
            summary = self.summary,
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
    fn notification_level_should_notify_all_terminal_only() {
        let level = NotificationLevel::All;
        // Terminal/meaningful states — should notify
        assert!(level.should_notify("done"));
        assert!(level.should_notify("needs_review"));
        assert!(level.should_notify("blocked"));
        assert!(level.should_notify("failed"));
        // Intermediate states — should NOT notify
        assert!(!level.should_notify("new"));
        assert!(!level.should_notify("routed"));
        assert!(!level.should_notify("in_progress"));
        assert!(!level.should_notify("in_review"));
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
        assert!(level.should_notify("needs_review"));
        assert!(level.should_notify("blocked"));
        assert!(level.should_notify("failed"));
    }

    #[test]
    fn notification_level_should_notify_none() {
        let level = NotificationLevel::None;
        assert!(!level.should_notify("done"));
        assert!(!level.should_notify("needs_review"));
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
        };
        let msg = n.format_telegram();
        assert!(msg.contains("✅"));
        assert!(msg.contains("Task #42"));
        assert!(msg.contains("done"));
        assert!(msg.contains("Fix auth bug"));
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
        };
        let msg = n.format_slack();
        assert!(msg.contains("✅"));
        assert!(msg.contains("*Task #7*"));
        assert!(msg.contains("done"));
        assert!(msg.contains("Refactor engine"));
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
        };
        let msg = n.format_discord();
        assert!(msg.contains("⚠️"));
        assert!(msg.contains("**Task #99**"));
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
        };
        let msg = n.format_with_project("telegram");
        assert!(msg.starts_with("[widgets] "));
        assert!(msg.contains("Task #10"));
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
        };
        let formatted = n.format_with_project("telegram");
        assert!(
            formatted.starts_with("[unknown]"),
            "should fallback: {formatted}"
        );
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
        };
        let tg = n.format_with_project("telegram");
        let dc = n.format_with_project("discord");
        // Both start with project prefix
        assert!(tg.starts_with("[svc]"));
        assert!(dc.starts_with("[svc]"));
        // Discord uses ** for bold, telegram uses HTML <b> tags
        assert!(dc.contains("**Task #5**"));
        assert!(tg.contains("<b>Task #5</b>"));
    }
}

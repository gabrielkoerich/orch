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
    /// Broadcast all task completions.
    All,
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
            Self::All => true,
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
}

impl TaskNotification {
    /// Format for Telegram (Markdown).
    pub fn format_telegram(&self) -> String {
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
            title = escape_markdown(&self.title),
            agent = self.agent,
            duration = duration,
            summary = escape_markdown(&self.summary),
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

/// Escape special Markdown characters for Telegram's MarkdownV1.
fn escape_markdown(s: &str) -> String {
    s.replace('_', "\\_")
        .replace('*', "\\*")
        .replace('[', "\\[")
        .replace('`', "\\`")
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
    fn notification_level_should_notify_all() {
        let level = NotificationLevel::All;
        assert!(level.should_notify("done"));
        assert!(level.should_notify("in_progress"));
        assert!(level.should_notify("needs_review"));
        assert!(level.should_notify("blocked"));
        assert!(level.should_notify("failed"));
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
        };
        let msg = n.format_telegram();
        assert!(msg.contains("Fix \\_underscores\\_ and \\*bold\\*"));
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
        };
        let msg = n.format_discord();
        assert!(msg.contains("⚠️"));
        assert!(msg.contains("**Task #99**"));
        assert!(msg.contains("needs_review"));
        assert!(msg.contains("codex"));
        assert!(msg.contains("30m 0s"));
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
}

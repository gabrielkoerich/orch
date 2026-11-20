//! Sender allow list for channels that take commands from a chat platform.

use std::collections::HashSet;

/// Chats and users allowed to talk to the engine, checked before any message leaves the channel.
/// An empty set on either side rejects everything
#[derive(Debug, Clone, Default)]
pub struct Access {
    chats: HashSet<String>,
    users: HashSet<String>,
}

impl Access {
    pub fn new<C, U>(chats: C, users: U) -> Self
    where
        C: IntoIterator<Item = String>,
        U: IntoIterator<Item = String>,
    {
        Self {
            chats: chats.into_iter().filter(|s| !s.is_empty()).collect(),
            users: users.into_iter().filter(|s| !s.is_empty()).collect(),
        }
    }

    pub fn allows(&self, chat_id: &str, user_id: &str) -> bool {
        !user_id.is_empty() && self.chats.contains(chat_id) && self.users.contains(user_id)
    }

    /// Warn once at startup when the policy drops every incoming message
    pub fn warn_if_closed(&self, channel: &str) {
        if self.chats.is_empty() || self.users.is_empty() {
            tracing::warn!(
                channel,
                "no chat or no allowed_user_ids configured, incoming messages are dropped"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(chats: &[&str], users: &[&str]) -> Access {
        Access::new(
            chats.iter().map(ToString::to_string),
            users.iter().map(ToString::to_string),
        )
    }

    #[test]
    fn allows_listed_chat_and_user() {
        assert!(access(&["1"], &["9"]).allows("1", "9"));
    }

    #[test]
    fn rejects_other_chat_or_user() {
        let a = access(&["1"], &["9"]);
        assert!(!a.allows("2", "9"));
        assert!(!a.allows("1", "8"));
        assert!(!a.allows("1", ""));
    }

    #[test]
    fn empty_policy_rejects_everything() {
        assert!(!access(&[], &[]).allows("1", "9"));
        assert!(!access(&["1"], &[]).allows("1", "9"));
        assert!(!access(&[], &["9"]).allows("1", "9"));
    }
}

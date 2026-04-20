//! Channel-to-project routing map.
//!
//! Built at engine startup from per-project `.orch.yml` configs.
//! Provides reverse lookup: (channel, topic/channel_id) → repo.

use std::collections::HashMap;

/// Per-project channel configuration, read from `.orch.yml`.
#[derive(Debug, Clone, Default)]
pub struct ProjectChannelConfig {
    pub telegram_topic_id: Option<String>,
    #[allow(dead_code)]
    pub telegram_bot_token: Option<String>,
    #[allow(dead_code)]
    pub telegram_chat_id: Option<String>,
    pub discord_channel_id: Option<String>,
    #[allow(dead_code)]
    pub discord_bot_token: Option<String>,
    #[allow(dead_code)]
    pub discord_guild_id: Option<String>,
}

/// Global channel configuration from `~/.orch/config.yml`.
#[derive(Debug, Clone, Default)]
pub struct GlobalChannelConfig {
    pub telegram_general_topic_id: Option<String>,
    pub discord_general_channel_id: Option<String>,
    /// Telegram topic ID for the control session (from `control.channels.telegram.topic_id`).
    pub control_telegram_topic_id: Option<String>,
    /// Discord channel ID for the control session (from `control.channels.discord.channel_id`).
    pub control_discord_channel_id: Option<String>,
}

/// Maps channel targets to projects and vice versa.
pub struct ChannelRouter {
    /// (channel_name, topic_or_channel_id) → repo
    target_to_repo: HashMap<(String, String), String>,
    /// repo → { channel_name: topic_or_channel_id }
    repo_to_targets: HashMap<String, HashMap<String, String>>,
    /// General channel IDs per channel type
    general: HashMap<String, String>,
    /// Control session channel IDs per channel type
    control: HashMap<String, String>,
    /// All configured project repos
    project_list: Vec<String>,
}

impl ChannelRouter {
    /// Build router from global config + per-project configs.
    pub fn new(global: &GlobalChannelConfig, projects: &[(String, ProjectChannelConfig)]) -> Self {
        let mut target_to_repo = HashMap::new();
        let mut repo_to_targets: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut general = HashMap::new();
        let mut control = HashMap::new();
        let mut project_list = Vec::new();

        if let Some(ref id) = global.telegram_general_topic_id {
            general.insert("telegram".to_string(), id.clone());
        }
        if let Some(ref id) = global.discord_general_channel_id {
            general.insert("discord".to_string(), id.clone());
        }
        if let Some(ref id) = global.control_telegram_topic_id {
            control.insert("telegram".to_string(), id.clone());
        }
        if let Some(ref id) = global.control_discord_channel_id {
            control.insert("discord".to_string(), id.clone());
        }

        for (repo, config) in projects {
            project_list.push(repo.clone());
            let mut targets = HashMap::new();

            if let Some(ref topic_id) = config.telegram_topic_id {
                target_to_repo.insert(("telegram".to_string(), topic_id.clone()), repo.clone());
                targets.insert("telegram".to_string(), topic_id.clone());
            }

            if let Some(ref channel_id) = config.discord_channel_id {
                target_to_repo.insert(("discord".to_string(), channel_id.clone()), repo.clone());
                targets.insert("discord".to_string(), channel_id.clone());
            }

            repo_to_targets.insert(repo.clone(), targets);
        }

        Self {
            target_to_repo,
            repo_to_targets,
            general,
            control,
            project_list,
        }
    }

    /// Resolve which project a channel message belongs to.
    /// Returns None if the target isn't mapped (e.g., General channel).
    pub fn resolve_project(&self, channel: &str, topic_or_channel_id: &str) -> Option<&str> {
        self.target_to_repo
            .get(&(channel.to_string(), topic_or_channel_id.to_string()))
            .map(|s| s.as_str())
    }

    /// Check if a target is the General channel.
    pub fn is_general(&self, channel: &str, topic_or_channel_id: &str) -> bool {
        self.general
            .get(channel)
            .map(|id| id == topic_or_channel_id)
            .unwrap_or(false)
    }

    /// Check if a target is the configured control session channel.
    pub fn is_control_channel(&self, channel: &str, topic_or_channel_id: &str) -> bool {
        self.control
            .get(channel)
            .map(|id| id == topic_or_channel_id)
            .unwrap_or(false)
    }

    /// Get the target (topic/channel ID) for a project on a given channel.
    pub fn target_for_project(&self, repo: &str, channel: &str) -> Option<&str> {
        self.repo_to_targets
            .get(repo)
            .and_then(|targets| targets.get(channel))
            .map(|s| s.as_str())
    }

    /// List all configured project repos.
    pub fn projects(&self) -> &[String] {
        &self.project_list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_router() -> ChannelRouter {
        let global = GlobalChannelConfig {
            telegram_general_topic_id: Some("1".to_string()),
            discord_general_channel_id: Some("9999".to_string()),
            ..Default::default()
        };
        let projects = vec![
            (
                "owner/orch".to_string(),
                ProjectChannelConfig {
                    telegram_topic_id: Some("42".to_string()),
                    discord_channel_id: Some("1111".to_string()),
                    ..Default::default()
                },
            ),
            (
                "owner/bean".to_string(),
                ProjectChannelConfig {
                    telegram_topic_id: Some("87".to_string()),
                    discord_channel_id: Some("2222".to_string()),
                    ..Default::default()
                },
            ),
        ];
        ChannelRouter::new(&global, &projects)
    }

    #[test]
    fn resolve_telegram_topic_to_project() {
        let router = test_router();
        assert_eq!(router.resolve_project("telegram", "42"), Some("owner/orch"));
        assert_eq!(router.resolve_project("telegram", "87"), Some("owner/bean"));
        assert_eq!(router.resolve_project("telegram", "99"), None);
    }

    #[test]
    fn resolve_discord_channel_to_project() {
        let router = test_router();
        assert_eq!(
            router.resolve_project("discord", "1111"),
            Some("owner/orch")
        );
        assert_eq!(
            router.resolve_project("discord", "2222"),
            Some("owner/bean")
        );
    }

    #[test]
    fn is_general_channel() {
        let router = test_router();
        assert!(router.is_general("telegram", "1"));
        assert!(router.is_general("discord", "9999"));
        assert!(!router.is_general("telegram", "42"));
        assert!(!router.is_general("discord", "1111"));
    }

    #[test]
    fn is_control_channel() {
        let global = GlobalChannelConfig {
            telegram_general_topic_id: Some("1".to_string()),
            discord_general_channel_id: Some("9999".to_string()),
            control_telegram_topic_id: Some("55".to_string()),
            control_discord_channel_id: Some("77".to_string()),
        };
        let router = ChannelRouter::new(&global, &[]);
        assert!(router.is_control_channel("telegram", "55"));
        assert!(router.is_control_channel("discord", "77"));
        assert!(!router.is_control_channel("telegram", "1")); // general, not control
        assert!(!router.is_control_channel("discord", "9999")); // general, not control
        assert!(!router.is_control_channel("telegram", "42")); // project channel
        assert!(!router.is_control_channel("slack", "55")); // unknown channel
    }

    #[test]
    fn control_channel_unconfigured_returns_false() {
        let router = test_router(); // no control fields set
        assert!(!router.is_control_channel("telegram", "1"));
        assert!(!router.is_control_channel("discord", "9999"));
    }

    #[test]
    fn target_for_project() {
        let router = test_router();
        assert_eq!(
            router.target_for_project("owner/orch", "telegram"),
            Some("42")
        );
        assert_eq!(
            router.target_for_project("owner/bean", "discord"),
            Some("2222")
        );
        assert_eq!(router.target_for_project("owner/unknown", "telegram"), None);
    }

    #[test]
    fn projects_list() {
        let router = test_router();
        assert_eq!(router.projects().len(), 2);
        assert!(router.projects().contains(&"owner/orch".to_string()));
        assert!(router.projects().contains(&"owner/bean".to_string()));
    }

    #[test]
    fn unmapped_target_returns_none() {
        let router = test_router();
        assert_eq!(router.resolve_project("telegram", "1"), None); // General is not a project
        assert_eq!(router.resolve_project("slack", "anything"), None);
    }

    #[test]
    fn project_with_custom_config_doesnt_affect_others() {
        let global = GlobalChannelConfig {
            telegram_general_topic_id: Some("1".into()),
            discord_general_channel_id: None,
            ..Default::default()
        };
        let projects = vec![
            (
                "owner/orch".into(),
                ProjectChannelConfig {
                    telegram_topic_id: Some("42".into()),
                    telegram_bot_token: Some("custom-token".into()),
                    ..Default::default()
                },
            ),
            (
                "owner/bean".into(),
                ProjectChannelConfig {
                    telegram_topic_id: Some("87".into()),
                    ..Default::default()
                },
            ),
        ];
        let router = ChannelRouter::new(&global, &projects);
        // Custom bot_token on orch should not affect bean's routing
        assert_eq!(router.resolve_project("telegram", "42"), Some("owner/orch"));
        assert_eq!(router.resolve_project("telegram", "87"), Some("owner/bean"));
        // Reverse lookup also works
        assert_eq!(
            router.target_for_project("owner/orch", "telegram"),
            Some("42")
        );
        assert_eq!(
            router.target_for_project("owner/bean", "telegram"),
            Some("87")
        );
        // General still resolves
        assert!(router.is_general("telegram", "1"));
        // Projects list includes both
        assert_eq!(router.projects().len(), 2);
    }
}

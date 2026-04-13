//! Webhook server health state, persisted to `~/.orch/webhook_status.json`.
//!
//! The engine writes this file during startup and each health-check cycle.
//! The `orch webhook status` CLI reads it so operators can inspect webhook
//! health without tailing logs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Persisted webhook health state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookStatus {
    /// Whether webhooks are enabled in config.
    pub configured: bool,
    /// Listening port (None when disabled or not yet bound).
    pub port: Option<u16>,
    /// Whether the last health-check ping succeeded.
    pub healthy: bool,
    /// True when the engine has fallen back to polling due to startup failure.
    pub fallback_mode: bool,
    /// UTC timestamp of the last health-check attempt.
    pub last_check_utc: Option<DateTime<Utc>>,
    /// Human-readable reason for the last failure (cleared on recovery).
    pub last_failure_reason: Option<String>,
    /// How many bind attempts were made at startup.
    pub startup_attempts: u32,
}

impl Default for WebhookStatus {
    fn default() -> Self {
        Self {
            configured: false,
            port: None,
            healthy: false,
            fallback_mode: true,
            last_check_utc: None,
            last_failure_reason: None,
            startup_attempts: 0,
        }
    }
}

/// Path to `~/.orch/webhook_status.json`.
pub fn status_path() -> anyhow::Result<PathBuf> {
    Ok(crate::home::orch_home()?.join("webhook_status.json"))
}

impl WebhookStatus {
    /// Persist the status to disk. Non-fatal: logs a warning on write failure.
    pub async fn save(&self) {
        match status_path().and_then(|p| {
            serde_json::to_string_pretty(self)
                .map(|j| (p, j))
                .map_err(anyhow::Error::from)
        }) {
            Ok((p, json)) => {
                if let Err(e) = tokio::fs::write(&p, json).await {
                    tracing::warn!(error = ?e, "failed to persist webhook status");
                }
            }
            Err(e) => tracing::warn!(?e, "failed to persist webhook status"),
        }
    }

    /// Load the last-known status from disk. Returns `None` if the file does
    /// not exist or cannot be parsed.
    pub fn load() -> Option<Self> {
        let path = status_path().ok()?;
        let json = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&json).ok()
    }
}

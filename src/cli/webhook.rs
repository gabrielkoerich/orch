//! `orch webhook` subcommands.

use crate::webhook_status::WebhookStatus;

/// Print the last-known webhook health state from `~/.orch/webhook_status.json`.
pub fn status() -> anyhow::Result<()> {
    match WebhookStatus::load() {
        None => {
            println!("Webhook status: unavailable");
            println!("  (service not running or webhooks have never been configured)");
        }
        Some(s) => {
            let health_str = if s.healthy { "healthy" } else { "unhealthy" };
            let fallback_str = if s.fallback_mode {
                "yes (polling)"
            } else {
                "no"
            };
            let port_str = s
                .port
                .map(|p| p.to_string())
                .unwrap_or_else(|| "—".to_string());
            let last_check = s
                .last_check_utc
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "never".to_string());
            let failure = s.last_failure_reason.as_deref().unwrap_or("none");

            println!("Webhook status:");
            println!("  Configured:       {}", s.configured);
            println!("  Port:             {}", port_str);
            println!("  Health:           {}", health_str);
            println!("  Fallback mode:    {}", fallback_str);
            println!("  Last check (UTC): {}", last_check);
            println!("  Startup attempts: {}", s.startup_attempts);
            println!("  Last failure:     {}", failure);
        }
    }
    Ok(())
}

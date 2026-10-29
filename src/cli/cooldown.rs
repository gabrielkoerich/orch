//! CLI handlers for `orch cooldown list` and `orch cooldown clear`.
//!
//! Cooldowns are persisted to the SQLite KV store so they survive restarts.
//! The `list` command opens the store, hydrates the in-memory map, then
//! prints all active entries — accurate even when the service is not running.

use crate::engine::cooldown;
use std::sync::Arc;

/// List all active agent and model cooldowns.
pub async fn list() -> anyhow::Result<()> {
    let store = Arc::new(crate::cli::init_store().await?);
    // Hydrate in-memory map from persisted KV so we see the full state.
    cooldown::init_cooldown_store(store.clone()).await;

    // Recompute degraded agents so the CLI shows the same skip reasons as the
    // service, including agents held only by the rate-limit threshold
    let router = crate::engine::router::Router::from_config();
    router.refresh_health(&store).await;

    let entries = cooldown::list_all_cooldowns();
    let degraded = cooldown::list_degraded_agents();
    if entries.is_empty() && degraded.is_empty() {
        println!("No active cooldowns.");
        return Ok(());
    }

    let now = chrono::Utc::now().timestamp();
    println!("{:<30} {:<12} REASON", "KEY", "REMAINING");
    println!("{}", "-".repeat(72_usize));
    for (key, until, reason) in &entries {
        let remaining_secs = until - now;
        let remaining = format_remaining(remaining_secs);
        println!("{:<30} {:<12} {}", key, remaining, reason);
    }
    for (agent, reason) in degraded {
        if entries.iter().any(|(key, ..)| key == &agent) {
            continue;
        }
        println!("{:<30} {:<12} {}", agent, "degraded", reason);
    }
    Ok(())
}

/// Clear a specific cooldown by key, or all cooldowns with `--all`.
pub async fn clear(key: Option<String>, all: bool) -> anyhow::Result<()> {
    let store = Arc::new(crate::cli::init_store().await?);
    // Hydrate first so clear operates on the current state.
    cooldown::init_cooldown_store(store.clone()).await;

    if all {
        let agents = crate::engine::router::Router::from_config().available_agents;
        cooldown::clear_cooldown("*", &store).await;
        cooldown::mark_rate_limits_cleared(&store, &agents).await;
        println!("Cleared all cooldowns.");
    } else if let Some(k) = key {
        cooldown::clear_cooldown(&k, &store).await;
        println!("Cleared cooldown: {k}");
    } else {
        anyhow::bail!("specify a key (e.g. 'claude' or 'claude:sonnet') or use --all");
    }
    Ok(())
}

fn format_remaining(secs: i64) -> String {
    if secs <= 0 {
        return "expired".to_string();
    }
    if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
    } else {
        let d = secs / 86400;
        let h = (secs % 86400) / 3600;
        if h == 0 {
            format!("{d}d")
        } else {
            format!("{d}d{h}h")
        }
    }
}

//! CLI handlers for `orch cooldown list` and `orch cooldown clear`.
//!
//! Cooldowns are persisted to the SQLite KV store so they survive restarts.
//! The `list` command opens the store, hydrates the in-memory map, then
//! prints all active entries — accurate even when the service is not running.

use crate::cli;
use crate::engine::cooldown;
use crate::engine::router::Router;
use std::sync::Arc;

/// List all active agent and model cooldowns.
pub async fn list() -> anyhow::Result<()> {
    let store = Arc::new(cli::init_store().await?);
    // Hydrate in-memory map from persisted KV so we see the full state.
    cooldown::init_cooldown_store(Arc::clone(&store)).await;

    // Recompute degraded agents so the CLI shows the same skip reasons as the
    // service, including agents held only by the rate-limit threshold
    let router = Router::from_config();
    router.refresh_health(&store).await;

    let entries = cooldown::list_all_cooldowns();
    let degraded = cooldown::list_degraded_agents();
    let windows = cooldown::list_rate_limit_windows(&store).await;
    let now = chrono::Utc::now().timestamp();
    if entries.is_empty() && degraded.is_empty() {
        println!("No active cooldowns.");
        print_windows(&windows, now);
        return Ok(());
    }

    println!("{:<30} {:<12} REASON", "KEY", "REMAINING");
    println!("{}", "-".repeat(72_usize));
    for (key, until, reason) in &entries {
        let remaining_secs = until - now;
        let remaining = format_remaining(remaining_secs);
        println!("{key:<30} {remaining:<12} {reason}");
    }
    for (agent, reason) in degraded {
        if entries.iter().any(|(key, ..)| key == &agent) {
            continue;
        }
        println!("{:<30} {:<12} {}", agent, "degraded", reason);
    }
    print_windows(&windows, now);
    Ok(())
}

fn print_windows(windows: &[(String, cooldown::RateLimitWindow)], now: i64) {
    if windows.is_empty() {
        return;
    }
    println!();
    println!(
        "{:<12} {:<12} {:<10} {:<12} RESETS IN",
        "AGENT", "WINDOW", "STATUS", "UTILIZATION"
    );
    for (agent, w) in windows {
        let used = w
            .utilization
            .map_or_else(|| "-".to_string(), |u| format!("{:.0}%", u * 100.0));
        let resets = w
            .resets_at
            .map_or_else(|| "-".to_string(), |at| format_remaining(at - now));
        println!(
            "{agent:<12} {:<12} {:<10} {used:<12} {resets}",
            w.window_type, w.status
        );
    }
}

/// Clear a specific cooldown by key, or all cooldowns with `--all`.
pub async fn clear(key: Option<String>, all: bool) -> anyhow::Result<()> {
    let store = Arc::new(cli::init_store().await?);
    // Hydrate first so clear operates on the current state.
    cooldown::init_cooldown_store(Arc::clone(&store)).await;

    if all {
        let agents = Router::from_config().available_agents;
        cooldown::clear_cooldown("*", &store).await;
        cooldown::mark_rate_limits_cleared(&store, &agents).await;
        println!("Cleared all cooldowns.");
    } else if let Some(k) = key {
        cooldown::clear_cooldown(&k, &store).await;
        println!("Cleared cooldown: {k}");
    } else {
        anyhow::bail!("specify a key (e.g. 'claude' or 'claude:<model>') or use --all");
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

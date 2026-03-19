use crate::config;

/// Show task metrics and statistics, per-project by default.
pub async fn stats(all: bool) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;

    if all {
        let summary = store.get_metrics_summary_24h().await?;
        let cost = store.get_cost_summary().await.ok();
        println!();
        print_summary_table("All Projects", &summary, cost.as_ref());
    } else {
        let repos = get_configured_repos();
        if repos.is_empty() {
            // Fallback to global stats if no projects configured
            let summary = store.get_metrics_summary_24h().await?;
            let cost = store.get_cost_summary().await.ok();
            println!();
            print_summary_table("All Projects", &summary, cost.as_ref());
        } else {
            println!();
            for repo in &repos {
                let summary = store.get_metrics_summary_24h_by_repo(repo).await?;
                // Per-repo cost not yet supported by the store
                print_summary_table(repo, &summary, None);
            }
        }
    }

    Ok(())
}

fn print_summary_table(
    title: &str,
    summary: &crate::store::MetricsSummary,
    cost: Option<&crate::store::CostSummary>,
) {
    println!("── {} ──────────────────────────────────", title);

    let total = summary.tasks_completed_24h + summary.tasks_failed_24h;
    let success_rate = if total > 0 {
        summary.tasks_completed_24h as f64 / total as f64 * 100.0
    } else {
        0.0
    };

    println!(
        "  Tasks (24h):  {} completed, {} failed",
        summary.tasks_completed_24h, summary.tasks_failed_24h
    );
    println!("  Success rate: {:.1}%", success_rate);

    // Duration by complexity
    let mut durations = Vec::new();
    if let Some(d) = summary.avg_duration_simple {
        durations.push(format!("simple {:.1}m", d / 60.0));
    }
    if let Some(d) = summary.avg_duration_medium {
        durations.push(format!("medium {:.1}m", d / 60.0));
    }
    if let Some(d) = summary.avg_duration_complex {
        durations.push(format!("complex {:.1}m", d / 60.0));
    }
    if !durations.is_empty() {
        println!("  Avg duration: {}", durations.join(" | "));
    }

    // Agent stats
    if !summary.agent_stats.is_empty() {
        let agents: Vec<String> = summary
            .agent_stats
            .iter()
            .map(|a| format!("{}: {} ({:.0}%)", a.agent, a.total_runs, a.success_rate))
            .collect();
        println!("  Agents:       {}", agents.join(" | "));
    }

    // Cost: extract 24h period if available
    if let Some(c) = cost {
        if let Some(period_24h) = c.periods.iter().find(|p| p.label == "24h") {
            if period_24h.total_cost_usd > 0.0 {
                println!("  Cost (24h):   ${:.2}", period_24h.total_cost_usd);
            }
        }
    }

    if summary.rate_limits_24h > 0 {
        println!("  Rate limits:  {}", summary.rate_limits_24h);
    }

    println!();
}

/// Get all configured project repos from global config + .orch.yml files.
fn get_configured_repos() -> Vec<String> {
    let mut repos = Vec::new();

    // Try current project
    if let Ok(repo) = config::get_current_repo() {
        if !repo.is_empty() {
            repos.push(repo);
        }
    }

    // Try projects from global config
    if let Ok(paths) = config::get_project_paths() {
        for path_str in &paths {
            let candidate = std::path::PathBuf::from(path_str).join(".orch.yml");
            if candidate.exists() {
                if let Ok(content) = std::fs::read_to_string(&candidate) {
                    if let Ok(val) = serde_yml::from_str::<serde_yml::Value>(&content) {
                        if let Some(repo) = val
                            .get("gh")
                            .and_then(|g| g.get("repo"))
                            .and_then(|r| r.as_str())
                        {
                            if !repos.contains(&repo.to_string()) {
                                repos.push(repo.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    repos
}

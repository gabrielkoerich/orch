use crate::cli;
use crate::cli::task_ref;
use crate::store::{self, TaskStore};
use std::sync::Arc;

/// Show cost breakdown for a specific task.
pub async fn show_task(id: &str) -> anyhow::Result<()> {
    let owned = cli::init_store().await?;
    let resolved = task_ref::resolve(&owned, id).await?;
    let store: Option<Arc<TaskStore>> = Some(Arc::new(owned));
    let repo = resolved.repo.clone();
    let id = resolved.external_id.as_str();

    let usage = store::get_token_usage(&store, &repo, id).await;
    let cost = store::get_reported_cost(&store, &repo, id).await;
    let model = store::opt_store_get_task(&store, &repo, id)
        .await
        .and_then(|t| t.model)
        .unwrap_or_default();

    if usage.total_tokens() == 0 {
        println!("No token data available for task #{id}");
        println!("Run the task first to collect token usage.");
        return Ok(());
    }

    println!("Task #{id}: cost breakdown");
    println!(
        "├── Input tokens:  {:>12}",
        format_tokens(usage.input_tokens)
    );
    println!(
        "├── Output tokens: {:>12}",
        format_tokens(usage.output_tokens)
    );
    println!(
        "└── Cost:          {:>12}",
        format_cost(cost.unwrap_or(0.0))
    );
    println!();
    println!(
        "Model: {}",
        if model.is_empty() { "unknown" } else { &model }
    );

    Ok(())
}

/// Show aggregate cost summary over time periods (24h, 7d, 30d).
pub async fn show_summary() -> anyhow::Result<()> {
    let store = cli::init_store().await?;

    let summary = store.get_cost_summary().await?;

    println!();
    println!("Cost Summary");
    println!("{}", "=".repeat(60));
    println!();
    println!(
        "{:<8} {:>10} {:>10} {:>10} {:>8}",
        "PERIOD", "INPUT", "OUTPUT", "COST", "TASKS"
    );
    println!("{}", "-".repeat(60));

    for period in &summary.periods {
        println!(
            "{:<8} {:>10} {:>10} {:>10} {:>8}",
            period.label,
            format_tokens(u64::try_from(period.input_tokens).unwrap_or(0)),
            format_tokens(u64::try_from(period.output_tokens).unwrap_or(0)),
            format_cost(period.total_cost_usd),
            period.task_count,
        );
    }
    println!();

    Ok(())
}

/// Show cost breakdown by agent.
pub async fn show_by_agent() -> anyhow::Result<()> {
    let store = cli::init_store().await?;

    let groups = store.get_cost_by_agent().await?;

    if groups.is_empty() {
        println!("No cost data available yet.");
        return Ok(());
    }

    println!();
    println!("Cost by Agent");
    println!("{}", "=".repeat(60));
    println!();
    println!(
        "{:<12} {:>10} {:>10} {:>10} {:>8}",
        "AGENT", "INPUT", "OUTPUT", "COST", "TASKS"
    );
    println!("{}", "-".repeat(60));

    let mut total_cost = 0.0;
    for group in &groups {
        total_cost += group.total_cost_usd;
        println!(
            "{:<12} {:>10} {:>10} {:>10} {:>8}",
            group.name,
            format_tokens(u64::try_from(group.input_tokens).unwrap_or(0)),
            format_tokens(u64::try_from(group.output_tokens).unwrap_or(0)),
            format_cost(group.total_cost_usd),
            group.task_count,
        );
    }
    println!("{}", "-".repeat(60));
    println!(
        "{:<12} {:>10} {:>10} {:>10}",
        "",
        "",
        "",
        format_cost(total_cost)
    );
    println!();

    Ok(())
}

/// Show cost breakdown by model.
pub async fn show_by_model() -> anyhow::Result<()> {
    let store = cli::init_store().await?;

    let groups = store.get_cost_by_model().await?;

    if groups.is_empty() {
        println!("No cost data available yet.");
        return Ok(());
    }

    println!();
    println!("Cost by Model");
    println!("{}", "=".repeat(60));
    println!();
    println!(
        "{:<20} {:>10} {:>10} {:>10} {:>8}",
        "MODEL", "INPUT", "OUTPUT", "COST", "TASKS"
    );
    println!("{}", "-".repeat(60));

    let mut total_cost = 0.0;
    for group in &groups {
        total_cost += group.total_cost_usd;
        println!(
            "{:<20} {:>10} {:>10} {:>10} {:>8}",
            group.name,
            format_tokens(u64::try_from(group.input_tokens).unwrap_or(0)),
            format_tokens(u64::try_from(group.output_tokens).unwrap_or(0)),
            format_cost(group.total_cost_usd),
            group.task_count,
        );
    }
    println!("{}", "-".repeat(60));
    println!(
        "{:<20} {:>10} {:>10} {:>10}",
        "",
        "",
        "",
        format_cost(total_cost)
    );
    println!();

    Ok(())
}

/// Format token counts with thousands separators.
/// A cost the agents reported, or "not reported" when none did. orch never estimates one.
pub(crate) fn format_cost(cost_usd: f64) -> String {
    if cost_usd > 0.0 {
        format!("${cost_usd:.4}")
    } else {
        "not reported".to_string()
    }
}

fn format_tokens(n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tokens_zero() {
        assert_eq!(format_tokens(0), "0");
    }

    #[test]
    fn format_tokens_small() {
        assert_eq!(format_tokens(123), "123");
    }

    #[test]
    fn format_tokens_thousands() {
        assert_eq!(format_tokens(1_234), "1,234");
    }

    #[test]
    fn format_tokens_millions() {
        assert_eq!(format_tokens(1_234_567), "1,234,567");
    }
}

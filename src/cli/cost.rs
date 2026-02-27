use crate::db::Db;
use crate::sidecar;

/// Show cost breakdown for a specific task (from sidecar data).
pub fn show_task(id: &str) -> anyhow::Result<()> {
    let usage = sidecar::get_token_usage(id);
    let cost_estimate = sidecar::get_cost_estimate(id);
    let model = sidecar::get_model(id);

    if usage.total_tokens() == 0 {
        println!("No token data available for task #{}", id);
        println!("Run the task first to collect token usage.");
        return Ok(());
    }

    let pricing = sidecar::pricing_for_model(&model);

    println!("Task #{}: cost breakdown", id);
    println!(
        "├── Input tokens:  {:>12}",
        format_tokens(usage.input_tokens)
    );
    println!(
        "├── Output tokens: {:>12}",
        format_tokens(usage.output_tokens)
    );
    println!(
        "├── Input cost:    {:>12}",
        format!("${:.4}", cost_estimate.input_cost_usd)
    );
    println!(
        "├── Output cost:   {:>12}",
        format!("${:.4}", cost_estimate.output_cost_usd)
    );
    println!(
        "└── Total cost:    {:>12}",
        format!("${:.4}", cost_estimate.total_cost_usd)
    );
    println!();
    println!(
        "Model: {}",
        if model.is_empty() { "unknown" } else { &model }
    );
    println!(
        "Pricing: ${:.2}/${:.2} per 1M tokens",
        pricing.input_per_million_usd, pricing.output_per_million_usd
    );

    Ok(())
}

/// Show aggregate cost summary over time periods (24h, 7d, 30d).
pub async fn show_summary() -> anyhow::Result<()> {
    let db = Db::open(&crate::db::default_path()?)?;
    db.migrate().await?;

    let summary = db.get_cost_summary().await?;

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
            format_tokens(period.input_tokens as u64),
            format_tokens(period.output_tokens as u64),
            format!("${:.4}", period.total_cost_usd),
            period.task_count,
        );
    }
    println!();

    Ok(())
}

/// Show cost breakdown by agent.
pub async fn show_by_agent() -> anyhow::Result<()> {
    let db = Db::open(&crate::db::default_path()?)?;
    db.migrate().await?;

    let groups = db.get_cost_by_agent().await?;

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
            format_tokens(group.input_tokens as u64),
            format_tokens(group.output_tokens as u64),
            format!("${:.4}", group.total_cost_usd),
            group.task_count,
        );
    }
    println!("{}", "-".repeat(60));
    println!(
        "{:<12} {:>10} {:>10} {:>10}",
        "",
        "",
        "",
        format!("${:.4}", total_cost)
    );
    println!();

    Ok(())
}

/// Show cost breakdown by model.
pub async fn show_by_model() -> anyhow::Result<()> {
    let db = Db::open(&crate::db::default_path()?)?;
    db.migrate().await?;

    let groups = db.get_cost_by_model().await?;

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
            format_tokens(group.input_tokens as u64),
            format_tokens(group.output_tokens as u64),
            format!("${:.4}", group.total_cost_usd),
            group.task_count,
        );
    }
    println!("{}", "-".repeat(60));
    println!(
        "{:<20} {:>10} {:>10} {:>10}",
        "",
        "",
        "",
        format!("${:.4}", total_cost)
    );
    println!();

    Ok(())
}

/// Format token counts with thousands separators.
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

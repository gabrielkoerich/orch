use crate::cli::init_store;
use crate::cli::ndjson;
use crate::home;
use crate::store::{TaskStatus, TaskStore};
use anyhow::Context;
use serde::Serialize;
use std::sync::Arc;

pub async fn export(
    task_id: &str,
    attempt: Option<u32>,
    format: ExportFormat,
) -> anyhow::Result<()> {
    let store = Arc::new(init_store().await?);

    let (internal_id, repo) = parse_task_id(task_id, &store).await?;
    let task = store.get(internal_id).await.context("task not found")?;

    let attempt = match attempt {
        Some(n) => n,
        None => {
            let runs = store.get_runs(internal_id).await?;
            if runs.is_empty() {
                anyhow::bail!("no attempts found for task");
            }
            let max_attempt = runs.iter().map(|r| r.attempt).max().unwrap_or(1);
            max_attempt as u32
        }
    };

    let attempt_dir = home::task_attempt_dir(&repo, task_id, attempt)?;
    let output_path = attempt_dir.join("output.json");

    if !output_path.exists() {
        anyhow::bail!("attempt {} not found (no output file)", attempt);
    }

    let content = std::fs::read_to_string(&output_path)?;

    match format {
        ExportFormat::Markdown => export_markdown(&task, attempt, &content, &store, &repo).await,
        ExportFormat::Json => export_json(&task, attempt, &content),
        ExportFormat::Raw => {
            println!("{}", content);
            Ok(())
        }
    }
}

async fn export_markdown(
    task: &crate::store::Task,
    attempt: u32,
    content: &str,
    store: &Arc<TaskStore>,
    _repo: &str,
) -> anyhow::Result<()> {
    let runs = store.get_runs(task.id).await?;
    let attempt_run = runs.iter().find(|r| r.attempt == attempt as i32);

    let agent = task.agent.as_deref().unwrap_or("unknown");
    let model = task.model.as_deref().unwrap_or("-");

    let duration = attempt_run.map(|r| r.duration_secs).unwrap_or(0.0);
    let duration_str = format_duration(duration);

    let input_tokens = task.input_tokens;
    let output_tokens = task.output_tokens;

    let cost = task.total_cost_usd;

    println!(
        "# Session: {} (Attempt {})",
        task_id_display(&task.id, &task.external_id),
        attempt
    );
    println!();
    println!("**Agent**: {} ({})", agent, model);
    println!("**Duration**: {}", duration_str);
    println!(
        "**Tokens**: {} in / {} out",
        format_num(input_tokens),
        format_num(output_tokens)
    );
    if cost > 0.0 {
        println!("**Cost**: ${:.4}", cost);
    }
    println!("---");

    if content.trim().is_empty() {
        println!("(no output captured)");
        return Ok(());
    }

    println!();
    println!("## Output");
    println!();

    let lines: Vec<&str> = content.lines().collect();

    for line in &lines {
        if let Some(formatted) = ndjson::format_line(line) {
            for fline in formatted.lines() {
                println!("{fline}");
            }
        }
    }

    let summary = &task.summary;
    if !summary.is_empty() {
        println!();
        println!("---");
        println!();
        println!("## Result");
        println!();

        let status = if task.status == TaskStatus::Done {
            "success"
        } else if task.status == TaskStatus::Blocked {
            "blocked"
        } else {
            "incomplete"
        };

        println!("**Status**: {}", status);
        println!("**Summary**: {}", summary);
    }

    let error = &task.last_error;
    if !error.is_empty() {
        println!();
        println!("**Error**: {}", error);
    }

    Ok(())
}

fn export_json(task: &crate::store::Task, attempt: u32, content: &str) -> anyhow::Result<()> {
    let output_lines: Vec<serde_json::Value> = content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            serde_json::from_str(trimmed).ok()
        })
        .collect();

    #[derive(Serialize)]
    struct SessionExport {
        task_id: i64,
        external_id: Option<String>,
        attempt: u32,
        agent: Option<String>,
        model: Option<String>,
        input_tokens: i64,
        output_tokens: i64,
        total_cost_usd: f64,
        summary: String,
        last_error: String,
        status: String,
        output: Vec<serde_json::Value>,
    }

    let export = SessionExport {
        task_id: task.id,
        external_id: task.external_id.clone(),
        attempt,
        agent: task.agent.clone(),
        model: task.model.clone(),
        input_tokens: task.input_tokens,
        output_tokens: task.output_tokens,
        total_cost_usd: task.total_cost_usd,
        summary: task.summary.clone(),
        last_error: task.last_error.clone(),
        status: task.status.as_str().to_string(),
        output: output_lines,
    };

    println!("{}", serde_json::to_string_pretty(&export)?);

    Ok(())
}

async fn parse_task_id(task_id: &str, store: &Arc<TaskStore>) -> anyhow::Result<(i64, String)> {
    if task_id.starts_with("internal:") {
        let id_part = task_id
            .strip_prefix("internal:")
            .unwrap_or(task_id)
            .parse::<i64>()
            .context("invalid internal task ID")?;
        let repo = store
            .get(id_part)
            .await
            .context("internal task not found")?
            .repo;
        Ok((id_part, repo))
    } else {
        let id: i64 = task_id.parse().context("invalid task ID")?;
        let repo = crate::config::get_current_repo()
            .context("no project configured — run `orch init` first")?;
        Ok((id, repo))
    }
}

fn task_id_display(internal_id: &i64, external_id: &Option<String>) -> String {
    match external_id {
        Some(ext) => format!("{}:{}", ext, internal_id),
        None => format!("internal:{}", internal_id),
    }
}

fn format_num(n: i64) -> String {
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

fn format_duration(secs: f64) -> String {
    if secs < 60.0 {
        format!("{:.0}s", secs)
    } else if secs < 3600.0 {
        let mins = secs / 60.0;
        if mins < 10.0 {
            format!("{:.1}m", mins)
        } else {
            format!("{:.0}m", mins)
        }
    } else {
        let hours = secs / 3600.0;
        format!("{:.1}h", hours)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub enum ExportFormat {
    #[default]
    Markdown,
    Json,
    Raw,
}

impl std::str::FromStr for ExportFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "markdown" | "md" => Ok(Self::Markdown),
            "json" | "js" => Ok(Self::Json),
            "raw" | "ndjson" => Ok(Self::Raw),
            _ => Err(format!("unknown format: {}", s)),
        }
    }
}

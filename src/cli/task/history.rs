//! `orch task` logs, runs, activity, history and inspect

use super::{
    format_age, format_run_cost, format_run_duration, format_run_text, review_attempt_line,
    truncate_err,
};
use crate::cli;
use crate::cli::task::cost::format_cost;
use crate::cli::task::reference;
use crate::engine::agents::patterns;
use crate::engine::tasks::priority::Priority;
use crate::store;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::sys::home;
use crate::sys::tmux::TmuxManager;
use std::fs;
use std::sync::Arc;

/// Show logs / post-mortem for a task (internal or external).
pub async fn logs(id: &str) -> anyhow::Result<()> {
    let store_owned = cli::init_store().await?;
    let resolved = reference::resolve(&store_owned, id).await?;
    let store: Option<Arc<TaskStore>> = Some(Arc::new(store_owned.clone()));
    let repo = resolved.repo.clone();
    let task_key = resolved.external_id.clone();

    if let Some(store_id) = resolved.store_id {
        if let Ok(task) = store_owned.get(store_id).await {
            if resolved.is_internal() {
                println!("ID: {} (internal)", task.id);
                println!("Title: {}", task.title);
                println!("Status: {}", task.status.as_str());
                if let Some(agent) = &task.agent {
                    println!("Agent: {agent}");
                }
                if let Some(reason) = &task.block_reason {
                    println!("Block reason: {reason}");
                }
                println!("Created: {}", task.created_at);
                println!("Updated: {}", task.updated_at);
            } else {
                println!("ID: {task_key} (external)");
                println!("Title: {}", task.title);
                println!("State: {}", task.status.as_str());
                println!("Labels: {}", task.labels.join(", "));
                println!("Author: {}", task.author);
                println!("URL: {}", task.url);
                println!("Created: {}", task.created_at);
                println!("Updated: {}", task.updated_at);
            }
        }
    } else {
        println!("ID: {}", resolved.display_spec());
    }

    // Task store fields
    {
        {
            // Agent, model, and attempts from store — single DB round-trip.
            if let Some(ref s) = store
                && let Ok(Some(t)) = s.get_by_external_id(&repo, &task_key).await
            {
                if let Some(ref agent) = t.agent {
                    println!("Agent: {agent}");
                }
                if let Some(ref model) = t.model {
                    println!("Model: {model}");
                }
                if t.attempts > 0 {
                    println!("Attempts: {}", t.attempts);
                }
            }

            // Token & cost summary
            let usage = store::get_token_usage(&store, &repo, &task_key).await;
            let cost = store::get_reported_cost(&store, &repo, &task_key).await;
            let total_tokens = usage.total_tokens();
            if total_tokens > 0 || cost.is_some() {
                println!("\nCost summary:");
                println!("  input tokens:  {}", usage.input_tokens);
                println!("  output tokens: {}", usage.output_tokens);
                println!("  total tokens:  {total_tokens}");
                println!("  cost:          {}", format_cost(cost.unwrap_or(0.0)));
            }

            // Memory (recent attempts)
            {
                let mem = store::get_recent_memory(&store, &repo, &task_key, 10).await;
                if !mem.is_empty() {
                    println!("\nMemory (recent attempts):");
                    for m in mem {
                        println!(
                            "- Attempt {} @ {}: agent={} model={}",
                            m.attempt,
                            m.timestamp,
                            m.agent,
                            m.model.clone().unwrap_or_default()
                        );
                        if let Some(err) = m.error.as_ref() {
                            println!("    Error: {err}");
                        }
                        if !m.learnings.is_empty() {
                            println!("    Learnings:");
                            for l in &m.learnings {
                                println!("      - {l}");
                            }
                        }
                        if !m.files_modified.is_empty() {
                            println!("    Files modified:");
                            for f in &m.files_modified {
                                println!("      - {f}");
                            }
                        }
                    }
                }
            }
        }
    }

    // Show agent output from last attempt if available
    let attempt_n = store::opt_store_get_task(&store, &repo, &task_key)
        .await
        .map_or(0, |t| u32::try_from(t.attempts).unwrap_or(0));
    if attempt_n > 0 {
        match home::task_attempt_dir(&repo, &task_key, attempt_n) {
            Ok(attempt_dir) => {
                let output_file = attempt_dir.join("output.json");
                let exit_file = attempt_dir.join("exit.txt");

                if output_file.exists()
                    && let Ok(content) = fs::read_to_string(&output_file)
                {
                    let tail = patterns::safe_tail(&content, 2000);
                    let truncated = if content.len() > 2000 {
                        format!(
                            "...({} chars truncated)...\n{}",
                            content.len() - tail.len(),
                            tail
                        )
                    } else {
                        content.clone()
                    };
                    println!("\n--- Last attempt output (attempt {attempt_n}) ---");
                    println!("{truncated}");
                }

                if exit_file.exists()
                    && let Ok(exit_code) = fs::read_to_string(&exit_file)
                {
                    println!("Exit code: {}", exit_code.trim());
                }
            }
            Err(e) => {
                println!("(could not resolve attempt dir: {e})");
            }
        }
    }

    // Show review agent output if available
    let review_task_id = format!("{task_key}-review");
    if let Ok(review_attempt_dir) = home::task_attempt_dir(&repo, &review_task_id, 1) {
        let review_output_file = review_attempt_dir.join("output.json");
        if review_output_file.exists()
            && let Ok(content) = fs::read_to_string(&review_output_file)
        {
            println!("\n--- Review agent output ---");
            println!("{content}");
        }
    }

    // Show audit trail from task_runs table (if available in store)
    if let Ok(store) = cli::init_store().await
        && let Ok(Some(store_id)) = store.resolve_task_id(&repo, &task_key).await
        && let Ok(runs) = store.get_runs(store_id).await
        && !runs.is_empty()
    {
        println!("\n--- Run audit trail ({} runs) ---", runs.len());
        for run in &runs {
            println!(
                "\n  Run #{} [{}] agent={} model={} outcome={}",
                run.attempt, run.run_type, run.agent, run.model, run.outcome,
            );
            if let Some(code) = run.exit_code {
                println!("    exit_code: {code}");
            }
            if !run.error.is_empty() {
                println!("    error: {}", run.error);
            }
            if run.input_tokens > 0 || run.output_tokens > 0 || run.total_cost_usd > 0.0 {
                println!(
                    "    tokens: {}in/{}out  cost: {}  duration: {:.1}s",
                    run.input_tokens,
                    run.output_tokens,
                    format_cost(run.total_cost_usd),
                    run.duration_secs,
                );
            }
            println!("    started: {}", run.started_at);
            if let Some(ref completed) = run.completed_at {
                println!("    completed: {completed}");
            }
        }
    }

    // If tmux session is live, append recent pane capture
    let tmux = TmuxManager::new();
    let session = tmux.session_name(&repo, &task_key);
    if tmux.session_exists(&session).await {
        println!("\nLive tmux session detected: {session} — appending recent output:\n---");
        match tmux.capture_pane(&session, 200).await {
            Ok(output) => {
                println!("{output}");
            }
            Err(e) => {
                println!("(tmux capture failed: {e})");
            }
        }
    }

    Ok(())
}

/// Show run history for a task.
pub async fn runs(id: &str, verbose: bool) -> anyhow::Result<()> {
    let store = cli::init_store().await?;
    let task_id = reference::resolve_store_id(&store, id).await?;

    let runs = store.get_runs(task_id).await?;
    if runs.is_empty() {
        println!("No runs found for task {id}.");
        return Ok(());
    }

    println!(
        "{:<4} {:<8} {:<18} {:<12} {:<8} {:<8} ERROR",
        "RUN", "AGENT", "MODEL", "OUTCOME", "DURATION", "COST"
    );
    println!("{}", "-".repeat(90));

    for run in runs {
        let model = if run.model.is_empty() {
            "-"
        } else {
            &run.model
        };
        let error = format_run_text(&run.error, verbose, 48);
        println!(
            "{:<4} {:<8} {:<18} {:<12} {:<8} {:<8} {}",
            run.attempt,
            run.agent,
            model,
            run.outcome,
            format_run_duration(run.duration_secs),
            format_run_cost(run.total_cost_usd),
            error,
        );

        if verbose {
            if !run.stdout.is_empty() {
                println!("  stdout:\n{}", run.stdout);
            }
            if !run.stderr.is_empty() {
                println!("  stderr:\n{}", run.stderr);
            }
            if !run.parsed_response.is_empty() {
                println!("  parsed_response:\n{}", run.parsed_response);
            }
            println!();
        }
    }

    Ok(())
}

/// Show lifecycle activity timeline for a task.
pub async fn activity_log(id: &str, limit: Option<usize>, json: bool) -> anyhow::Result<()> {
    let store = cli::init_store().await?;
    let task_id = reference::resolve_store_id(&store, id).await?;

    let events = store.get_activity(task_id, limit).await?;
    if events.is_empty() {
        println!("No activity found for task {id}.");
        return Ok(());
    }

    println!(
        "{:<20} {:<16} {:<12} {:<12} {:<10} {:<14} DETAILS",
        "TIME", "EVENT", "FROM", "TO", "AGENT", "MODEL"
    );
    println!("{}", "-".repeat(120));

    for event in events {
        let details = if json {
            serde_json::to_string(&event.details).unwrap_or_else(|_| "{}".to_string())
        } else if event.details.is_null() || event.details == serde_json::json!({}) {
            String::new()
        } else {
            let compact =
                serde_json::to_string(&event.details).unwrap_or_else(|_| "{}".to_string());
            truncate_err(&compact, 80)
        };
        println!(
            "{:<20} {:<16} {:<12} {:<12} {:<10} {:<14} {}",
            event.timestamp,
            event.event_type,
            event.from_status.as_deref().unwrap_or("-"),
            event.to_status.as_deref().unwrap_or("-"),
            event.agent.as_deref().unwrap_or("-"),
            event.model.as_deref().unwrap_or("-"),
            details
        );
    }

    Ok(())
}

/// Show task routing history with attempt timeline.
pub async fn history(id: &str) -> anyhow::Result<()> {
    let store = cli::init_store().await?;
    let task_id = reference::resolve_store_id(&store, id).await?;

    let task = store.get(task_id).await?;
    let events = store.get_activity(task_id, None).await?;

    // Header
    println!("Routing History for Task {id}");
    println!("{}", "=".repeat(60));
    println!("Title: {}", task.title);
    println!("Status: {:?}", task.status);
    println!();

    // Find routing events (routed, rerouted)
    let routing_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "routed" || e.event_type == "rerouted")
        .collect();

    if routing_events.is_empty() {
        println!("No routing history found. Task has not been routed yet.");
        return Ok(());
    }

    // Display routing timeline
    println!("Attempt Timeline:");
    println!("{}", "-".repeat(60));

    for (i, event) in routing_events.iter().enumerate() {
        let attempt_num = i + 1;
        let agent = event.agent.as_deref().unwrap_or("-");
        let model = event.model.as_deref().unwrap_or("-");
        let timestamp = &event.timestamp;

        if event.event_type == "routed" {
            println!("\nAttempt #{attempt_num}");
            println!("  Time:     {timestamp}");
            println!("  Agent:    {agent}");
            println!("  Model:    {model}");

            // Extract routing details
            if let Some(reason) = event.details.get("reason").and_then(|r| r.as_str()) {
                println!("  Reason:   {reason}");
            }
            if let Some(complexity) = event.details.get("complexity").and_then(|c| c.as_str()) {
                println!("  Complexity: {complexity}");
            }
        } else if event.event_type == "rerouted" {
            println!("  → Rerouted at {timestamp}");

            // Check for failure reason
            if let Some(failure) = event.details.get("failure_reason").and_then(|f| f.as_str()) {
                println!("    Failure: {failure}");
            } else if let Some(failure) = event.details.get("silence_duration_secs") {
                println!("    Failure: Agent silent for {failure}s");
            }

            // Show from/to agent if available
            if let Some(from) = event.details.get("from_agent").and_then(|a| a.as_str())
                && let Some(to) = event.details.get("to_agent").and_then(|a| a.as_str())
            {
                println!("    Change:   {from} → {to}");
            }

            // Show if agents were exhausted
            if event.details.get("agents_exhausted").is_some() {
                println!("    Note:     All agents exhausted");
            }
            if event.details.get("no_fallback_available").is_some() {
                println!("    Note:     No fallback available");
            }
        }
    }

    // Summary
    println!("\n{}", "-".repeat(60));
    println!("Summary:");
    println!("  Total routing attempts: {}", routing_events.len());

    // Get cost summary from task
    let total_tokens = task.input_tokens + task.output_tokens;
    if total_tokens > 0 {
        println!("  Input tokens:  {}", task.input_tokens);
        println!("  Output tokens: {}", task.output_tokens);
        println!("  Total cost:    {}", format_cost(task.total_cost_usd));
    }

    // Show current assigned agent/model if available
    if let Some(ref agent) = task.agent {
        println!("  Current agent: {agent}");
    }
    if let Some(ref model) = task.model {
        println!("  Current model: {model}");
    }

    Ok(())
}

/// Unified diagnostic view for a task: status, agent, attempts, last error, PR, block reason.
pub async fn inspect(id: &str) -> anyhow::Result<()> {
    let store = cli::init_store().await?;
    let task_id = reference::resolve_store_id(&store, id).await?;

    let task = store.get(task_id).await?;

    // ── Identity ─────────────────────────────────────────────────────────────
    println!("Task: {}", task.title);
    println!(
        "  ID      : {} (internal:{})",
        task.external_id.as_deref().unwrap_or("-"),
        task.id
    );
    println!("  Status  : {}", task.status.as_str());
    println!(
        "  Priority: {}",
        Priority::from_labels(&task.labels).as_str()
    );
    if task.status == TaskStatus::Waiting {
        let blockers = store
            .unmet_blocker_labels(task.id)
            .await
            .unwrap_or_default();
        if !blockers.is_empty() {
            println!("  Waiting : {}", blockers.join(", "));
        }
    }
    println!(
        "  Created : {} ({})",
        task.created_at,
        format_age(&task.created_at)
    );
    println!(
        "  Updated : {} ({})",
        task.updated_at,
        format_age(&task.updated_at)
    );
    if !task.url.is_empty() {
        println!("  URL     : {}", task.url);
    }

    // ── Routing ───────────────────────────────────────────────────────────────
    println!();
    println!("Routing:");
    println!("  Agent      : {}", task.agent.as_deref().unwrap_or("-"));
    println!("  Model      : {}", task.model.as_deref().unwrap_or("-"));
    if !task.complexity.is_empty() {
        println!("  Complexity : {}", task.complexity);
    }
    if task.route_attempts > 0 {
        println!("  Route attempts : {}", task.route_attempts);
    }
    if !task.route_reason.is_empty() {
        println!("  Reason     : {}", truncate_err(&task.route_reason, 120));
    }

    // ── Execution ─────────────────────────────────────────────────────────────
    println!();
    println!("Execution:");
    println!("  Attempts : {}", task.attempts);
    if !task.branch.is_empty() {
        println!("  Branch   : {}", task.branch);
    }
    if !task.worktree.is_empty() {
        println!("  Worktree : {}", task.worktree);
    }
    if !task.last_error.is_empty() {
        println!("  Last error : {}", truncate_err(&task.last_error, 200));
    }
    if !task.summary.is_empty() {
        println!("  Summary  : {}", truncate_err(&task.summary, 200));
    }

    // ── PR / Review ───────────────────────────────────────────────────────────
    if task.pr_number.is_some() || task.review_cycles > 0 || !task.pr_review_context.is_empty() {
        println!();
        println!("PR / Review:");
        if let Some(pr) = task.pr_number {
            println!("  PR number     : {pr}");
        }
        if task.review_cycles > 0 {
            println!("  Review cycles : {}", task.review_cycles);
        }
        if !task.pr_review_context.is_empty() {
            println!(
                "  Review context: {}",
                truncate_err(&task.pr_review_context, 200)
            );
        }
    }

    // Each failed review attempt, so a rebroadcast escalation shows what kept failing
    // instead of only the final "escalated after N retries"
    let attempts: Vec<_> = store
        .get_activity(task.id, None)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.event_type == "review_attempt_failed")
        .collect();
    if !attempts.is_empty() {
        println!();
        println!("Review attempts ({} failed):", attempts.len());
        for activity in attempts.iter().rev().take(10).rev() {
            println!("  {}", review_attempt_line(activity));
        }
    }

    // ── Block reason ─────────────────────────────────────────────────────────
    if let Some(ref reason) = task.block_reason {
        println!();
        println!("Blocked:");
        println!("  Reason : {reason}");
        if task.auto_unblock_count > 0 {
            println!("  Auto-unblock attempts : {}", task.auto_unblock_count);
        }
    }

    // ── Cost ─────────────────────────────────────────────────────────────────
    let total_tokens = task.input_tokens + task.output_tokens;
    if total_tokens > 0 || task.total_cost_usd > 0.0 {
        println!();
        println!("Cost:");
        println!(
            "  Tokens : {}in / {}out",
            task.input_tokens, task.output_tokens
        );
        println!("  Total  : {}", format_cost(task.total_cost_usd));
    }

    // ── Live session ─────────────────────────────────────────────────────────
    let task_key = task
        .external_id
        .clone()
        .unwrap_or_else(|| format!("internal:{}", task.id));
    let tmux = TmuxManager::new();
    let session = tmux.session_name(&task.repo, &task_key);
    if tmux.session_exists(&session).await {
        println!();
        println!("Live session: {session} (use `orch task attach {id}` to connect)");
    }

    Ok(())
}

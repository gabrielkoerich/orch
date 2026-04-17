//! CLI handlers for `orch chat` — control session interaction.

use crate::control;
use crate::store::{parse_since_duration, TaskStore};
use std::io::{self, BufRead, Write};

/// Interactive REPL mode — reads from stdin, sends to control session.
pub async fn interactive(session_id: &str) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;
    let model = control::get_model(&store).await?;
    let agent = control::get_agent(&store).await?;

    let session_label = if session_id == TaskStore::DEFAULT_SESSION {
        String::new()
    } else {
        format!(" [{session_id}]")
    };

    println!("orch control session ({agent}:{model}){session_label}");
    println!("Type /model, /agent to manage. Ctrl+C to exit.");
    println!("---");

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        print!("orch> ");
        stdout.flush()?;

        let mut line = String::new();
        let bytes = stdin.lock().read_line(&mut line)?;
        if bytes == 0 {
            break;
        }

        let message = line.trim();
        if message.is_empty() {
            continue;
        }
        if message == "exit" || message == "quit" {
            break;
        }

        match control::maybe_handle_control_command(&store, session_id, message).await? {
            Some(response) => {
                println!("{response}");
                println!();
            }
            None => match control::send_message(&store, session_id, "cli", None, message).await {
                Ok(response) => {
                    println!("{response}");
                    println!();
                }
                Err(e) => {
                    eprintln!("error: {e}");
                }
            },
        }
    }

    Ok(())
}

/// Single message mode — send one message, print response, exit.
pub async fn single_message(session_id: &str, message: &str) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;
    if let Some(response) =
        control::maybe_handle_control_command(&store, session_id, message).await?
    {
        println!("{response}");
        return Ok(());
    }

    let response = control::send_message(&store, session_id, "cli", None, message).await?;
    println!("{response}");
    Ok(())
}

/// Show conversation history.
pub async fn history(
    session_id: &str,
    search: Option<String>,
    since: Option<String>,
    limit: i64,
    with_cost: bool,
) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;

    // Parse --since into an ISO8601 cutoff timestamp if provided.
    let since_ts: Option<String> = since
        .as_deref()
        .map(parse_since_duration)
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let messages = if let Some(query) = search {
        store
            .search_control_messages(session_id, &query, since_ts.as_deref(), limit)
            .await?
    } else {
        store
            .list_control_messages(session_id, since_ts.as_deref(), limit)
            .await?
    };

    if messages.is_empty() {
        println!("No messages found.");
        return Ok(());
    }

    for msg in &messages {
        let role_label = match msg.role.as_str() {
            "user" => "you",
            "assistant" => msg.agent.as_deref().unwrap_or("assistant"),
            _ => &msg.role,
        };
        let model_info = msg
            .model
            .as_deref()
            .map(|m| format!(" ({m})"))
            .unwrap_or_default();

        let cost_info = if with_cost {
            let mut parts = Vec::new();
            if let Some(cost) = msg.cost_usd {
                parts.push(format!("${cost:.4}"));
            }
            if let (Some(input), Some(output)) = (msg.input_tokens, msg.output_tokens) {
                parts.push(format!(
                    "in:{} out:{}",
                    format_number(input),
                    format_number(output)
                ));
            } else if let Some(tokens) = msg.tokens_used {
                parts.push(format!("{} tok", format_number(tokens)));
            }
            if parts.is_empty() {
                String::new()
            } else {
                format!(" [{}]", parts.join(", "))
            }
        } else {
            String::new()
        };

        println!(
            "[{}] {}{}{}",
            msg.created_at, role_label, model_info, cost_info
        );
        for line in msg.content.lines() {
            println!("  {line}");
        }
        println!();
    }

    Ok(())
}

/// Show session cost statistics.
pub async fn stats(session_id: &str) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;

    let summary = store.get_session_cost_summary(session_id).await?;
    let fallback_model = control::get_model(&store).await?;
    let fallback_agent = control::get_agent(&store).await?;
    let current_model = summary.primary_model.clone().unwrap_or(fallback_model);
    let current_agent = summary.primary_agent.clone().unwrap_or(fallback_agent);

    if summary.total_messages == 0 {
        println!("No messages found in session '{}'.", session_id);
        return Ok(());
    }

    let session_label = if session_id == TaskStore::DEFAULT_SESSION {
        "default".to_string()
    } else {
        session_id.to_string()
    };

    println!(
        "Session: {} ({}:{})",
        session_label, current_agent, current_model
    );
    println!(
        "Messages: {} ({} user, {} assistant)",
        summary.total_messages,
        summary.total_messages - summary.assistant_messages,
        summary.assistant_messages
    );

    if summary.total_tokens > 0 {
        println!("Total tokens: {}", format_number(summary.total_tokens));
        if summary.total_input_tokens > 0 || summary.total_output_tokens > 0 {
            let input = summary.total_input_tokens;
            let output = summary.total_output_tokens;
            println!("  Input: {}", format_number(input));
            println!("  Output: {}", format_number(output));
        }
    }

    println!("Estimated cost: ${:.4}", summary.total_cost_usd);

    if !summary.by_model.is_empty() {
        println!("\nBreakdown by model:");
        for (model, count, tokens, cost) in &summary.by_model {
            let tokens_str = if *tokens > 0 {
                format!(", {} tok", format_number(*tokens))
            } else {
                String::new()
            };
            println!("  {}: {} msg{} (${:.4})", model, count, tokens_str, cost);
        }
    }

    Ok(())
}

fn format_number(n: i64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (count, c) in s.chars().rev().enumerate() {
        if count > 0 && count % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

//! CLI handlers for `orch chat` — control session interaction.

use crate::control;
use crate::store::{parse_since_duration, TaskStore};
use std::io::{self, BufRead, Write};

/// Interactive REPL mode — reads from stdin, sends to control session.
pub async fn interactive(session_id: &str) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;
    let model = control::get_model(&store).await;
    let agent = control::get_agent(&store).await;

    let session_label = if session_id == TaskStore::DEFAULT_SESSION {
        String::new()
    } else {
        format!(" [{session_id}]")
    };

    println!("orch control session ({agent}:{model}){session_label}");
    println!("Type /model [agent:]<model> or /agent <name> to switch, Ctrl+C to exit");
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

        match control::maybe_handle_control_command(&store, message).await? {
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
    if let Some(response) = control::maybe_handle_control_command(&store, message).await? {
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

        println!("[{}] {}{}", msg.created_at, role_label, model_info);
        for line in msg.content.lines() {
            println!("  {line}");
        }
        println!();
    }

    Ok(())
}

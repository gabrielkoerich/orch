//! CLI handlers for `orch chat` — control session interaction.

use crate::control;
use std::io::{self, BufRead, Write};

/// Interactive REPL mode — reads from stdin, sends to control session.
pub async fn interactive() -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;
    let model = control::get_model(&store).await;
    let agent = control::get_agent(&store).await;

    println!("orch control session ({agent}:{model})");
    println!("Type /model [agent:]<model> to switch, Ctrl+C to exit");
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

        match control::send_message(&store, "cli", None, message).await {
            Ok(response) => {
                println!("{response}");
                println!();
            }
            Err(e) => {
                eprintln!("error: {e}");
            }
        }
    }

    Ok(())
}

/// Single message mode — send one message, print response, exit.
pub async fn single_message(message: &str) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;
    let response = control::send_message(&store, "cli", None, message).await?;
    println!("{response}");
    Ok(())
}

/// Show conversation history.
pub async fn history(search: Option<String>, limit: i64) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;

    let messages = if let Some(query) = search {
        store.search_control_messages(&query, limit).await?
    } else {
        store.list_control_messages(limit, 0).await?
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

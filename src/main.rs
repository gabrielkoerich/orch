mod backends;
#[allow(dead_code)] // channels are scaffolding — not wired into engine yet
mod channels;
mod config;
mod cron;
mod db;
mod engine;
mod github;
mod parser;
#[allow(dead_code)] // security module ready for use, not integrated yet
mod security;
mod sidecar;
mod tmux;

use clap::{Parser, Subcommand};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "orch-core", version, about = "Orch — The Agent Orchestrator")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the orchestrator service
    Serve,
    /// Parse and normalize agent JSON response
    Parse {
        /// Path to JSON file (or - for stdin)
        path: String,
    },
    /// Check if a cron expression matches now
    Cron {
        /// Cron expression (5 fields)
        expression: String,
        /// Check if schedule fired since this timestamp
        #[arg(long)]
        since: Option<String>,
    },
    /// Read/write sidecar JSON files
    Sidecar {
        #[command(subcommand)]
        action: SidecarAction,
    },
    /// Read config values
    Config {
        /// Config key (dot-separated path)
        key: String,
    },
    /// Stream live output from a running task
    Stream {
        /// Task ID to stream
        task_id: String,
    },
}

#[derive(Subcommand)]
enum SidecarAction {
    /// Get a field from a sidecar file
    Get {
        /// Task ID
        task_id: String,
        /// Field name
        field: String,
    },
    /// Set a field in a sidecar file
    Set {
        /// Task ID
        task_id: String,
        /// Field=value pairs
        fields: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("orch_core=info".parse()?),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve => {
            tracing::info!("starting orch-core serve");
            engine::serve().await?;
        }
        Commands::Parse { path } => {
            parser::parse_and_print(&path)?;
        }
        Commands::Cron { expression, since } => {
            let matches = cron::check(&expression, since.as_deref())?;
            std::process::exit(if matches { 0 } else { 1 });
        }
        Commands::Sidecar { action } => match action {
            SidecarAction::Get { task_id, field } => {
                let val = sidecar::get(&task_id, &field)?;
                println!("{val}");
            }
            SidecarAction::Set { task_id, fields } => {
                sidecar::set(&task_id, &fields)?;
            }
        },
        Commands::Config { key } => {
            let val = config::get(&key)?;
            println!("{val}");
        }
        Commands::Stream { task_id } => {
            stream_task(&task_id).await?;
        }
    }

    Ok(())
}

/// Stream live output from a running task.
///
/// This connects to the transport layer and prints output chunks
/// as they arrive from the tmux capture loop.
async fn stream_task(task_id: &str) -> anyhow::Result<()> {
    use crate::channels::transport::Transport;
    use tokio::sync::broadcast;

    // Create transport (this is a local connection to the running service)
    let transport = Arc::new(Transport::new());

    // Bind to the task session
    let session_name = format!("orch-{}", task_id);
    transport.bind(task_id, &session_name, "cli", "stream").await;

    // Subscribe to output
    let mut rx = match transport.subscribe(task_id).await {
        Some(rx) => rx,
        None => {
            anyhow::bail!("no active session for task {}", task_id);
        }
    };

    println!("Streaming output from task {} (session: {})", task_id, session_name);
    println!("Press Ctrl+C to stop streaming");
    println!("---");

    // Print output chunks as they arrive
    loop {
        match rx.recv().await {
            Ok(chunk) => {
                print!("{}", chunk.content);
                // Flush stdout to see output immediately
                std::io::Write::flush(&mut std::io::stdout())?;

                if chunk.is_final {
                    println!("\n--- Stream ended ---");
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("skipped {} missed messages", n);
            }
            Err(broadcast::error::RecvError::Closed) => {
                println!("\n--- Stream closed ---");
                break;
            }
        }
    }

    Ok(())
}

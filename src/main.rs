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
    }

    Ok(())
}

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
mod template;
mod tmux;

use crate::engine::tasks::{CreateTaskRequest, TaskFilter, TaskManager, TaskType};
use anyhow::Context;
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
    /// Render a template file with environment variables
    Template {
        /// Path to template file
        path: String,
        /// Additional KEY=VALUE pairs (optional)
        vars: Vec<String>,
    },
    /// Task management (internal and external tasks)
    Task {
        #[command(subcommand)]
        action: TaskAction,
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

#[derive(Subcommand)]
enum TaskAction {
    /// List tasks (internal + external)
    List {
        /// Filter by status
        #[arg(long)]
        status: Option<String>,
        /// Filter by source
        #[arg(long)]
        source: Option<String>,
    },
    /// Create an internal task
    Add {
        /// Task title
        title: String,
        /// Task body
        #[arg(short, long)]
        body: Option<String>,
        /// Task source (e.g., manual, cron, mention)
        #[arg(short, long, default_value = "manual")]
        source: String,
    },
    /// Get task details
    Get {
        /// Task ID
        id: i64,
    },
    /// Publish internal task to GitHub
    Publish {
        /// Task ID
        id: i64,
        /// Labels to add
        #[arg(short, long)]
        labels: Vec<String>,
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
        Commands::Template { path, vars } => {
            template::render_and_print(&path, &vars)?;
        }
        Commands::Task { action } => match action {
            TaskAction::List { status, source } => {
                task_list(status, source).await?;
            }
            TaskAction::Add {
                title,
                body,
                source,
            } => {
                task_add(title, body, source).await?;
            }
            TaskAction::Get { id } => {
                task_get(id).await?;
            }
            TaskAction::Publish { id, labels } => {
                task_publish(id, labels).await?;
            }
        },
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
    transport
        .bind(task_id, &session_name, "cli", "stream")
        .await;

    // Subscribe to output
    let mut rx = match transport.subscribe(task_id).await {
        Some(rx) => rx,
        None => {
            anyhow::bail!("no active session for task {}", task_id);
        }
    };

    println!(
        "Streaming output from task {} (session: {})",
        task_id, session_name
    );
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

/// Initialize task manager with database and backend.
async fn init_task_manager() -> anyhow::Result<TaskManager> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::db::Db;

    let repo = config::get("repo").context(
        "'repo' not set in config — run `orch-core init` or set repo in ~/.orchestrator/config.yml",
    )?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo));
    let db = Arc::new(Db::open(&crate::db::default_path()?)?);
    db.migrate().await?;
    Ok(TaskManager::new(db, backend))
}

/// List tasks with optional filters.
async fn task_list(status: Option<String>, source: Option<String>) -> anyhow::Result<()> {
    let task_manager = init_task_manager().await?;
    let filter = TaskFilter { status, source };
    let tasks = task_manager.list_tasks(filter).await?;

    if tasks.is_empty() {
        println!("No tasks found.");
        return Ok(());
    }

    println!("{:<10} {:<12} {:<20} TITLE", "ID", "TYPE", "STATUS");
    println!("{}", "-".repeat(80));

    for task in tasks {
        match task {
            engine::tasks::Task::External(ext) => {
                let status = ext
                    .labels
                    .iter()
                    .find(|l| l.starts_with("status:"))
                    .map(|s| s.replace("status:", ""))
                    .unwrap_or_else(|| "unknown".to_string());
                println!(
                    "{:<10} {:<12} {:<20} {}",
                    ext.id.0, "external", status, ext.title
                );
            }
            engine::tasks::Task::Internal(int) => {
                println!(
                    "{:<10} {:<12} {:<20} {}",
                    int.id,
                    "internal",
                    int.status.as_str(),
                    int.title
                );
            }
        }
    }

    Ok(())
}

/// Create a new internal task.
async fn task_add(title: String, body: Option<String>, source: String) -> anyhow::Result<()> {
    let task_manager = init_task_manager().await?;
    let req = CreateTaskRequest {
        title,
        body: body.unwrap_or_default(),
        task_type: TaskType::Internal,
        labels: vec![],
        source,
        source_id: String::new(),
    };
    let task = task_manager.create_task(req).await?;

    match task {
        engine::tasks::Task::Internal(t) => {
            println!("Created internal task #{}: {}", t.id, t.title);
        }
        _ => {
            println!("Created task");
        }
    }

    Ok(())
}

/// Get task details by ID.
async fn task_get(id: i64) -> anyhow::Result<()> {
    let task_manager = init_task_manager().await?;
    let task = task_manager.get_task(id).await?;

    match task {
        engine::tasks::Task::External(ext) => {
            println!("ID: {} (external)", ext.id.0);
            println!("Title: {}", ext.title);
            println!("State: {}", ext.state);
            println!("Labels: {}", ext.labels.join(", "));
            println!("Author: {}", ext.author);
            println!("URL: {}", ext.url);
            println!("Created: {}", ext.created_at);
            println!("Updated: {}", ext.updated_at);
            println!("\n{}", ext.body);
        }
        engine::tasks::Task::Internal(int) => {
            println!("ID: {} (internal)", int.id);
            println!("Title: {}", int.title);
            println!("Status: {}", int.status.as_str());
            println!("Source: {}", int.source);
            if let Some(agent) = &int.agent {
                println!("Agent: {}", agent);
            }
            if let Some(reason) = &int.block_reason {
                println!("Block reason: {}", reason);
            }
            println!("Created: {}", int.created_at.to_rfc3339());
            println!("Updated: {}", int.updated_at.to_rfc3339());
            println!("\n{}", int.body);
        }
    }

    Ok(())
}

/// Publish an internal task to GitHub.
async fn task_publish(id: i64, labels: Vec<String>) -> anyhow::Result<()> {
    let task_manager = init_task_manager().await?;
    let ext_id = task_manager.publish_task(id, &labels).await?;
    println!("Published task #{} as GitHub issue {}", id, ext_id.0);
    Ok(())
}

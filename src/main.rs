mod backends;
#[allow(dead_code)] // channels are scaffolding — not wired into engine yet
mod channels;
mod cli;
mod config;
mod cron;
mod db;
mod engine;
mod github;
mod home;
mod parser;
#[allow(dead_code)] // security module ready for use, not integrated yet
pub mod security;
mod sidecar;
mod template;
mod tmux;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};

#[derive(Parser)]
#[command(name = "orch", version, about = "Orch — The Agent Orchestrator")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the orchestrator service
    Serve,
    /// Show version information
    Version,
    /// Initialize orchestrator for a project
    Init {
        /// Repository in OWNER/REPO format
        #[arg(long)]
        repo: Option<String>,
    },
    /// Tail orchestrator logs
    Log {
        /// Number of lines to show, or "watch" for live follow
        #[arg(default_value = "50")]
        lines: String,
    },
    /// List installed agent CLIs
    Agents,
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
    /// Task management
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// Job management (scheduled tasks)
    Job {
        #[command(subcommand)]
        action: JobAction,
    },
    /// Service management (start/stop/restart)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Show task metrics summary
    Metrics,
    /// GitHub Projects V2 board management
    Board {
        #[command(subcommand)]
        action: BoardAction,
    },
    /// Multi-project management
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },
    /// Show cost tracking and token usage
    Cost {
        /// Task ID to show cost for
        task_id: Option<String>,
        /// Show aggregate cost summary (24h, 7d, 30d)
        #[arg(long)]
        summary: bool,
        /// Show costs grouped by agent
        #[arg(long)]
        agent: bool,
        /// Show costs grouped by model
        #[arg(long)]
        model: bool,
    },
    /// Generate shell completions
    Completions {
        /// Shell type
        shell: Shell,
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
    /// Create a task
    Add {
        /// Task title
        title: String,
        /// Task body
        #[arg(short, long)]
        body: Option<String>,
        /// Labels to add
        #[arg(short, long)]
        labels: Vec<String>,
        /// Task source (e.g., manual, cron, mention)
        #[arg(short, long, default_value = "manual")]
        source: String,
    },
    /// Get task details
    Get {
        /// Task ID
        id: i64,
    },
    /// Show task status summary
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Route a task to an agent
    Route {
        /// Task ID
        id: i64,
    },
    /// Run a task (manual execution)
    Run {
        /// Task ID (if omitted, picks next routed task)
        id: Option<String>,
    },
    /// Retry a task (reset to new)
    Retry {
        /// Task ID
        id: i64,
    },
    /// Unblock a task or all blocked tasks
    Unblock {
        /// Task ID or "all"
        id: String,
    },
    /// Attach to a running agent's tmux session
    Attach {
        /// Task ID
        id: String,
    },
    /// List active agent tmux sessions
    Live,
    /// Kill a running agent tmux session
    Kill {
        /// Task ID
        id: String,
    },
    /// Publish internal task to GitHub
    Publish {
        /// Task ID
        id: i64,
        /// Labels to add
        #[arg(short, long)]
        labels: Vec<String>,
    },
    /// Show token cost breakdown for a task
    Cost {
        /// Task ID
        id: String,
    },
}

#[derive(Subcommand)]
enum JobAction {
    /// List scheduled jobs
    List,
    /// Add a scheduled job
    Add {
        /// Cron schedule expression
        schedule: String,
        /// Job title
        title: String,
        /// Job body
        #[arg(short, long)]
        body: Option<String>,
        /// Job type: task or bash
        #[arg(short = 't', long, default_value = "task")]
        r#type: String,
        /// Bash command (for type=bash)
        #[arg(short, long)]
        command: Option<String>,
    },
    /// Remove a job
    Remove {
        /// Job ID
        id: String,
    },
    /// Enable a job
    Enable {
        /// Job ID
        id: String,
    },
    /// Disable a job
    Disable {
        /// Job ID
        id: String,
    },
    /// Run one job scheduler tick
    Tick,
}

#[derive(Subcommand)]
enum BoardAction {
    /// List accessible GitHub Projects V2 boards
    List,
    /// Link current repo to a project board by ID
    Link {
        /// Project node ID (PVT_...)
        id: String,
    },
    /// Re-discover field IDs and update config
    Sync,
    /// Show current board config
    Info,
}

#[derive(Subcommand)]
enum ProjectAction {
    /// Add a project to the global registry (local path or GitHub slug)
    Add {
        /// Local path, GitHub slug (owner/repo), or GitHub URL
        #[arg(default_value = ".")]
        path: String,
    },
    /// Remove a project from the global registry
    Remove {
        /// Path to the project directory
        path: String,
    },
    /// List all registered projects
    List,
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Start the orchestrator service
    Start,
    /// Stop the orchestrator service
    Stop,
    /// Restart the orchestrator service
    Restart,
    /// Show service status
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("orch=info".parse()?),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve => {
            tracing::info!("starting orch serve");
            engine::serve().await?;
        }
        Commands::Version => {
            cli::version();
        }
        Commands::Init { repo } => {
            cli::init(repo)?;
        }
        Commands::Log { lines } => {
            cli::log(&lines)?;
        }
        Commands::Agents => {
            cli::agents();
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
            cli::stream_task(&task_id).await?;
        }
        Commands::Template { path, vars } => {
            template::render_and_print(&path, &vars)?;
        }
        Commands::Task { action } => match action {
            TaskAction::List { status, source } => {
                cli::task::list(status, source).await?;
            }
            TaskAction::Add {
                title,
                body,
                labels,
                source,
            } => {
                cli::task::add(title, body, labels, source).await?;
            }
            TaskAction::Get { id } => {
                cli::task::get(id).await?;
            }
            TaskAction::Status { json } => {
                cli::task::status(json).await?;
            }
            TaskAction::Route { id } => {
                cli::task::route(id).await?;
            }
            TaskAction::Run { id } => {
                cli::task::run(id).await?;
            }
            TaskAction::Retry { id } => {
                cli::task::retry(id).await?;
            }
            TaskAction::Unblock { id } => {
                cli::task::unblock(&id).await?;
            }
            TaskAction::Attach { id } => {
                cli::task::attach(&id)?;
            }
            TaskAction::Live => {
                cli::task::live().await?;
            }
            TaskAction::Kill { id } => {
                cli::task::kill(&id).await?;
            }
            TaskAction::Publish { id, labels } => {
                cli::task::publish(id, labels).await?;
            }
            TaskAction::Cost { id } => {
                cli::task::cost(&id)?;
            }
        },
        Commands::Job { action } => match action {
            JobAction::List => {
                cli::job::list()?;
            }
            JobAction::Add {
                schedule,
                title,
                body,
                r#type,
                command,
            } => {
                cli::job::add(
                    &schedule,
                    &title,
                    body.as_deref(),
                    &r#type,
                    command.as_deref(),
                )?;
            }
            JobAction::Remove { id } => {
                cli::job::remove(&id)?;
            }
            JobAction::Enable { id } => {
                cli::job::enable(&id)?;
            }
            JobAction::Disable { id } => {
                cli::job::disable(&id)?;
            }
            JobAction::Tick => {
                cli::job::tick().await?;
            }
        },
        Commands::Service { action } => match action {
            ServiceAction::Start => {
                cli::service::start()?;
            }
            ServiceAction::Stop => {
                cli::service::stop()?;
            }
            ServiceAction::Restart => {
                cli::service::restart()?;
            }
            ServiceAction::Status => {
                cli::service::status()?;
            }
        },
    Commands::Metrics => {
            cli::metrics().await?;
        }
        /// Combined dashboard view: tasks, sessions, recent activity
        Commands::Dashboard => {
            cli::dashboard::dashboard().await?;
        }
        Commands::Board { action } => match action {
            BoardAction::List => {
                cli::board_list().await?;
            }
            BoardAction::Link { id } => {
                cli::board_link(&id).await?;
            }
            BoardAction::Sync => {
                cli::board_sync().await?;
            }
            BoardAction::Info => {
                cli::board_info()?;
            }
        },
        Commands::Project { action } => match action {
            ProjectAction::Add { path } => {
                cli::project_add(&path)?;
            }
            ProjectAction::Remove { path } => {
                cli::project_remove(&path)?;
            }
            ProjectAction::List => {
                cli::project_list()?;
            }
        },
        Commands::Cost {
            task_id,
            summary,
            agent,
            model,
        } => {
            if let Some(id) = task_id {
                cli::cost::show_task(&id)?;
            } else if agent {
                cli::cost::show_by_agent().await?;
            } else if model {
                cli::cost::show_by_model().await?;
            } else if summary {
                cli::cost::show_summary().await?;
            } else {
                // Default: show summary
                cli::cost::show_summary().await?;
            }
        }
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            generate(shell, &mut cmd, "orch", &mut std::io::stdout());
        }
    }

    Ok(())
}

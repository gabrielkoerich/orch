mod backends;
mod channels;
mod cli;
mod cmd;
mod cmd_cache;
mod config;
mod control;
mod cron;
mod engine;
mod github;
mod home;
mod parser;
mod repo_context;
pub mod security;
mod store;
mod template;
mod tmux;
mod webhook_status;

use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};

const MAX_SERVICE_LOG_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ROTATED_SERVICE_LOGS: usize = 3;

/// Ensure common tool directories are on PATH.
///
/// Sources `$HOME/.path` if it exists (user-managed PATH exports), then adds
/// well-known directories as a fallback. This is needed because launchd services
/// inherit a minimal PATH that excludes Homebrew, Cargo, npm globals, etc.
fn ensure_path() {
    let home = std::env::var("HOME").unwrap_or_default();

    // Source ~/.path — a shell snippet that exports PATH entries.
    // We parse `export PATH="..."` lines and extract the directories.
    let dotpath = format!("{home}/.path");
    if std::path::Path::new(&dotpath).is_file() {
        if let Ok(contents) = std::fs::read_to_string(&dotpath) {
            let current = std::env::var("PATH").unwrap_or_default();
            let mut path = current.clone();
            for line in contents.lines() {
                let line = line.trim();
                // Match: export PATH="...:$PATH" or export PATH="...$PATH"
                if let Some(value) = line
                    .strip_prefix("export PATH=\"")
                    .and_then(|s| s.strip_suffix('"'))
                {
                    // Extract the new directory (only the part before the first $PATH)
                    // Use strip_prefix to handle $PATH only at the end, preserving any
                    // additional path components that come before it
                    let dir = if let Some((before, _)) = value.split_once("$PATH") {
                        before
                    } else {
                        value
                    }
                    .replace("$HOME", &home)
                    .trim_end_matches(':')
                    .to_string();
                    if !dir.is_empty()
                        && !path.split(':').any(|d| d == dir)
                        && std::path::Path::new(&dir).is_dir()
                    {
                        path = format!("{dir}:{path}");
                    }
                }
            }
            if path != current {
                std::env::set_var("PATH", &path);
                return;
            }
        }
    }

    // Fallback: add well-known directories if ~/.path doesn't exist.
    let extra = [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        &format!("{home}/.cargo/bin"),
        &format!("{home}/.local/bin"),
        &format!("{home}/.bun/bin"),
    ];
    let current = std::env::var("PATH").unwrap_or_default();
    let mut new_path = current.clone();
    for dir in &extra {
        if !new_path.split(':').any(|d| d == *dir) && std::path::Path::new(dir).is_dir() {
            new_path = format!("{dir}:{new_path}");
        }
    }
    if new_path != current {
        std::env::set_var("PATH", &new_path);
    }
}

/// Load `export KEY=VALUE` lines from `~/.private` into the process environment.
///
/// Called once at startup so all child processes (router LLM, agent runners)
/// inherit tokens like KIMI_API_KEY without requiring a shell source.
fn load_private_env() {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = format!("{home}/.private");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return;
    };
    for line in contents.lines() {
        let line = line.trim();
        // Match: export KEY="value" or export KEY=value
        let Some(rest) = line.strip_prefix("export ") else {
            continue;
        };
        let Some((key, val)) = rest.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim().trim_matches('"');
        if !key.is_empty() && std::env::var(key).is_err() {
            std::env::set_var(key, val);
        }
    }
}

fn rotate_log_if_needed(path: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };

    if meta.len() <= MAX_SERVICE_LOG_BYTES {
        return;
    }

    let oldest = rotated_log_path(path, MAX_ROTATED_SERVICE_LOGS);
    let _ = std::fs::remove_file(&oldest);

    for suffix in (1..MAX_ROTATED_SERVICE_LOGS).rev() {
        let from = rotated_log_path(path, suffix);
        let to = rotated_log_path(path, suffix + 1);
        let _ = std::fs::rename(from, to);
    }

    let _ = std::fs::rename(path, rotated_log_path(path, 1));
}

fn rotate_service_logs_if_needed() {
    let brew_prefix = std::env::var("HOMEBREW_PREFIX").unwrap_or_else(|_| "/opt/homebrew".into());
    let brew_log_dir = std::path::PathBuf::from(&brew_prefix).join("var/log");
    rotate_log_if_needed(&brew_log_dir.join("orch.log"));
    // Truncate error log on every startup so agents never see stale panics from previous runs
    // and file duplicate issues based on old errors.
    let _ = std::fs::File::create(brew_log_dir.join("orch.error.log"));

    if let Ok(state_dir) = home::state_dir() {
        rotate_log_if_needed(&state_dir.join("orch.log"));
    }
}

fn rotated_log_path(path: &std::path::Path, suffix: usize) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{suffix}"));
    std::path::PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::{rotate_log_if_needed, rotated_log_path, Cli, Commands, MAX_SERVICE_LOG_BYTES};
    use clap::Parser;

    #[test]
    fn rotated_log_path_appends_suffix() {
        let path = std::path::Path::new("/opt/homebrew/var/log/orch.log");
        assert_eq!(
            rotated_log_path(path, 2),
            std::path::PathBuf::from("/opt/homebrew/var/log/orch.log.2")
        );
    }

    #[test]
    fn rotate_log_if_needed_keeps_three_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orch.log");

        std::fs::write(&path, vec![b'a'; (MAX_SERVICE_LOG_BYTES + 1) as usize]).unwrap();
        std::fs::write(path.with_file_name("orch.log.1"), b"one").unwrap();
        std::fs::write(path.with_file_name("orch.log.2"), b"two").unwrap();
        std::fs::write(path.with_file_name("orch.log.3"), b"three").unwrap();

        rotate_log_if_needed(&path);

        assert!(!path.exists());
        assert_eq!(
            std::fs::read(path.with_file_name("orch.log.1"))
                .unwrap()
                .len() as u64,
            MAX_SERVICE_LOG_BYTES + 1
        );
        assert_eq!(
            std::fs::read(path.with_file_name("orch.log.2")).unwrap(),
            b"one"
        );
        assert_eq!(
            std::fs::read(path.with_file_name("orch.log.3")).unwrap(),
            b"two"
        );
    }

    #[test]
    fn stream_defaults_to_formatted_output() {
        let cli = Cli::parse_from(["orch", "stream"]);
        match cli.command {
            Commands::Stream { formatted, raw, .. } => {
                assert!(formatted);
                assert!(!raw);
            }
            _ => panic!("expected stream command"),
        }
    }

    #[test]
    fn stream_allows_disabling_formatted_output() {
        let cli = Cli::parse_from(["orch", "stream", "--formatted=false"]);
        match cli.command {
            Commands::Stream { formatted, raw, .. } => {
                assert!(!formatted);
                assert!(!raw);
            }
            _ => panic!("expected stream command"),
        }
    }

    #[test]
    fn stream_raw_flag_still_parses_for_backwards_compat() {
        let cli = Cli::parse_from(["orch", "stream", "--raw"]);
        match cli.command {
            Commands::Stream { formatted, raw, .. } => {
                assert!(formatted);
                assert!(raw);
            }
            _ => panic!("expected stream command"),
        }
    }
}

#[derive(Parser)]
#[command(name = "orch", version = env!("ORCH_VERSION"), about = "Orch — The Agent Orchestration Engine")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the orch service
    Serve,
    /// Show version information
    Version,
    /// Initialize orch for a project
    Init {
        /// Repository in OWNER/REPO format
        #[arg(long)]
        repo: Option<String>,
    },
    /// Tail orch logs
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
    /// Read config values
    Config {
        /// Config key (dot-separated path)
        key: String,
    },
    /// Stream live output from running tasks (all if no ID given)
    Stream {
        /// Task ID to stream (omit to stream all running tasks)
        task_id: Option<String>,
        /// Format NDJSON into human-readable output (`--formatted=false` for raw)
        #[arg(long, default_value_t = true, action = ArgAction::Set)]
        formatted: bool,
        /// Print raw NDJSON instead of human-readable output (deprecated: use `--formatted=false`)
        #[arg(long, hide = true)]
        raw: bool,
        /// Read NDJSON from stdin and pipe through the NDJSON formatter.
        /// Useful for piping tmux capture-pane output:
        /// `tmux capture-pane -p -t session:pane | orch stream --pipe`
        #[arg(long, default_value_t = false)]
        pipe: bool,
    },
    /// Chat with the orch control session
    #[command(
        subcommand_precedence_over_arg = true,
        args_conflicts_with_subcommands = true
    )]
    Chat {
        /// Session profile (default: "default")
        #[arg(long, short, default_value = "default")]
        session: String,
        /// Send a single message (omit for interactive mode)
        message: Vec<String>,
        #[command(subcommand)]
        action: Option<ChatAction>,
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
    Metrics {
        /// Show slow tasks and error distribution
        #[arg(long)]
        details: bool,
        /// Time window to report on (e.g. "24h", "7d", "30d"; default: "24h")
        #[arg(long, default_value = "24h")]
        since: String,
    },
    /// Combined dashboard: tasks, sessions, recent activity
    Dashboard {
        /// Show tasks across all projects (default when outside a project directory)
        #[arg(long, short = 'g')]
        global: bool,
        /// Filter to a specific project (e.g. owner/repo or just repo name)
        #[arg(long)]
        project: Option<String>,
    },
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
    /// Manage agent and model cooldowns
    Cooldown {
        #[command(subcommand)]
        action: CooldownAction,
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
    /// Show task metrics and statistics
    Stats {
        /// Aggregate all projects into one table
        #[arg(long)]
        all: bool,
        /// Time window to report on (e.g. "24h", "7d", "30d"; default: "24h")
        #[arg(long, default_value = "24h")]
        since: String,
    },
    /// Generate shell completions
    Completions {
        /// Shell type
        shell: Shell,
    },
    /// Stream task events in real-time
    Events {
        /// Filter by repo (substring match)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by task ID
        #[arg(long)]
        task: Option<String>,
    },
    /// Webhook server management
    Webhook {
        #[command(subcommand)]
        action: WebhookAction,
    },
    /// Diagnose state inconsistencies between SQLite and GitHub
    Doctor {
        /// Run a full (expensive) audit including historical done tasks
        #[arg(long)]
        full: bool,
        /// Attempt automatic repairs for fixable issues
        #[arg(long)]
        fix: bool,
        /// Show what --fix would do without applying changes
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove orphaned worktrees not owned by any task
    Prune {
        /// Show what would be removed without making changes
        #[arg(long)]
        dry_run: bool,
    },
    /// Export task session output in human-readable format
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Send a Telegram notification message
    ///
    /// Agents can use this to notify about job results:
    ///
    ///   orch notify "Backup completed successfully"
    ///   orch notify --target 602365815 "Deploy failed"
    ///
    /// Falls back to channels.telegram.chat_id from config when --target is omitted.
    Notify {
        /// Message text to send
        message: String,
        /// Telegram chat_id override (falls back to channels.telegram.chat_id from config)
        #[arg(long, short = 't')]
        target: Option<String>,
    },
}

#[derive(Subcommand)]
enum CooldownAction {
    /// List all active agent and model cooldowns
    List,
    /// Clear a specific cooldown or all cooldowns
    Clear {
        /// Key to clear (e.g. "claude" or "claude:sonnet")
        key: Option<String>,
        /// Clear all active cooldowns
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum WebhookAction {
    /// Show webhook server health status
    Status,
}

#[derive(Subcommand)]
enum SessionAction {
    /// Export session output for a task in human-readable format
    Export {
        /// Task ID (e.g. "internal:8" or issue number)
        task_id: String,
        /// Attempt number (defaults to latest)
        #[arg(long, short = 'a')]
        attempt: Option<u32>,
        /// Output format: markdown (default), json, raw
        #[arg(long, short = 'f', default_value = "markdown")]
        format: String,
    },
}

#[derive(Subcommand)]
enum ChatAction {
    /// Search conversation history
    History {
        /// Search term
        #[arg(long)]
        search: Option<String>,
        /// Only show messages from the last N duration (e.g. 7d, 24h, 30m)
        #[arg(long)]
        since: Option<String>,
        /// Max results
        #[arg(long, default_value = "20")]
        limit: i64,
        /// Include cost per message
        #[arg(long)]
        with_cost: bool,
    },
    /// Show session cost statistics
    Stats,
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
        /// Show tasks across all configured projects
        #[arg(short = 'g', long = "global")]
        global: bool,
        /// Filter to a specific project repo slug or name
        #[arg(long)]
        project: Option<String>,
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
        /// Force routing to a specific agent (e.g. claude, codex, opencode)
        #[arg(long)]
        agent: Option<String>,
        /// Force routing to a specific model (e.g. opus, sonnet). Requires --agent.
        #[arg(long, requires = "agent")]
        model: Option<String>,
    },
    /// Reroute a task to a different agent/model (alias for retry with forced routing)
    Reroute {
        /// Task ID
        id: i64,
        /// Force routing to a specific agent (e.g. claude, codex, opencode)
        #[arg(long)]
        agent: Option<String>,
        /// Force routing to a specific model (e.g. opus, sonnet). Requires --agent.
        #[arg(long, requires = "agent")]
        model: Option<String>,
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
    /// Show task tree view (parent-child relationships)
    Tree {
        /// Task ID (if omitted, shows all root tasks)
        id: Option<i64>,
    },
    /// Show logs / post-mortem for a task (internal or external)
    Logs {
        /// Task ID (e.g. "internal:8" or issue number)
        id: String,
    },
    /// Show task activity timeline
    Log {
        /// Task ID (e.g. "internal:8" or issue number)
        id: String,
        /// Max number of events to show
        #[arg(long)]
        limit: Option<usize>,
        /// Print raw JSON details
        #[arg(long)]
        json: bool,
    },
    /// Show task routing history with attempt timeline
    History {
        /// Task ID (e.g. "internal:8" or issue number)
        id: String,
    },
    /// Show task run history and audit details
    Runs {
        /// Task ID (e.g. "internal:8" or issue number)
        id: String,
        /// Show full stdout/stderr/parsed response for each run
        #[arg(long)]
        verbose: bool,
    },
    /// Mark a task as done (without running an agent)
    Close {
        /// Task ID (e.g. "internal:8" or issue number)
        id: String,
        /// Optional note to add as a comment on external tasks
        #[arg(short, long)]
        note: Option<String>,
    },
    /// Watch a task's status changes in real-time
    Watch {
        /// Task ID
        id: String,
    },
    /// Reopen a done/blocked task (resets status, reopens GitHub issue, syncs labels)
    Reopen {
        /// Task ID (e.g. "1234" or "internal:8")
        id: String,
    },
    /// Unified diagnostic view: status, agent, attempts, last error, PR, block reason
    Inspect {
        /// Task ID (e.g. "internal:8" or issue number)
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
    /// Run a single job immediately (ignoring schedule)
    Run {
        /// Job ID
        id: String,
        /// Project filter (repo slug substring) to disambiguate jobs with the same ID
        #[arg(short, long)]
        project: Option<String>,
    },
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
    /// Start the orch service
    Start,
    /// Stop the orch service
    Stop,
    /// Restart the orch service
    Restart,
    /// Show service status
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Augment PATH with common tool locations.
    // launchd services inherit a minimal PATH (/usr/bin:/bin:/usr/sbin:/sbin)
    // that excludes Homebrew, Cargo, npm globals, etc.
    ensure_path();

    // Load private environment variables from ~/.private into the process.
    // This makes tokens (KIMI_API_KEY, etc.) available to all child processes
    // (router, agents) without requiring the shell to source the file first.
    load_private_env();

    let cli = Cli::parse();

    if matches!(cli.command, Commands::Serve) {
        rotate_service_logs_if_needed();
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("orch=info".parse()?),
        )
        .init();

    match cli.command {
        Commands::Serve => {
            let _span = tracing::info_span!(concat!("orch/", env!("ORCH_VERSION"))).entered();
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
        Commands::Config { key } => {
            let val = config::get(&key)?;
            println!("{val}");
        }
        Commands::Stream {
            task_id,
            formatted,
            raw,
            pipe,
        } => {
            let formatted = if raw { false } else { formatted };
            if pipe {
                // Read NDJSON from stdin and format lines
                cli::stream_pipe()?;
                return Ok(());
            }
            match task_id {
                Some(id) => cli::stream_task(&id, formatted).await?,
                None => cli::stream_all(formatted).await?,
            }
        }
        Commands::Chat {
            action,
            message,
            session,
        } => match action {
            Some(ChatAction::History {
                search,
                since,
                limit,
                with_cost,
            }) => {
                cli::chat::history(&session, search, since, limit, with_cost).await?;
            }
            Some(ChatAction::Stats) => {
                cli::chat::stats(&session).await?;
            }
            None if !message.is_empty() => {
                cli::chat::single_message(&session, &message.join(" ")).await?;
            }
            None => {
                cli::chat::interactive(&session).await?;
            }
        },
        Commands::Template { path, vars } => {
            template::render_and_print(&path, &vars)?;
        }
        Commands::Task { action } => match action {
            TaskAction::List {
                status,
                source,
                global,
                project,
            } => {
                cli::task::list(status, source, global, project).await?;
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
            TaskAction::Retry { id, agent, model } => {
                cli::task::retry(id, agent, model).await?;
            }
            TaskAction::Reroute { id, agent, model } => {
                cli::task::retry(id, agent, model).await?;
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
                cli::task::cost(&id).await?;
            }
            TaskAction::Tree { id } => {
                cli::task::tree(id).await?;
            }
            TaskAction::Logs { id } => {
                cli::task::logs(&id).await?;
            }
            TaskAction::Log { id, limit, json } => {
                cli::task::activity_log(&id, limit, json).await?;
            }
            TaskAction::History { id } => {
                cli::task::history(&id).await?;
            }
            TaskAction::Runs { id, verbose } => {
                cli::task::runs(&id, verbose).await?;
            }
            TaskAction::Close { id, note } => {
                cli::task::close(&id, note.as_deref()).await?;
            }
            TaskAction::Watch { id } => {
                cli::events::stream(None, Some(&id)).await?;
            }
            TaskAction::Reopen { id } => {
                cli::task::reopen(&id).await?;
            }
            TaskAction::Inspect { id } => {
                cli::task::inspect(&id).await?;
            }
        },
        Commands::Job { action } => match action {
            JobAction::List => {
                cli::job::list().await?;
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
            JobAction::Run { id, project } => {
                cli::job::run(&id, project.as_deref()).await?;
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
        Commands::Metrics { details, since } => {
            let hours = cli::parse_since(&since).unwrap_or_else(|| {
                eprintln!(
                    "Warning: unrecognised --since value {:?}, defaulting to 24h",
                    since
                );
                24
            });
            cli::metrics(details, hours).await?;
        }
        // Combined dashboard view: tasks, sessions, recent activity
        Commands::Dashboard { global, project } => {
            cli::dashboard::dashboard(global, project).await?;
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
        Commands::Cooldown { action } => match action {
            CooldownAction::List => {
                cli::cooldown::list().await?;
            }
            CooldownAction::Clear { key, all } => {
                cli::cooldown::clear(key, all).await?;
            }
        },
        Commands::Cost {
            task_id,
            summary,
            agent,
            model,
        } => {
            if let Some(id) = task_id {
                cli::cost::show_task(&id).await?;
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
        Commands::Stats { all, since } => {
            let hours = cli::parse_since(&since).unwrap_or_else(|| {
                eprintln!(
                    "Warning: unrecognised --since value {:?}, defaulting to 24h",
                    since
                );
                24
            });
            cli::stats::stats(all, hours).await?;
        }
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            generate(shell, &mut cmd, "orch", &mut std::io::stdout());
        }
        Commands::Events { repo, task } => {
            cli::events::stream(repo.as_deref(), task.as_deref()).await?;
        }
        Commands::Webhook { action } => match action {
            WebhookAction::Status => {
                cli::webhook::status()?;
            }
        },
        Commands::Doctor { full, fix, dry_run } => {
            cli::doctor::run(full, fix, dry_run).await?;
        }
        Commands::Prune { dry_run } => {
            cli::prune::run(dry_run).await?;
        }
        Commands::Session { action } => match action {
            SessionAction::Export {
                task_id,
                attempt,
                format,
            } => {
                use crate::cli::session::ExportFormat;
                let fmt = format
                    .parse::<ExportFormat>()
                    .unwrap_or(ExportFormat::Markdown);
                cli::session::export(&task_id, attempt, fmt).await?;
            }
        },
        Commands::Notify { message, target } => {
            cli::notify::send(&message, target.as_deref()).await?;
        }
    }

    Ok(())
}

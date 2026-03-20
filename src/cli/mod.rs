pub mod chat;
pub mod cost;
pub mod dashboard;
pub mod job;
pub mod service;
pub mod stats;
pub mod task;
pub mod tree;
pub mod webhook;

use crate::channels::capture::CaptureService;
use crate::channels::transport::Transport;
use crate::channels::OutputChunk;
use crate::cmd::SyncCommandErrorContext;
use crate::config;
use crate::engine::tasks::TaskManager;
use anyhow::Context;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Print version information.
pub fn version() {
    println!("orch {}", env!("ORCH_VERSION"));
}

/// Initialize orchestrator for a project.
pub fn init(repo: Option<String>) -> anyhow::Result<()> {
    let orch_home = crate::home::orch_home()?;
    std::fs::create_dir_all(&orch_home)?;

    let config_path = orch_home.join("config.yml");

    let repo_value = match repo {
        Some(r) => {
            // Normalize: accept SSH URLs, HTTPS URLs, or owner/repo slugs
            if let Some((owner, name)) = parse_github_slug(&r) {
                format!("{}/{}", owner, name)
            } else {
                r
            }
        }
        None => {
            // Try to detect from git remote (origin) by reading the remote URL
            let git_remote = std::process::Command::new("git")
                .args(["config", "--get", "remote.origin.url"])
                .output_with_context();

            match git_remote {
                Ok(o) if o.status.success() => {
                    let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if let Some((owner, repo)) = parse_github_slug(&url) {
                        format!("{}/{}", owner, repo)
                    } else {
                        eprintln!("Could not parse repository from git remote: {}", url);
                        eprintln!("Use --repo OWNER/REPO");
                        std::process::exit(1);
                    }
                }
                _ => {
                    eprintln!("Could not detect repository. Use --repo OWNER/REPO");
                    std::process::exit(1);
                }
            }
        }
    };

    // Ensure global config exists
    if !config_path.exists() {
        let content = "# Orch global configuration\n# See: https://github.com/gabrielkoerich/orch\n\nprojects: []\n\nrouter:\n  mode: llm\n  agent: claude\n  model: haiku\n";
        std::fs::write(&config_path, content)?;
    }

    println!("Initialized orch for {repo_value}");
    println!("Global config: {}", config_path.display());

    // Create project-local .orch.yml if not exists
    let local_config = std::path::Path::new(".orch.yml");
    if !local_config.exists() {
        std::fs::write(
            local_config,
            format!("# Project-specific orch config\ngh:\n  repo: \"{repo_value}\"\n"),
        )?;
        println!("Created .orch.yml");
    }

    // Register project in global config
    let cwd = std::env::current_dir()?;
    let cwd_str = cwd.to_string_lossy().to_string();

    // Check if already registered
    let paths = config::get_project_paths().unwrap_or_default();
    if !paths.iter().any(|p| p == &cwd_str) {
        project_add(".")?;
    } else {
        println!("Project already registered in global config");
    }

    // Install orch-review workflow
    install_review_workflow(&cwd)?;

    // Guidance for board setup
    println!();
    println!("Next steps:");
    println!("  orch board list     — find GitHub Projects V2 boards");
    println!("  orch board link <id> — link a board for status tracking");

    Ok(())
}

/// The orch review gate workflow template, loaded from the repo's own workflow file.
/// Installed by `orch init` into `.github/workflows/orch-review.yml`.
const ORCH_REVIEW_WORKFLOW: &str = include_str!("../../.github/workflows/orch-review.yml");

/// Install the orch review gate workflow into the project.
fn install_review_workflow(project_dir: &std::path::Path) -> anyhow::Result<()> {
    let workflows_dir = project_dir.join(".github").join("workflows");
    let workflow_path = workflows_dir.join("orch-review.yml");

    if workflow_path.exists() {
        println!(
            "Review workflow already exists: {}",
            workflow_path.display()
        );
        return Ok(());
    }

    std::fs::create_dir_all(&workflows_dir)?;
    std::fs::write(&workflow_path, ORCH_REVIEW_WORKFLOW)?;
    println!(
        "Installed review gate workflow: {}",
        workflow_path.display()
    );

    Ok(())
}

/// Show orchestrator logs.
pub fn log(lines: &str) -> anyhow::Result<()> {
    let state_dir = crate::home::state_dir().unwrap_or_default();
    let brew_prefix = std::env::var("HOMEBREW_PREFIX").unwrap_or_else(|_| "/opt/homebrew".into());

    let mut log_files = Vec::new();

    let candidates = [
        state_dir.join("orch.log"),
        state_dir.join("orch.error.log"),
        std::path::PathBuf::from(&brew_prefix).join("var/log/orch.log"),
        std::path::PathBuf::from(&brew_prefix).join("var/log/orch.error.log"),
        // Legacy paths
        std::path::PathBuf::from(&brew_prefix).join("var/log/orchestrator.log"),
        std::path::PathBuf::from(&brew_prefix).join("var/log/orchestrator.error.log"),
    ];

    for path in &candidates {
        if path.exists()
            && std::fs::metadata(path)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        {
            log_files.push(path.clone());
        }
    }

    if log_files.is_empty() {
        println!("No log files found");
        return Ok(());
    }

    if lines == "watch" {
        let args: Vec<String> = std::iter::once("-f".to_string())
            .chain(log_files.iter().map(|p| p.to_string_lossy().to_string()))
            .collect();
        let status = std::process::Command::new("tail")
            .args(&args)
            .status_with_context()?;
        std::process::exit(status.code().unwrap_or(1));
    } else {
        let n = lines.parse::<usize>().unwrap_or(50);
        for path in &log_files {
            let filename = path.file_name().unwrap_or_default().to_string_lossy();
            println!("=== {filename} ===");

            let content = std::fs::read_to_string(path)?;
            let all_lines: Vec<&str> = content.lines().collect();
            let start = if all_lines.len() > n {
                all_lines.len() - n
            } else {
                0
            };
            for line in &all_lines[start..] {
                println!("{line}");
            }
            println!();
        }
    }

    Ok(())
}

/// List installed agent CLIs.
pub fn agents() {
    let agents = ["claude", "codex", "opencode", "kimi", "minimax"];

    println!("{:<12} {:<10} PATH", "AGENT", "STATUS");
    println!("{}", "-".repeat(60));

    for agent in &agents {
        match which::which(agent) {
            Ok(path) => {
                // Try to get version
                let version = std::process::Command::new(agent)
                    .arg("--version")
                    .output_with_context()
                    .ok()
                    .and_then(|o| {
                        if o.status.success() {
                            Some(
                                String::from_utf8_lossy(&o.stdout)
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .trim()
                                    .to_string(),
                            )
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                let info = if version.is_empty() {
                    path.display().to_string()
                } else {
                    format!("{} ({})", path.display(), version)
                };
                println!("{:<12} {:<10} {}", agent, "installed", info);
            }
            Err(_) => {
                println!("{:<12} {:<10} ", agent, "missing");
            }
        }
    }
}

/// Show task metrics summary.
pub async fn metrics(details: bool) -> anyhow::Result<()> {
    let store = init_store().await?;

    let summary = store.get_metrics_summary_24h().await?;

    println!();
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║              Orch Metrics (Last 24 Hours)               ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();

    // Task counts
    println!(" Tasks:");
    println!("   {:>6} completed", summary.tasks_completed_24h);
    println!("   {:>6} failed", summary.tasks_failed_24h);
    println!();

    // Average duration by complexity
    println!(" Average Duration by Complexity:");
    if let Some(d) = summary.avg_duration_simple {
        println!("   {:>6.1}s (simple)", d);
    } else {
        println!("   {:>6} (simple)", "-");
    }
    if let Some(d) = summary.avg_duration_medium {
        println!("   {:>6.1}s (medium)", d);
    } else {
        println!("   {:>6} (medium)", "-");
    }
    if let Some(d) = summary.avg_duration_complex {
        println!("   {:>6.1}s (complex)", d);
    } else {
        println!("   {:>6} (complex)", "-");
    }
    println!();

    // Agent success rates
    if !summary.agent_stats.is_empty() {
        println!(" Agent Success Rates:");
        for stat in &summary.agent_stats {
            println!(
                "   {:<12} {:>4} runs, {:>5.1}% success",
                stat.agent, stat.total_runs, stat.success_rate
            );
        }
        println!();
    }

    // Rate limits
    println!(" Rate Limit Events: {:>6}", summary.rate_limits_24h);
    println!();

    if details {
        // Slow tasks (last 7 days)
        let slow = store.get_slow_tasks_7d().await?;
        if !slow.is_empty() {
            println!(" Slowest Tasks (Last 7 Days):");
            println!(
                "   {:<20} {:<12} {:<10} DURATION",
                "TASK ID", "AGENT", "COMPLEXITY"
            );
            println!("   {}", "-".repeat(56));
            for t in &slow {
                println!(
                    "   {:<20} {:<12} {:<10} {:.1}s",
                    t.task_id,
                    t.agent,
                    t.complexity.as_deref().unwrap_or("-"),
                    t.duration_seconds
                );
            }
            println!();
        }

        // Error distribution (last 7 days)
        let errors = store.get_error_distribution_7d().await?;
        if !errors.is_empty() {
            println!(" Error Distribution (Last 7 Days):");
            println!("   {:<30} {:>6}", "ERROR TYPE", "COUNT");
            println!("   {}", "-".repeat(38));
            for e in &errors {
                println!(
                    "   {:<30} {:>6}",
                    e.error_type.as_deref().unwrap_or("unknown"),
                    e.count
                );
            }
            println!();
        } else {
            println!(" Error Distribution (Last 7 Days): none");
            println!();
        }
    }

    Ok(())
}

/// Stream live output from a running task.
pub async fn stream_task(task_id: &str) -> anyhow::Result<()> {
    let transport = Arc::new(Transport::new());

    let tmux = crate::tmux::TmuxManager::new();
    let repo = crate::config::get_current_repo().unwrap_or_default();
    let session_name = tmux.session_name(&repo, task_id);
    transport
        .bind(task_id, &session_name, "cli", "stream")
        .await;

    // Start a CaptureService to poll the tmux session and push chunks to transport.
    // This is necessary because the CLI's Transport is isolated from the engine's.
    let capture = Arc::new(CaptureService::new(transport.clone()));
    capture.register_session(task_id, &session_name).await;
    let capture_handle = tokio::spawn({
        let capture = capture.clone();
        async move { capture.run().await }
    });

    let mut rx = match transport.subscribe(task_id).await {
        Some(rx) => rx,
        None => {
            capture_handle.abort();
            anyhow::bail!("no active session for task {}", task_id);
        }
    };

    println!(
        "Streaming output from task {} (session: {})",
        task_id, session_name
    );
    println!("Press Ctrl+C to stop streaming");
    println!("---");

    loop {
        match rx.recv().await {
            Ok(chunk) => {
                print!("{}", chunk.content);
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

    // Clean up: unregister and stop capture
    capture.unregister_session(task_id).await;
    capture_handle.abort();

    Ok(())
}

/// Stream output from all running orch tmux sessions.
///
/// Discovers sessions on startup and every tick, automatically picking up
/// new sessions that appear while streaming. Prefixes each line with the
/// session name so interleaved output is distinguishable.
pub async fn stream_all() -> anyhow::Result<()> {
    use std::collections::HashSet;
    use tokio::sync::broadcast;

    let transport = Arc::new(crate::channels::transport::Transport::new());
    let capture = Arc::new(crate::channels::capture::CaptureService::new(
        transport.clone(),
    ));

    // Track which sessions we've already registered
    let known: Arc<tokio::sync::Mutex<HashSet<String>>> =
        Arc::new(tokio::sync::Mutex::new(HashSet::new()));

    // Discover sessions and register any new ones. Returns list of newly added session names.
    let discover = {
        let transport = transport.clone();
        let capture = capture.clone();
        let known = known.clone();
        move || {
            let transport = transport.clone();
            let capture = capture.clone();
            let known = known.clone();
            async move {
                let sessions = crate::channels::tmux::list_orch_sessions()
                    .await
                    .unwrap_or_default();
                let mut added = Vec::new();
                let mut known = known.lock().await;
                for session in sessions {
                    if known.contains(&session) {
                        continue;
                    }
                    // Use session name as the task key (unique, descriptive)
                    transport
                        .bind(&session, &session, "cli", "stream-all")
                        .await;
                    capture.register_session(&session, &session).await;
                    known.insert(session.clone());
                    added.push(session);
                }
                added
            }
        }
    };

    // Initial discovery
    let initial = discover().await;
    if initial.is_empty() {
        println!("No running orch sessions found. Waiting for sessions...");
    } else {
        println!(
            "Streaming {} session(s): {}",
            initial.len(),
            initial.join(", ")
        );
    }
    println!("Press Ctrl+C to stop");
    println!("---");

    // Spawn capture loop (runs while sessions are registered, but we keep re-discovering)
    let capture_handle = tokio::spawn({
        let capture = capture.clone();
        async move { capture.start().await }
    });

    // Spawn discovery loop — checks for new sessions every 3 seconds
    let discover_handle = tokio::spawn({
        let discover = discover.clone();
        async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
            loop {
                interval.tick().await;
                let added = discover().await;
                for s in &added {
                    eprintln!("+ new session: {s}");
                }
            }
        }
    });

    // Merge output from all sessions via a single aggregated channel.
    // We poll transport bindings periodically and subscribe to new ones.
    let (merged_tx, mut merged_rx) = tokio::sync::mpsc::channel::<(String, OutputChunk)>(512);

    // Spawn subscriber watcher — subscribes to broadcast channels of new sessions
    let subscriber_handle = tokio::spawn({
        let transport = transport.clone();
        let known_subs: Arc<tokio::sync::Mutex<HashSet<String>>> =
            Arc::new(tokio::sync::Mutex::new(HashSet::new()));
        let merged_tx = merged_tx.clone();
        async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                let bindings = transport.active_sessions().await;
                for binding in bindings {
                    let session = binding.tmux_session.clone();
                    let mut subs = known_subs.lock().await;
                    if subs.contains(&session) {
                        continue;
                    }
                    subs.insert(session.clone());
                    let mut rx = binding.output_tx.subscribe();
                    let tx = merged_tx.clone();
                    tokio::spawn(async move {
                        loop {
                            match rx.recv().await {
                                Ok(chunk) => {
                                    if tx.send((session.clone(), chunk)).await.is_err() {
                                        break;
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    });
                }
            }
        }
    });

    // Drop our copy so the channel closes when all senders are gone
    drop(merged_tx);

    // Print merged output with session prefix
    while let Some((session, chunk)) = merged_rx.recv().await {
        if chunk.content.is_empty() && chunk.is_final {
            eprintln!("- session ended: {session}");
            continue;
        }
        // Prefix each line with the session name (strip the "orch-" prefix for brevity)
        let label = session.strip_prefix("orch-").unwrap_or(&session);
        for line in chunk.content.lines() {
            println!("[{label}] {line}");
        }
        // If content didn't end with newline, don't add extra one
        if !chunk.content.is_empty() && !chunk.content.ends_with('\n') {
            std::io::Write::flush(&mut std::io::stdout())?;
        }
    }

    // Clean up
    capture_handle.abort();
    discover_handle.abort();
    subscriber_handle.abort();

    Ok(())
}

/// List accessible GitHub Projects V2 boards.
pub async fn board_list() -> anyhow::Result<()> {
    use crate::github::projects::ProjectSync;

    let projects = ProjectSync::list_projects().await?;

    if projects.is_empty() {
        println!("No GitHub Projects V2 boards found");
        return Ok(());
    }

    println!("{:<50} {:<8} ID", "TITLE", "NUMBER");
    println!("{}", "-".repeat(80));
    for p in &projects {
        println!("{:<50} #{:<7} {}", p.title, p.number, p.id);
    }

    Ok(())
}

/// Link current repo to a GitHub Projects V2 board by ID and discover fields.
pub async fn board_link(project_id: &str) -> anyhow::Result<()> {
    use crate::github::projects::{write_project_config, ProjectSync};

    println!("Discovering board fields...");
    let sync = ProjectSync::discover_fields(project_id).await?;

    write_project_config(&sync)?;

    println!("Linked board: {}", project_id);
    println!("Status field: {}", sync.status_field_id());
    println!("Column mappings:");
    for (col, opt_id) in sync.status_map() {
        println!("  {col}: {opt_id}");
    }

    Ok(())
}

/// Re-discover field IDs from configured board and update config.
pub async fn board_sync() -> anyhow::Result<()> {
    use crate::github::projects::{write_project_config, ProjectSync};

    let project_id = config::get("gh.project_id")
        .map_err(|_| anyhow::anyhow!("no board configured — run `orch board link <id>` first"))?;

    if project_id.is_empty() {
        anyhow::bail!("no board configured — run `orch board link <id>` first");
    }

    println!("Syncing board fields for {}...", project_id);
    let sync = ProjectSync::discover_fields(&project_id).await?;

    write_project_config(&sync)?;

    println!("Updated config:");
    println!("  Status field: {}", sync.status_field_id());
    for (col, opt_id) in sync.status_map() {
        println!("  {col}: {opt_id}");
    }

    Ok(())
}

/// Show current board configuration.
pub fn board_info() -> anyhow::Result<()> {
    let project_id = config::get("gh.project_id").unwrap_or_default();

    if project_id.is_empty() {
        println!("No board configured");
        println!("  Run `orch board list` to see available boards");
        println!("  Run `orch board link <id>` to link one");
        return Ok(());
    }

    println!("Board ID: {}", project_id);

    if let Ok(field_id) = config::get("gh.project_status_field_id") {
        println!("Status field: {}", field_id);
    }

    for col in &["backlog", "in_progress", "review", "done"] {
        if let Ok(opt_id) = config::get(&format!("gh.project_status_map.{col}")) {
            println!("  {col}: {opt_id}");
        }
    }

    Ok(())
}

/// Try to parse a GitHub slug (owner/repo) from the input.
///
/// Accepts:
/// - `owner/repo` — direct slug
/// - `https://github.com/owner/repo` — GitHub URL (with optional .git suffix)
///
/// Returns `None` if the input looks like a local path.
fn parse_github_slug(input: &str) -> Option<(String, String)> {
    // SSH URL format: git@github.com:owner/repo.git
    if input.starts_with("git@github.com:") {
        let path = input
            .trim_start_matches("git@github.com:")
            .trim_end_matches('/')
            .trim_end_matches(".git");
        let parts: Vec<&str> = path.splitn(3, '/').collect();
        if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
        return None;
    }

    // GitHub URL format
    if input.starts_with("https://github.com/") || input.starts_with("http://github.com/") {
        let path = input
            .trim_start_matches("https://github.com/")
            .trim_start_matches("http://github.com/")
            .trim_end_matches('/')
            .trim_end_matches(".git");
        let parts: Vec<&str> = path.splitn(3, '/').collect();
        if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
        return None;
    }

    // Skip anything that looks like a local path
    if input.starts_with('/')
        || input.starts_with('.')
        || input.starts_with('~')
        || (input.contains(std::path::MAIN_SEPARATOR) && std::path::MAIN_SEPARATOR != '/')
    {
        return None;
    }

    // owner/repo slug (exactly one slash, no path-like characters)
    let parts: Vec<&str> = input.splitn(3, '/').collect();
    if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
        // Sanity check: slugs don't contain spaces or typical path characters
        let owner = parts[0];
        let repo = parts[1].trim_end_matches(".git");
        if !owner.contains(' ') && !repo.contains(' ') && !repo.contains('/') {
            return Some((owner.to_string(), repo.to_string()));
        }
    }

    None
}

/// Add a project to the global registry.
///
/// Accepts:
/// - A local path (existing behavior)
/// - A GitHub slug (`owner/repo`) — auto-clones as a bare repo
/// - A GitHub URL (`https://github.com/owner/repo`) — auto-clones as a bare repo
pub fn project_add(input: &str) -> anyhow::Result<()> {
    // Check if this is a GitHub slug or URL
    if let Some((owner, repo)) = parse_github_slug(input) {
        return project_add_github(&owner, &repo);
    }

    // Local path (existing behavior)
    project_add_local(input)
}

/// Add a local project path to the global registry.
fn project_add_local(path: &str) -> anyhow::Result<()> {
    let abs_path = if path == "." {
        std::env::current_dir()?
    } else {
        std::path::PathBuf::from(path).canonicalize()?
    };

    // Verify .orch.yml exists in the project
    let orch_yml = abs_path.join(".orch.yml");
    if !orch_yml.exists() {
        // Check for legacy .orchestrator.yml
        let legacy = abs_path.join(".orchestrator.yml");
        if legacy.exists() {
            println!("Found .orchestrator.yml — consider renaming to .orch.yml");
        } else {
            anyhow::bail!(
                "no .orch.yml found in {} — run `orch init` in the project first",
                abs_path.display()
            );
        }
    }

    let path_str = abs_path.to_string_lossy().to_string();
    register_project_path(&path_str)?;

    // Show the repo from .orch.yml if available
    if orch_yml.exists() {
        let project_content = std::fs::read_to_string(&orch_yml)?;
        let project_doc: serde_yml::Value = serde_yml::from_str(&project_content)?;
        if let Some(repo) = project_doc
            .get("gh")
            .and_then(|gh| gh.get("repo"))
            .and_then(|r| r.as_str())
        {
            println!("  repo: {}", repo);
        }
    }

    Ok(())
}

/// Clone a GitHub repo as a bare clone and register it.
fn project_add_github(owner: &str, repo: &str) -> anyhow::Result<()> {
    let projects_dir = crate::home::projects_dir()?;
    let bare_path = projects_dir.join(owner).join(format!("{repo}.git"));
    let slug = format!("{owner}/{repo}");

    if bare_path.exists() {
        println!("Bare clone already exists: {}", bare_path.display());
    } else {
        // Create parent directory
        let parent = bare_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("bare path has no parent: {}", bare_path.display()))?;
        std::fs::create_dir_all(parent)?;

        println!("Cloning {slug} as bare repo...");
        let status = std::process::Command::new("gh")
            .args([
                "repo",
                "clone",
                &slug,
                &bare_path.to_string_lossy(),
                "--",
                "--bare",
            ])
            .status_with_context()?;

        if !status.success() {
            anyhow::bail!("gh repo clone failed for {slug}");
        }

        println!("Cloned to {}", bare_path.display());
    }

    // Create .orch.yml inside the bare clone if it doesn't exist
    let orch_yml = bare_path.join(".orch.yml");
    if !orch_yml.exists() {
        let content = format!("# Project-specific orch config\ngh:\n  repo: \"{slug}\"\n");
        std::fs::write(&orch_yml, content)?;
        println!("Created .orch.yml with gh.repo: {slug}");
    }

    // Register in global config
    let path_str = bare_path.to_string_lossy().to_string();
    register_project_path(&path_str)?;
    println!("  repo: {slug}");

    Ok(())
}

/// Register a project path in the global config (shared by local and GitHub flows).
fn register_project_path(path_str: &str) -> anyhow::Result<()> {
    let config_path = crate::home::config_path()?;
    let content = if config_path.exists() {
        std::fs::read_to_string(&config_path)?
    } else {
        String::new()
    };

    let mut doc: serde_yml::Value = if content.is_empty() {
        serde_yml::Value::Mapping(serde_yml::Mapping::new())
    } else {
        serde_yml::from_str(&content)?
    };

    let root = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("config is not a YAML mapping"))?;

    // Get or create projects list
    let projects_key = serde_yml::Value::String("projects".to_string());
    if !root.contains_key(&projects_key) {
        root.insert(projects_key.clone(), serde_yml::Value::Sequence(Vec::new()));
    }

    let projects = root
        .get_mut(&projects_key)
        .and_then(|v| v.as_sequence_mut())
        .ok_or_else(|| anyhow::anyhow!("projects is not a list"))?;

    // Check for duplicates
    let already_exists = projects
        .iter()
        .any(|p| p.as_str().map(|s| s == path_str).unwrap_or(false));

    if already_exists {
        println!("Project already registered: {}", path_str);
        return Ok(());
    }

    projects.push(serde_yml::Value::String(path_str.to_string()));
    std::fs::write(&config_path, serde_yml::to_string(&doc)?)?;

    println!("Added project: {}", path_str);

    Ok(())
}

/// Remove a project from the global registry.
pub fn project_remove(path: &str) -> anyhow::Result<()> {
    let abs_path = std::path::PathBuf::from(path)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(path));
    let path_str = abs_path.to_string_lossy().to_string();

    let config_path = crate::home::config_path()?;
    if !config_path.exists() {
        anyhow::bail!("no global config found");
    }

    let content = std::fs::read_to_string(&config_path)?;
    let mut doc: serde_yml::Value = serde_yml::from_str(&content)?;

    let root = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("config is not a YAML mapping"))?;

    let projects_key = serde_yml::Value::String("projects".to_string());
    let projects = root
        .get_mut(&projects_key)
        .and_then(|v| v.as_sequence_mut())
        .ok_or_else(|| anyhow::anyhow!("no projects list in config"))?;

    let before_len = projects.len();
    projects.retain(|p| {
        p.as_str()
            .map(|s| s != path_str && s != path)
            .unwrap_or(true)
    });

    if projects.len() == before_len {
        println!("Project not found: {}", path);
        return Ok(());
    }

    std::fs::write(&config_path, serde_yml::to_string(&doc)?)?;
    println!("Removed project: {}", path_str);

    Ok(())
}

/// List all registered projects.
pub fn project_list() -> anyhow::Result<()> {
    let projects = config::get_project_paths()?;

    if projects.is_empty() {
        println!("No projects registered");
        println!("  Run `orch project add <path>` to register a project");
        return Ok(());
    }

    println!("{:<50} REPO", "PATH");
    println!("{}", "-".repeat(80));

    for path_str in &projects {
        let path = std::path::Path::new(path_str);

        // Try to read repo from .orch.yml
        let repo = read_project_repo(path).unwrap_or_else(|| "(.orch.yml not found)".to_string());

        let status = if path.exists() { "" } else { " (missing)" };

        println!("{:<50} {}{}", path_str, repo, status);
    }

    Ok(())
}

/// Read gh.repo from a project's .orch.yml.
fn read_project_repo(project_path: &std::path::Path) -> Option<String> {
    let orch_yml = project_path.join(".orch.yml");
    let content = std::fs::read_to_string(&orch_yml).ok()?;
    let doc: serde_yml::Value = serde_yml::from_str(&content).ok()?;
    doc.get("gh")
        .and_then(|gh| gh.get("repo"))
        .and_then(|r| r.as_str())
        .map(String::from)
}

/// Initialize task manager with database and backend.
pub async fn init_task_manager() -> anyhow::Result<TaskManager> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::store::TaskStore;

    let repo = config::get_current_repo()
        .context("'repo' not set — run `orch init` or set gh.repo in .orch.yml")?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
    let store = Arc::new(TaskStore::open(&crate::store::default_db_path()?).await?);
    Ok(TaskManager::with_store(backend, store, repo))
}

/// Initialize the unified task store for CLI commands.
pub async fn init_store() -> anyhow::Result<crate::store::TaskStore> {
    crate::store::TaskStore::open(&crate::store::default_db_path()?).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slug_owner_repo() {
        let result = parse_github_slug("gabrielkoerich/orch");
        assert_eq!(
            result,
            Some(("gabrielkoerich".to_string(), "orch".to_string()))
        );
    }

    #[test]
    fn parse_slug_https_url() {
        let result = parse_github_slug("https://github.com/gabrielkoerich/orch");
        assert_eq!(
            result,
            Some(("gabrielkoerich".to_string(), "orch".to_string()))
        );
    }

    #[test]
    fn parse_slug_https_url_with_git_suffix() {
        let result = parse_github_slug("https://github.com/gabrielkoerich/orch.git");
        assert_eq!(
            result,
            Some(("gabrielkoerich".to_string(), "orch".to_string()))
        );
    }

    #[test]
    fn parse_slug_https_url_trailing_slash() {
        let result = parse_github_slug("https://github.com/gabrielkoerich/orch/");
        assert_eq!(
            result,
            Some(("gabrielkoerich".to_string(), "orch".to_string()))
        );
    }

    #[test]
    fn parse_slug_absolute_path_returns_none() {
        assert_eq!(parse_github_slug("/Users/gb/Projects/my-app"), None);
    }

    #[test]
    fn parse_slug_relative_path_returns_none() {
        assert_eq!(parse_github_slug("./my-app"), None);
    }

    #[test]
    fn parse_slug_dot_returns_none() {
        assert_eq!(parse_github_slug("."), None);
    }

    #[test]
    fn parse_slug_tilde_path_returns_none() {
        assert_eq!(parse_github_slug("~/Projects/my-app"), None);
    }

    #[test]
    fn parse_slug_single_word_returns_none() {
        assert_eq!(parse_github_slug("my-app"), None);
    }

    #[test]
    fn parse_slug_three_segments_returns_none() {
        // Three segments like a deep path shouldn't match as a slug
        assert_eq!(parse_github_slug("a/b/c"), None);
    }

    #[test]
    fn parse_slug_ssh_url() {
        let result = parse_github_slug("git@github.com:gabrielkoerich/bean.git");
        assert_eq!(
            result,
            Some(("gabrielkoerich".to_string(), "bean".to_string()))
        );
    }

    #[test]
    fn parse_slug_ssh_url_no_git_suffix() {
        let result = parse_github_slug("git@github.com:gabrielkoerich/bean");
        assert_eq!(
            result,
            Some(("gabrielkoerich".to_string(), "bean".to_string()))
        );
    }
}

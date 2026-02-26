pub mod job;
pub mod service;
pub mod task;

use crate::channels::transport::Transport;
use crate::config;
use crate::engine::tasks::TaskManager;
use anyhow::Context;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Print version information.
pub fn version() {
    let pkg_version = env!("CARGO_PKG_VERSION");
    let git_desc = option_env!("ORCH_GIT_DESCRIBE").unwrap_or("unknown");
    println!("orch {pkg_version} ({git_desc})");
}

/// Initialize orchestrator for a project.
pub fn init(repo: Option<String>) -> anyhow::Result<()> {
    let orch_home = crate::home::orch_home()?;
    std::fs::create_dir_all(&orch_home)?;

    let config_path = orch_home.join("config.yml");

    let repo_value = match repo {
        Some(r) => r,
        None => {
            // Try to detect from git remote
            let output = std::process::Command::new("gh")
                .args([
                    "repo",
                    "view",
                    "--json",
                    "nameWithOwner",
                    "-q",
                    ".nameWithOwner",
                ])
                .output();

            match output {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
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

    // Guidance for board setup
    println!();
    println!("Next steps:");
    println!("  orch board list     — find GitHub Projects V2 boards");
    println!("  orch board link <id> — link a board for status tracking");

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
        let status = std::process::Command::new("tail").args(&args).status()?;
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
                    .output()
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
pub async fn metrics() -> anyhow::Result<()> {
    use crate::db::Db;

    let db = Db::open(&crate::db::default_path()?)?;
    db.migrate().await?;

    let summary = db.get_metrics_summary_24h().await?;

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

    Ok(())
}

/// Stream live output from a running task.
pub async fn stream_task(task_id: &str) -> anyhow::Result<()> {
    let transport = Arc::new(Transport::new());

    let session_name = format!("orch-{}", task_id);
    transport
        .bind(task_id, &session_name, "cli", "stream")
        .await;

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

/// Add a project path to the global registry.
pub fn project_add(path: &str) -> anyhow::Result<()> {
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

    // Read current global config
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

    projects.push(serde_yml::Value::String(path_str.clone()));
    std::fs::write(&config_path, serde_yml::to_string(&doc)?)?;

    println!("Added project: {}", path_str);

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
    use crate::db::Db;

    let repo = config::get_current_repo()
        .context("'repo' not set — run `orch init` or set gh.repo in .orch.yml")?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo));
    let db = Arc::new(Db::open(&crate::db::default_path()?)?);
    db.migrate().await?;
    Ok(TaskManager::new(db, backend))
}

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

    if config_path.exists() {
        // Update repo in existing config
        let content = std::fs::read_to_string(&config_path)?;
        if content.contains("repo:") {
            let updated = regex::Regex::new(r"(?m)^repo:.*$")?
                .replace(&content, &format!("repo: {repo_value}"))
                .to_string();
            std::fs::write(&config_path, updated)?;
        } else {
            let mut content = content;
            content.push_str(&format!("\nrepo: {repo_value}\n"));
            std::fs::write(&config_path, content)?;
        }
    } else {
        let content = format!(
            "# Orch configuration\nrepo: {repo_value}\n\nrouter:\n  mode: llm\n  agent: claude\n  model: haiku\n"
        );
        std::fs::write(&config_path, content)?;
    }

    println!("Initialized orch for {repo_value}");
    println!("Config: {}", config_path.display());

    // Create project-local .orch.yml if not exists
    let local_config = std::path::Path::new(".orch.yml");
    if !local_config.exists() {
        std::fs::write(
            local_config,
            format!("# Project-specific orch config\nrepo: {repo_value}\n"),
        )?;
        println!("Created .orch.yml");
    }

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

/// Initialize task manager with database and backend.
pub async fn init_task_manager() -> anyhow::Result<TaskManager> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::db::Db;

    let repo = config::get("repo").context(
        "'repo' not set in config — run `orch init` or set repo in ~/.orch/config.yml",
    )?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo));
    let db = Arc::new(Db::open(&crate::db::default_path()?)?);
    db.migrate().await?;
    Ok(TaskManager::new(db, backend))
}

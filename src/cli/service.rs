use crate::cmd::SyncCommandErrorContext;

/// Run a service subcommand for orch: `brew services` on macOS, the `orch` systemd user unit elsewhere.
fn service(action: &str) -> anyhow::Result<()> {
    let (program, args) = if cfg!(target_os = "macos") {
        ("brew", vec!["services", action, "orch"])
    } else {
        // brew's `info` is systemd's `status`
        let action = if action == "info" { "status" } else { action };
        ("systemctl", vec!["--user", action, "orch"])
    };

    let status = std::process::Command::new(program)
        .args(&args)
        .status_with_context()?;

    if !status.success() {
        anyhow::bail!("{program} {} failed", args.join(" "));
    }

    Ok(())
}

/// Start the orch service.
pub fn start() -> anyhow::Result<()> {
    service("start")
}

/// Stop the orch service.
pub fn stop() -> anyhow::Result<()> {
    service("stop")
}

/// Restart the orch service.
pub fn restart() -> anyhow::Result<()> {
    service("restart")
}

/// Show service status.
pub fn status() -> anyhow::Result<()> {
    service("info")
}

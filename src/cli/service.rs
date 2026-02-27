/// Run a `brew services` subcommand for orch.
fn brew_services(action: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("brew")
        .args(["services", action, "orch"])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run `brew services {action} orch`: {e}"))?;

    if !status.success() {
        anyhow::bail!("brew services {action} orch failed");
    }

    Ok(())
}

/// Start the orchestrator service.
pub fn start() -> anyhow::Result<()> {
    brew_services("start")
}

/// Stop the orchestrator service.
pub fn stop() -> anyhow::Result<()> {
    brew_services("stop")
}

/// Restart the orchestrator service.
pub fn restart() -> anyhow::Result<()> {
    brew_services("restart")
}

/// Show service status.
pub fn status() -> anyhow::Result<()> {
    brew_services("info")
}

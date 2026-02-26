use crate::sidecar;
use std::path::PathBuf;

/// Check if orch was installed via Homebrew.
fn is_brew_installed() -> bool {
    let brew_prefix = std::env::var("HOMEBREW_PREFIX").unwrap_or_else(|_| "/opt/homebrew".into());
    let cellar_path = PathBuf::from(&brew_prefix).join("Cellar/orch");
    cellar_path.exists()
}

fn pid_file() -> PathBuf {
    sidecar::state_file("orch.pid").unwrap_or_else(|_| {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".orchestrator")
            .join("state")
            .join("orch.pid")
    })
}

/// Start the orchestrator service.
pub fn start() -> anyhow::Result<()> {
    if is_brew_installed() {
        println!("Starting via brew services...");
        let status = std::process::Command::new("brew")
            .args(["services", "start", "orch"])
            .status()?;
        if !status.success() {
            anyhow::bail!("brew services start failed");
        }
    } else {
        // Daemonize: fork orch serve
        let orch_bin = std::env::current_exe()?;
        let log_dir = sidecar::state_dir()?;

        let stdout_log = std::fs::File::create(log_dir.join("orchestrator.log"))?;
        let stderr_log = std::fs::File::create(log_dir.join("orchestrator.error.log"))?;

        let child = std::process::Command::new(orch_bin)
            .arg("serve")
            .stdout(stdout_log)
            .stderr(stderr_log)
            .spawn()?;

        // Write PID file
        let pid_path = pid_file();
        std::fs::write(&pid_path, child.id().to_string())?;

        println!("Started orch serve (pid: {})", child.id());
    }
    Ok(())
}

/// Stop the orchestrator service.
pub fn stop() -> anyhow::Result<()> {
    if is_brew_installed() {
        println!("Stopping via brew services...");
        let status = std::process::Command::new("brew")
            .args(["services", "stop", "orch"])
            .status()?;
        if !status.success() {
            anyhow::bail!("brew services stop failed");
        }
    } else {
        let pid_path = pid_file();
        if pid_path.exists() {
            let pid_str = std::fs::read_to_string(&pid_path)?;
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                libc_kill(pid);
                std::fs::remove_file(&pid_path)?;
                println!("Stopped orch (pid: {})", pid);
            } else {
                println!("Invalid PID file");
            }
        } else {
            println!("Orch is not running (no PID file)");
        }
    }
    Ok(())
}

/// Send SIGTERM to a process.
fn libc_kill(pid: i32) {
    // Use std::process::Command to send kill signal
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
}

/// Restart the orchestrator service.
pub fn restart() -> anyhow::Result<()> {
    if is_brew_installed() {
        println!("Restarting via brew services...");
        let status = std::process::Command::new("brew")
            .args(["services", "restart", "orch"])
            .status()?;
        if !status.success() {
            anyhow::bail!("brew services restart failed");
        }
    } else {
        stop()?;
        std::thread::sleep(std::time::Duration::from_secs(1));
        start()?;
    }
    Ok(())
}

/// Show service status.
pub fn status() -> anyhow::Result<()> {
    if is_brew_installed() {
        let status = std::process::Command::new("brew")
            .args(["services", "info", "orch"])
            .status()?;
        if !status.success() {
            // Fallback: check PID
            check_pid_status()?;
        }
    } else {
        check_pid_status()?;
    }
    Ok(())
}

fn check_pid_status() -> anyhow::Result<()> {
    let pid_path = pid_file();
    if pid_path.exists() {
        let pid_str = std::fs::read_to_string(&pid_path)?;
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            let output = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .output()?;
            if output.status.success() {
                println!("Orch is running (pid: {})", pid);
            } else {
                println!("Orch is not running (stale pid: {})", pid);
            }
        } else {
            println!("Invalid PID file");
        }
    } else {
        println!("Orch is not running (no PID file)");
    }
    Ok(())
}

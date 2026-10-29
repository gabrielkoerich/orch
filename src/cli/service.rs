use crate::cmd::SyncCommandErrorContext;
#[cfg(any(test, not(target_os = "macos")))]
use anyhow::Context;
#[cfg(any(test, not(target_os = "macos")))]
use std::path::{Path, PathBuf};

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

/// Write the systemd user unit and enable it
pub fn install(force: bool) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let _ = force;
        macos_service_owned_by_homebrew();
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        linux_install(
            &systemd_user_dir()?,
            &current_binary()?,
            Path::new("systemctl"),
            force,
        )
    }
}

/// Disable the systemd user unit and remove the file
pub fn uninstall() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos_service_owned_by_homebrew();
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        linux_uninstall(&systemd_user_dir()?, Path::new("systemctl"))
    }
}

#[cfg(any(test, target_os = "macos"))]
fn macos_service_owned_by_homebrew() {
    println!("The Homebrew formula already installs the orch service.");
}

#[cfg(test)]
fn install_os(
    macos: bool,
    unit_dir: &Path,
    binary: &Path,
    systemctl: &Path,
    force: bool,
) -> anyhow::Result<()> {
    if macos {
        macos_service_owned_by_homebrew();
        return Ok(());
    }
    linux_install(unit_dir, binary, systemctl, force)
}

#[cfg(test)]
fn uninstall_os(macos: bool, unit_dir: &Path, systemctl: &Path) -> anyhow::Result<()> {
    if macos {
        macos_service_owned_by_homebrew();
        return Ok(());
    }
    linux_uninstall(unit_dir, systemctl)
}

#[cfg(any(test, not(target_os = "macos")))]
fn linux_install(
    unit_dir: &Path,
    binary: &Path,
    systemctl: &Path,
    force: bool,
) -> anyhow::Result<()> {
    let unit_path = unit_dir.join("orch.service");
    let unit = unit_contents(binary);
    if unit_path.exists() && !force {
        let existing = std::fs::read_to_string(&unit_path)
            .with_context(|| format!("reading {}", unit_path.display()))?;
        if existing != unit {
            print_unit_diff(&existing, &unit);
        }
        anyhow::bail!(
            "unit already exists at {}; pass --force to overwrite",
            unit_path.display()
        );
    }
    std::fs::create_dir_all(unit_dir)
        .with_context(|| format!("creating {}", unit_dir.display()))?;
    std::fs::write(&unit_path, &unit)
        .with_context(|| format!("writing {}", unit_path.display()))?;
    println!("Wrote {}", unit_path.display());
    run_systemctl(systemctl, &["daemon-reload"])?;
    run_systemctl(systemctl, &["enable", "--now", "orch"])?;
    println!("Enabled and started the orch user service.");
    println!("A user service stops at logout unless lingering is on.");
    println!("  loginctl enable-linger $USER");
    Ok(())
}

#[cfg(any(test, not(target_os = "macos")))]
fn linux_uninstall(unit_dir: &Path, systemctl: &Path) -> anyhow::Result<()> {
    let unit_path = unit_dir.join("orch.service");
    if !unit_path.exists() {
        let _ = run_systemctl(systemctl, &["disable", "--now", "orch"]);
        println!("No orch user unit at {}", unit_path.display());
        return Ok(());
    }
    run_systemctl(systemctl, &["disable", "--now", "orch"])?;
    std::fs::remove_file(&unit_path)
        .with_context(|| format!("removing {}", unit_path.display()))?;
    println!("Removed {}", unit_path.display());
    run_systemctl(systemctl, &["daemon-reload"])?;
    Ok(())
}

#[cfg(any(test, not(target_os = "macos")))]
fn unit_contents(binary: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=orch agent orchestrator\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={} serve\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec_start_bin(binary)
    )
}

#[cfg(any(test, not(target_os = "macos")))]
fn exec_start_bin(binary: &Path) -> String {
    let path = binary.display().to_string();
    if path.chars().any(char::is_whitespace) {
        format!("\"{path}\"")
    } else {
        path
    }
}

#[cfg(any(test, not(target_os = "macos")))]
fn print_unit_diff(existing: &str, proposed: &str) {
    println!("--- existing");
    println!("+++ proposed");
    let old: Vec<&str> = existing.lines().collect();
    let new: Vec<&str> = proposed.lines().collect();
    let len = old.len().max(new.len());
    for i in 0..len {
        match (old.get(i), new.get(i)) {
            (Some(a), Some(b)) if a == b => println!(" {a}"),
            (Some(a), Some(b)) => {
                println!("-{a}");
                println!("+{b}");
            }
            (Some(a), None) => println!("-{a}"),
            (None, Some(b)) => println!("+{b}"),
            (None, None) => {}
        }
    }
}

#[cfg(any(test, not(target_os = "macos")))]
fn run_systemctl(systemctl: &Path, args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new(systemctl)
        .arg("--user")
        .args(args)
        .status_with_context()?;
    if !status.success() {
        anyhow::bail!("{} --user {} failed", systemctl.display(), args.join(" "));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn systemd_user_dir() -> anyhow::Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("systemd").join("user"));
        }
    }
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".config").join("systemd").join("user"))
}

#[cfg(not(target_os = "macos"))]
fn current_binary() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot resolve the orch binary path")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn stub_systemctl(dir: &Path) -> PathBuf {
        let stub = dir.join("systemctl");
        let log = dir.join("systemctl.log");
        fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        stub
    }

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let unit_dir = root.path().join("user");
        fs::create_dir_all(&unit_dir).unwrap();
        let stub = stub_systemctl(root.path());
        let binary = PathBuf::from("/opt/orch/bin/orch");
        (root, unit_dir, binary, stub)
    }

    fn unit_path(unit_dir: &Path) -> PathBuf {
        unit_dir.join("orch.service")
    }

    #[test]
    fn install_writes_unit_with_binary_path_and_serve() {
        let (root, unit_dir, binary, stub) = setup();
        install_os(false, &unit_dir, &binary, &stub, false).unwrap();
        let unit = fs::read_to_string(unit_path(&unit_dir)).unwrap();
        assert!(
            unit.contains("ExecStart=/opt/orch/bin/orch serve"),
            "{unit}"
        );
        assert!(unit.contains("Restart=always"), "{unit}");
        assert!(unit.contains("WantedBy=default.target"), "{unit}");
        assert!(!unit.contains("Environment"), "{unit}");
        let log = fs::read_to_string(root.path().join("systemctl.log")).unwrap();
        assert!(log.contains("daemon-reload"), "{log}");
        assert!(log.contains("enable --now orch"), "{log}");
    }

    #[test]
    fn second_install_without_force_refuses_and_leaves_file() {
        let (_root, unit_dir, binary, stub) = setup();
        install_os(false, &unit_dir, &binary, &stub, false).unwrap();
        let path = unit_path(&unit_dir);
        let original = fs::read_to_string(&path).unwrap();
        fs::write(&path, original.replace("RestartSec=5", "RestartSec=99")).unwrap();
        let changed = fs::read_to_string(&path).unwrap();
        let err = install_os(false, &unit_dir, &binary, &stub, false).unwrap_err();
        assert!(
            err.to_string().contains("--force"),
            "error should mention --force: {err}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), changed);
    }

    #[test]
    fn uninstall_removes_the_unit() {
        let (_root, unit_dir, binary, stub) = setup();
        install_os(false, &unit_dir, &binary, &stub, false).unwrap();
        assert!(unit_path(&unit_dir).exists());
        uninstall_os(false, &unit_dir, &stub).unwrap();
        assert!(!unit_path(&unit_dir).exists());
    }

    #[test]
    fn macos_install_changes_nothing() {
        let (_root, unit_dir, binary, stub) = setup();
        #[cfg(target_os = "macos")]
        {
            install(false).unwrap();
            uninstall().unwrap();
        }
        install_os(true, &unit_dir, &binary, &stub, false).unwrap();
        install_os(true, &unit_dir, &binary, &stub, true).unwrap();
        assert!(!unit_path(&unit_dir).exists());
        assert!(unit_dir.read_dir().unwrap().next().is_none());
    }
}

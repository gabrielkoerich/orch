fn main() {
    // Version priority:
    // 1. ORCH_BUILD_VERSION env var (set by CI to the computed release version, e.g. "v0.5.7")
    // 2. git describe --tags --abbrev=0 (last tag, for local dev builds when tags exist)
    // 3. CARGO_PKG_VERSION from Cargo.toml (hardcoded fallback)
    let version = std::env::var("ORCH_BUILD_VERSION")
        .map(|v| v.trim_start_matches('v').to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(git_tag_version)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    println!("cargo:rustc-env=ORCH_VERSION={version}");
    println!("cargo:rerun-if-env-changed=ORCH_BUILD_VERSION");
}

fn git_tag_version() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let tag = String::from_utf8(output.stdout).ok()?;
    let version = tag.trim().trim_start_matches('v').to_string();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

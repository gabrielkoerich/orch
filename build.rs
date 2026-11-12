use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    // Version priority:
    // 1. ORCH_BUILD_VERSION env var (set by CI to the computed release version, e.g. "v0.5.7")
    // 2. git describe --tags --abbrev=0 (last tag, for local dev builds when tags exist)
    // 3. CARGO_PKG_VERSION from Cargo.toml (hardcoded fallback)
    let version = env::var("ORCH_BUILD_VERSION")
        .map(|v| v.trim_start_matches('v').to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(git_tag_version)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    println!("cargo:rustc-env=ORCH_VERSION={version}");
    println!("cargo:rerun-if-env-changed=ORCH_BUILD_VERSION");

    web_bundle();
}

// The web UI's script is built by Bun into web/dist, which git ignores. A build without it gets a
// page that says how to build it, and a release, which sets ORCH_REQUIRE_WEB, fails instead
fn web_bundle() {
    println!("cargo:rerun-if-changed=web/dist/app.js");
    println!("cargo:rerun-if-env-changed=ORCH_REQUIRE_WEB");
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let built = Path::new(&manifest).join("web/dist/app.js");
    if built.exists() {
        println!("cargo:rustc-env=ORCH_WEB_JS={}", built.display());
        return;
    }

    assert!(
        env::var_os("ORCH_REQUIRE_WEB").is_none(),
        "web/dist/app.js is missing, run `just build` before a release build"
    );
    let out = env::var("OUT_DIR").unwrap_or_default();
    let placeholder = Path::new(&out).join("app.js");
    let page = "document.body.textContent = \"This orch build has no web UI. Run `just build` in the orch repo and build again.\";\n";
    if let Err(e) = fs::write(&placeholder, page) {
        panic!("could not write {}: {e}", placeholder.display());
    }
    println!("cargo:rustc-env=ORCH_WEB_JS={}", placeholder.display());
}

fn git_tag_version() -> Option<String> {
    let output = Command::new("git")
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

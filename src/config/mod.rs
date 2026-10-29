//! Config reader — loads YAML config files and resolves dot-separated keys.
//!
//! Reads `~/.orch/config.yml` (global) and `.orch.yml` (project).
//! Project config overrides global config for the same key.
//!
//! Features:
//! - In-memory cache: parsed YAML is cached for the process lifetime
//! - File watching: uses notify crate to watch for config file changes
//! - Hot reload: a changed file is re-parsed into the cache; one that does not
//!   parse leaves the last good values in place
//! - Change notifications: subscribers receive events when config files change

pub mod projects_yaml;

use anyhow::Context;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::broadcast;

/// Cached YAML values — parsed once per file, reused for all key lookups.
/// Protected by RwLock for concurrent read access.
static CACHE: std::sync::LazyLock<RwLock<HashMap<PathBuf, serde_norway::Value>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

/// Files currently being watched for changes.
static WATCHER: std::sync::LazyLock<RwLock<HashMap<PathBuf, ()>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

/// Global file watcher instance (started on first use).
static FILE_WATCHER: std::sync::LazyLock<Arc<Mutex<Option<RecommendedWatcher>>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(None)));

/// Config files whose content on disk does not read or parse right now.
/// Tracked so the parse error is logged once per breakage, not once per
/// watcher event.
static BAD_FILES: std::sync::LazyLock<RwLock<HashSet<PathBuf>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashSet::new()));

/// Broadcast sender for config change notifications.
/// Sends the path of the changed config file.
static CHANGE_TX: std::sync::LazyLock<broadcast::Sender<PathBuf>> =
    std::sync::LazyLock::new(|| {
        let (tx, _) = broadcast::channel(16);
        tx
    });

/// Subscribe to config change notifications.
///
/// Returns a receiver that fires whenever a watched config file changes.
/// The receiver yields the path of the changed file.
pub fn subscribe() -> broadcast::Receiver<PathBuf> {
    CHANGE_TX.subscribe()
}

/// Pre-populate the config cache using async I/O.
///
/// Call once at engine startup so subsequent synchronous [`get`] / [`get_list`]
/// calls always hit the in-memory cache and never block a Tokio thread with
/// `std::fs::read_to_string`.
pub async fn warm_cache() -> anyhow::Result<()> {
    let global_path = global_config_path()?;
    if global_path.exists() {
        match tokio::fs::read_to_string(&global_path).await {
            Ok(content) => {
                if let Ok(parsed) = serde_norway::from_str::<serde_norway::Value>(&content) {
                    if let Ok(mut cache) = CACHE.write() {
                        cache.insert(global_path.clone(), parsed);
                    }
                    watch_file(&global_path);
                }
            }
            Err(e) => tracing::warn!("failed to warm global config cache: {e}"),
        }
    }

    let project_path = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".orch.yml");
    if project_path.exists() {
        match tokio::fs::read_to_string(&project_path).await {
            Ok(content) => {
                if let Ok(parsed) = serde_norway::from_str::<serde_norway::Value>(&content) {
                    if let Ok(mut cache) = CACHE.write() {
                        cache.insert(project_path.clone(), parsed);
                    }
                    watch_file(&project_path);
                }
            }
            Err(e) => tracing::warn!("failed to warm project config cache: {e}"),
        }
    }

    Ok(())
}

/// Clear the entire config cache. Exposed for use by integration tests that
/// need to run in an isolated config environment (e.g. a temp dir with no
/// project `.orch.yml`).
#[cfg(test)]
pub fn clear_test_cache() {
    if let Ok(mut cache) = CACHE.write() {
        cache.clear();
    }
}

/// Re-read a changed config file and notify subscribers.
///
/// A file that cannot be read or parsed leaves the last good value in the
/// cache and sends no notification, so a broken or half-written edit cannot
/// reset the running config to defaults.
///
/// Returns true when this call logged a newly invalid file, so repeated
/// watcher events for one breakage produce one error line
fn reload_file(path: &PathBuf) -> bool {
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|content| {
            serde_norway::from_str::<serde_norway::Value>(&content).map_err(|e| e.to_string())
        });

    match parsed {
        Ok(value) => {
            if let Ok(mut cache) = CACHE.write() {
                cache.insert(path.clone(), value);
            }
            if let Ok(mut bad) = BAD_FILES.write() {
                if bad.remove(path) {
                    tracing::info!(path = %path.display(), "config file parses again, reloaded");
                }
            }
            tracing::debug!("config cache refreshed for: {}", path.display());
            // Notify subscribers (ignore error if no active receivers)
            let _ = CHANGE_TX.send(path.clone());
            false
        }
        Err(err) => {
            let first_failure = BAD_FILES
                .write()
                .map(|mut bad| bad.insert(path.clone()))
                .unwrap_or(false);
            if first_failure {
                tracing::error!(
                    path = %path.display(),
                    %err,
                    "config file is invalid, keeping the last good values"
                );
            }
            first_failure
        }
    }
}

/// Start the file watcher if not already running.
fn ensure_watcher() {
    let mut watcher_guard = FILE_WATCHER.lock().unwrap_or_else(|e| e.into_inner());
    if watcher_guard.is_some() {
        return;
    }

    // Create the watcher
    let watcher = RecommendedWatcher::new(
        move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                if !is_content_change(&event.kind) {
                    return;
                }
                for path in event.paths {
                    // Check if it's a config file we care about
                    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

                    if filename == "config.yml" || filename == ".orch.yml" {
                        let _ = reload_file(&path);
                    }
                }
            }
        },
        Config::default().with_poll_interval(Duration::from_secs(2)),
    );

    if let Ok(w) = watcher {
        *watcher_guard = Some(w);
    }
}

// inotify reports every read as an access event, and reload_file reads the file, so reacting to
// access events reloads forever on Linux
fn is_content_change(kind: &notify::EventKind) -> bool {
    kind.is_create() || kind.is_modify() || kind.is_remove()
}

/**
 * Register a config file for change watching. `get` / `get_list` watch the files they read, but a
 * project's `.orch.yml` is read directly by `get_projects_with_paths`, so the caller that knows the
 * project list registers it here. A missing path is skipped: notify cannot watch one, and a project
 * may be configured through the legacy `.orchestrator.yml` name or the global `gh.repo` fallback,
 * which has no project directory at all
 **/
pub fn watch(path: &Path) {
    if !path.exists() {
        tracing::debug!(path = %path.display(), "no config file to watch here");
        return;
    }
    watch_file(&path.to_path_buf());
}

/// Watch a config file for changes.
fn watch_file(path: &PathBuf) {
    // First ensure watcher is running
    ensure_watcher();

    // Track that we want to watch this file
    if let Ok(mut watched) = WATCHER.write() {
        if !watched.contains_key(path) {
            watched.insert(path.clone(), ());

            // Add to notify watcher
            if let Ok(mut guard) = FILE_WATCHER.lock() {
                if let Some(ref mut watcher) = *guard {
                    match watcher.watch(path, RecursiveMode::NonRecursive) {
                        Ok(()) => tracing::debug!("now watching config file: {}", path.display()),
                        Err(e) => tracing::warn!(
                            "failed to register config file watcher for {}: {}",
                            path.display(),
                            e
                        ),
                    }
                }
            }
        }
    }
}

fn read_file_to_string(path: &Path) -> anyhow::Result<String> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            Ok(tokio::task::block_in_place(|| {
                std::fs::read_to_string(path)
            })?)
        }
        _ => Ok(std::fs::read_to_string(path)?),
    }
}

fn load_cached_root(path: &PathBuf) -> anyhow::Result<serde_norway::Value> {
    if let Ok(cache) = CACHE.read() {
        if let Some(cached) = cache.get(path) {
            return Ok(cached.clone());
        }
    }

    let content =
        read_file_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: serde_norway::Value =
        serde_norway::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;

    if let Ok(mut cache) = CACHE.write() {
        cache.insert(path.clone(), parsed.clone());
    }

    Ok(parsed)
}

/// Resolve the global config path: `~/.orch/config.yml`
fn global_config_path() -> anyhow::Result<PathBuf> {
    crate::home::config_path()
}

/// Get a config value as a list of strings by dot-separated key.
///
/// Supports both YAML sequences and comma-separated strings:
/// ```yaml
/// workflow:
///   allowed_tools:
///     - Edit
///     - Write
///     - git
/// ```
/// or:
/// ```yaml
/// workflow:
///   allowed_tools: "Edit,Write,git"
/// ```
///
/// Returns empty vec if key not found.
pub fn get_list(key: &str) -> anyhow::Result<Vec<String>> {
    // Try project config first
    // Use an absolute path so the cache key is stable across tests/processes
    // that change the current working directory.
    let project_path = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".orch.yml");
    if project_path.exists() {
        if let Ok(val) = resolve_list(&project_path, key) {
            if !val.is_empty() {
                return Ok(val);
            }
        }
    }

    // Fall back to global config
    let global_path = global_config_path()?;
    if global_path.exists() {
        return resolve_list(&global_path, key);
    }

    Ok(vec![])
}

/// Resolve a dot-separated key as a list from a YAML file.
fn resolve_list(path: &PathBuf, key: &str) -> anyhow::Result<Vec<String>> {
    watch_file(path);

    let root = load_cached_root(path)?;

    extract_list(&root, key)
}

/// Extract a list of strings from a YAML tree by dot-separated key.
fn extract_list(root: &serde_norway::Value, key: &str) -> anyhow::Result<Vec<String>> {
    let mut current = root;
    for part in key.split('.') {
        match current.get(part) {
            Some(v) => current = v,
            None => return Ok(vec![]),
        }
    }

    match current {
        serde_norway::Value::Sequence(seq) => Ok(seq
            .iter()
            .filter_map(|item| item.as_str().map(String::from))
            .collect()),
        serde_norway::Value::String(s) => {
            // Support comma-separated string format
            Ok(s.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect())
        }
        _ => Ok(vec![]),
    }
}

/// Get a config value by dot-separated key (e.g. "agents.claude.model").
///
/// Lookup order:
/// 1. `.orch.yml` in the current directory (project config)
/// 2. `~/.orch/config.yml` (global config)
///
/// Returns the first match as a string.
/// Files are parsed once and cached for the process lifetime.
pub fn get(key: &str) -> anyhow::Result<String> {
    // Try project config first
    // Use an absolute path so the cache key is stable across tests/processes
    // that change the current working directory.
    let project_path = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".orch.yml");
    if project_path.exists() {
        if let Ok(val) = resolve_key(&project_path, key) {
            return Ok(val);
        }
    }

    // Fall back to global config
    let global_path = global_config_path()?;
    if global_path.exists() {
        return resolve_key(&global_path, key);
    }

    anyhow::bail!("config key not found: {key}")
}

/// Like [`get`], but reads the `.orch.yml` in `project_dir`, since the service runs from `/`
pub fn get_for_project(project_dir: &std::path::Path, key: &str) -> anyhow::Result<String> {
    let project_path = project_dir.join(".orch.yml");
    if project_path.exists() {
        if let Ok(val) = resolve_key(&project_path, key) {
            return Ok(val);
        }
    }

    let global_path = global_config_path()?;
    if global_path.exists() {
        return resolve_key(&global_path, key);
    }

    anyhow::bail!("config key not found: {key}")
}

/// Get all project paths from the global config `projects:` list.
///
/// Returns a list of absolute paths to project directories.
/// Each directory should contain a `.orch.yml` with `gh.repo`.
pub fn get_project_paths() -> anyhow::Result<Vec<String>> {
    let global_path = global_config_path()?;

    if global_path.exists() {
        if let Ok(paths) = resolve_projects_list(&global_path, "projects") {
            if !paths.is_empty() {
                return Ok(paths);
            }
        }
    }

    Ok(vec![])
}

/// Like `get_projects()` but also returns the project directory path.
///
/// Returns `(repo_slug, project_dir)` pairs. Used by the engine to resolve
/// per-project config (e.g., jobs from each project's `.orch.yml`).
pub fn get_projects_with_paths() -> anyhow::Result<Vec<(String, std::path::PathBuf)>> {
    let paths = get_project_paths()?;

    if !paths.is_empty() {
        let mut results = Vec::new();
        for path_str in &paths {
            let path = std::path::PathBuf::from(path_str);
            let orch_yml = path.join(".orch.yml");

            let config_file = if orch_yml.exists() {
                orch_yml
            } else {
                let legacy = path.join(".orchestrator.yml");
                if legacy.exists() {
                    legacy
                } else {
                    continue;
                }
            };

            if let Ok(content) = std::fs::read_to_string(&config_file) {
                if let Ok(doc) = serde_norway::from_str::<serde_norway::Value>(&content) {
                    if let Some(repo) = doc
                        .get("gh")
                        .and_then(|gh| gh.get("repo"))
                        .and_then(|r| r.as_str())
                    {
                        results.push((repo.to_string(), path));
                    }
                }
            }
        }

        if !results.is_empty() {
            return Ok(results);
        }
    }

    // Fall back to global gh.repo (no project dir available)
    if let Ok(repo) = get("gh.repo") {
        if !repo.is_empty() {
            return Ok(vec![(repo, std::path::PathBuf::from("."))]);
        }
    }

    Ok(vec![])
}

/// Resolve the repo slug for a specific project path.
///
/// Reads `gh.repo` from the project's `.orch.yml`.
pub fn get_repo_for_project(project_path: &std::path::Path) -> anyhow::Result<String> {
    let orch_yml = project_path.join(".orch.yml");
    let config_file = if orch_yml.exists() {
        orch_yml
    } else {
        let legacy = project_path.join(".orchestrator.yml");
        if legacy.exists() {
            legacy
        } else {
            anyhow::bail!("no .orch.yml found in {}", project_path.display());
        }
    };

    let content = std::fs::read_to_string(&config_file)?;
    let doc: serde_norway::Value = serde_norway::from_str(&content)?;

    doc.get("gh")
        .and_then(|gh| gh.get("repo"))
        .and_then(|r| r.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("gh.repo not set in {}", config_file.display()))
}

/// Resolve project path from CWD by finding the nearest `.orch.yml`.
///
/// Walks up from the current directory looking for `.orch.yml` or legacy `.orchestrator.yml`.
pub fn find_project_root() -> anyhow::Result<std::path::PathBuf> {
    let mut dir = std::env::current_dir()?;

    loop {
        if dir.join(".orch.yml").exists() || dir.join(".orchestrator.yml").exists() {
            return Ok(dir);
        }

        if !dir.pop() {
            break;
        }
    }

    anyhow::bail!("no .orch.yml found in current directory or parents — run `orch init`")
}

/// Resolve the repo slug for the current project (from CWD).
///
/// Walks up from CWD to find `.orch.yml`, reads `gh.repo` from it.
/// Falls back to global `gh.repo` for backward compatibility.
pub fn get_current_repo() -> anyhow::Result<String> {
    // Try to find project root from CWD
    if let Ok(project_root) = find_project_root() {
        if let Ok(repo) = get_repo_for_project(&project_root) {
            return Ok(repo);
        }
    }

    // Fall back to per-project config in CWD (.orch.yml)
    if let Ok(repo) = get("gh.repo") {
        if !repo.is_empty() {
            return Ok(repo);
        }
    }

    // Legacy fallback
    if let Ok(repo) = get("repo") {
        if !repo.is_empty() {
            return Ok(repo);
        }
    }

    anyhow::bail!("could not determine repo — ensure .orch.yml has gh.repo set")
}

/// Skill repository configuration.
#[derive(Debug, Clone)]
pub struct SkillConfig {
    /// GitHub repository in "owner/repo" format.
    pub repo: String,
}

/// Get skills configuration from the global config.
///
/// Reads the `skills:` list from `~/.orch/config.yml`:
/// ```yaml
/// skills:
///   - repo: "owner/skill-repo"
///   - repo: "owner/another-repo"
/// ```
///
/// Returns an empty vector if no skills are configured.
pub fn get_skills() -> anyhow::Result<Vec<SkillConfig>> {
    let global_path = global_config_path()?;

    if !global_path.exists() {
        return Ok(vec![]);
    }

    let root = load_cached_root(&global_path)?;

    extract_skills_list(&root)
}

/// Extract skills list from a YAML tree.
fn extract_skills_list(root: &serde_norway::Value) -> anyhow::Result<Vec<SkillConfig>> {
    let Some(serde_norway::Value::Sequence(seq)) = root.get("skills") else {
        return Ok(vec![]);
    };

    let mut skills = Vec::new();
    for item in seq {
        if let serde_norway::Value::Mapping(map) = item {
            let repo = map
                .get("repo")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_default();

            if !repo.is_empty() {
                skills.push(SkillConfig { repo });
            }
        }
    }

    Ok(skills)
}

/// Resolve a projects list from a config file.
fn resolve_projects_list(path: &PathBuf, key: &str) -> anyhow::Result<Vec<String>> {
    // First, ensure we're watching this file for changes
    watch_file(path);

    let root = load_cached_root(path)?;

    // Try to extract projects list - if key doesn't exist, return empty vector
    match extract_projects_list(&root, key) {
        Ok(projects) => Ok(projects),
        Err(_) => Ok(vec![]),
    }
}

/// Extract a projects list from a YAML tree.
fn extract_projects_list(root: &serde_norway::Value, key: &str) -> anyhow::Result<Vec<String>> {
    let mut current = root;
    for part in key.split('.') {
        // If key doesn't exist, return empty vector
        let next = match current.get(part) {
            Some(v) => v,
            None => return Ok(vec![]),
        };
        current = next;
    }

    match current {
        serde_norway::Value::Sequence(seq) => {
            let mut projects = Vec::new();
            for item in seq {
                if let serde_norway::Value::String(s) = item {
                    projects.push(s.clone());
                } else if let serde_norway::Value::Mapping(map) = item {
                    // Support map format: { repo: "owner/repo", project_dir: "/path" }
                    if let Some(serde_norway::Value::String(repo)) = map.get("repo") {
                        projects.push(repo.clone());
                    }
                }
            }
            Ok(projects)
        }
        _ => Ok(vec![]),
    }
}

/// Resolve a dot-separated key from a YAML file.
///
/// Caches the parsed YAML so repeated lookups don't re-read disk.
/// Sets up file watching for hot reload when first loading.
fn resolve_key(path: &PathBuf, key: &str) -> anyhow::Result<String> {
    // First, ensure we're watching this file for changes
    watch_file(path);

    let root = load_cached_root(path)?;

    extract_value(&root, key)
}

/// Extract a value from a YAML tree by dot-separated key.
fn extract_value(root: &serde_norway::Value, key: &str) -> anyhow::Result<String> {
    let mut current = root;
    for part in key.split('.') {
        current = current
            .get(part)
            .with_context(|| format!("key not found: {key}"))?;
    }

    match current {
        serde_norway::Value::String(s) => Ok(s.clone()),
        serde_norway::Value::Number(n) => Ok(n.to_string()),
        serde_norway::Value::Bool(b) => Ok(b.to_string()),
        serde_norway::Value::Null => Ok(String::new()),
        _ => Ok(serde_norway::to_string(current)?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_yaml(dir: &std::path::Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn resolve_simple_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "config.yml", "repo: owner/repo\n");
        let val = resolve_key(&path, "repo").unwrap();
        assert_eq!(val, "owner/repo");
    }

    #[test]
    fn resolve_nested_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(
            dir.path(),
            "config.yml",
            "agents:\n  claude:\n    model: opus\n",
        );
        let val = resolve_key(&path, "agents.claude.model").unwrap();
        assert_eq!(val, "opus");
    }

    #[test]
    fn resolve_boolean_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "config.yml", "enabled: true\n");
        let val = resolve_key(&path, "enabled").unwrap();
        assert_eq!(val, "true");
    }

    #[test]
    fn resolve_number_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "config.yml", "timeout: 300\n");
        let val = resolve_key(&path, "timeout").unwrap();
        assert_eq!(val, "300");
    }

    #[test]
    fn resolve_missing_key_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "config.yml", "repo: owner/repo\n");
        let result = resolve_key(&path, "missing.key");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_missing_file_errors() {
        let path = PathBuf::from("/nonexistent/config.yml");
        let result = resolve_key(&path, "repo");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_projects_list_string_array() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(
            dir.path(),
            "config.yml",
            "projects:\n  - owner/repo1\n  - owner/repo2\n",
        );
        let projects = resolve_projects_list(&path, "projects").unwrap();
        assert_eq!(projects, vec!["owner/repo1", "owner/repo2"]);
    }

    #[test]
    fn resolve_projects_list_map_array() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(
            dir.path(),
            "config.yml",
            "projects:\n  - repo: owner/repo1\n    project_dir: /path/to/repo1\n  - repo: owner/repo2\n    project_dir: /path/to/repo2\n",
        );
        let projects = resolve_projects_list(&path, "projects").unwrap();
        assert_eq!(projects, vec!["owner/repo1", "owner/repo2"]);
    }

    #[test]
    fn resolve_projects_list_fallback_to_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "config.yml", "repo: owner/single-repo\n");
        let projects = resolve_projects_list(&path, "projects").unwrap();
        assert!(projects.is_empty());
    }

    #[test]
    fn extract_projects_list_from_sequence() {
        let yaml =
            serde_norway::from_str::<serde_norway::Value>("projects:\n  - a\n  - b\n  - c\n")
                .unwrap();
        let projects = extract_projects_list(&yaml, "projects").unwrap();
        assert_eq!(projects, vec!["a", "b", "c"]);
    }

    #[test]
    fn extract_projects_list_from_mapping_sequence() {
        let yaml =
            serde_norway::from_str::<serde_norway::Value>("projects:\n  - repo: a\n  - repo: b\n")
                .unwrap();
        let projects = extract_projects_list(&yaml, "projects").unwrap();
        assert_eq!(projects, vec!["a", "b"]);
    }

    #[test]
    fn extract_projects_list_empty_for_missing() {
        let yaml = serde_norway::from_str::<serde_norway::Value>("foo: bar").unwrap();
        let projects = extract_projects_list(&yaml, "projects").unwrap();
        assert!(projects.is_empty());
    }

    #[test]
    fn subscribe_receives_change_notifications() {
        let mut rx = subscribe();
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "config.yml", "repo: owner/repo\n");

        // Simulate the watcher firing on a file change
        assert!(!reload_file(&path));

        // Drain until we find our path (other tests may send on the shared channel)
        loop {
            match rx.try_recv() {
                Ok(received_path) if received_path == path => break,
                Ok(_) => continue, // skip notifications from other tests
                Err(e) => panic!("expected config change notification, got error: {e:?}"),
            }
        }
    }

    #[test]
    fn subscribe_multiple_receivers() {
        let mut rx1 = subscribe();
        let mut rx2 = subscribe();
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "config.yml", "repo: owner/repo\n");

        assert!(!reload_file(&path));

        // Drain until we find our path on both receivers
        let find_path = |rx: &mut tokio::sync::broadcast::Receiver<PathBuf>, expected: &PathBuf| loop {
            match rx.try_recv() {
                Ok(p) if &p == expected => return,
                Ok(_) => continue,
                Err(e) => panic!("expected config change notification, got error: {e:?}"),
            }
        };
        find_path(&mut rx1, &path);
        find_path(&mut rx2, &path);
    }

    #[test]
    fn resolve_list_from_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(
            dir.path(),
            "config.yml",
            "workflow:\n  allowed_tools:\n    - Edit\n    - Write\n    - git\n    - npm\n",
        );
        let list = resolve_list(&path, "workflow.allowed_tools").unwrap();
        assert_eq!(list, vec!["Edit", "Write", "git", "npm"]);
    }

    #[test]
    fn resolve_list_from_comma_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(
            dir.path(),
            "config.yml",
            "workflow:\n  allowed_tools: \"Edit,Write,git,npm\"\n",
        );
        let list = resolve_list(&path, "workflow.allowed_tools").unwrap();
        assert_eq!(list, vec!["Edit", "Write", "git", "npm"]);
    }

    #[test]
    fn resolve_list_missing_key_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "config.yml", "foo: bar\n");
        let list = resolve_list(&path, "workflow.allowed_tools").unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn extract_skills_list_from_sequence() {
        let yaml = serde_norway::from_str::<serde_norway::Value>(
            "skills:\n  - repo: owner/skills1\n  - repo: owner/skills2\n",
        )
        .unwrap();
        let skills = extract_skills_list(&yaml).unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].repo, "owner/skills1");
        assert_eq!(skills[1].repo, "owner/skills2");
    }

    #[test]
    fn extract_skills_list_single_repo() {
        let yaml =
            serde_norway::from_str::<serde_norway::Value>("skills:\n  - repo: owner/skills\n")
                .unwrap();
        let skills = extract_skills_list(&yaml).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].repo, "owner/skills");
    }

    /**
     * Read a key straight out of the cache. The reload tests must not call `resolve_key` or
     * `resolve_projects_list`: those register the file with the real notify watcher, whose thread
     * then races the test for the first `reload_file` call on a broken edit
     **/
    fn cached_value(path: &PathBuf, key: &str) -> String {
        extract_value(&load_cached_root(path).unwrap(), key).unwrap()
    }

    fn cached_projects(path: &PathBuf) -> Vec<String> {
        extract_projects_list(&load_cached_root(path).unwrap(), "projects").unwrap()
    }

    #[test]
    fn reading_a_config_file_does_not_trigger_a_reload() {
        use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, ModifyKind};
        assert!(!is_content_change(&notify::EventKind::Access(
            AccessKind::Open(AccessMode::Read)
        )));
        assert!(!is_content_change(&notify::EventKind::Access(
            AccessKind::Close(AccessMode::Read)
        )));
        assert!(is_content_change(&notify::EventKind::Modify(
            ModifyKind::Data(DataChange::Content)
        )));
        assert!(is_content_change(&notify::EventKind::Create(
            CreateKind::File
        )));
    }

    #[test]
    fn invalid_edit_keeps_last_good_values_and_sends_no_notification() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "config.yml", "engine:\n  max_parallel: 8\n");
        assert!(!reload_file(&path));
        assert_eq!(cached_value(&path, "engine.max_parallel"), "8");

        let mut rx = subscribe();
        std::fs::write(&path, "engine:\n  max_parallel: [unclosed\n").unwrap();
        assert!(reload_file(&path));

        assert_eq!(cached_value(&path, "engine.max_parallel"), "8");
        assert!(rx.try_recv().is_err(), "a broken edit must not notify");

        // A later good edit is picked up again
        std::fs::write(&path, "engine:\n  max_parallel: 12\n").unwrap();
        assert!(!reload_file(&path));
        assert_eq!(cached_value(&path, "engine.max_parallel"), "12");
    }

    #[test]
    fn adding_a_project_is_visible_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "config.yml", "projects:\n  - /srv/one\n");
        assert_eq!(cached_projects(&path), vec!["/srv/one"]);

        std::fs::write(&path, "projects:\n  - /srv/one\n  - /srv/two\n").unwrap();
        assert!(!reload_file(&path));

        assert_eq!(cached_projects(&path), vec!["/srv/one", "/srv/two"]);
    }

    #[test]
    fn repeated_watcher_events_for_one_breakage_log_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "config.yml", "engine:\n  max_parallel: 4\n");
        reload_file(&path);

        std::fs::write(&path, "engine: [broken\n").unwrap();
        assert!(reload_file(&path), "first bad read logs");
        assert!(!reload_file(&path), "same breakage does not log again");
    }

    #[test]
    #[serial_test::serial(orch_home)]
    fn a_project_setting_overrides_the_global_one() {
        let (home, with, without) = (
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        );
        write_yaml(home.path(), "config.yml", "workflow:\n  sandbox: os\n");
        write_yaml(with.path(), ".orch.yml", "workflow:\n  sandbox: false\n");
        std::env::set_var("ORCH_HOME", home.path());

        let got = (
            get_for_project(with.path(), "workflow.sandbox").unwrap(),
            get_for_project(without.path(), "workflow.sandbox").unwrap(),
        );
        std::env::remove_var("ORCH_HOME");
        assert_eq!(got, ("false".to_string(), "os".to_string()));
    }

    #[test]
    fn extract_skills_list_empty_for_missing() {
        let yaml = serde_norway::from_str::<serde_norway::Value>("foo: bar").unwrap();
        let skills = extract_skills_list(&yaml).unwrap();
        assert!(skills.is_empty());
    }
}

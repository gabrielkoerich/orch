//! Config reader — loads YAML config files and resolves dot-separated keys.
//!
//! Reads `~/.orch/config.yml` (global) and `.orch.yml` (project).
//! Project config overrides global config for the same key.
//!
//! Features:
//! - In-memory cache: parsed YAML is cached for the process lifetime
//! - File watching: uses notify crate to watch for config file changes
//! - Hot reload: cache is invalidated when config files change
//! - Change notifications: subscribers receive events when config files change

use anyhow::Context;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::broadcast;

/// Cached YAML values — parsed once per file, reused for all key lookups.
/// Protected by RwLock for concurrent read access.
static CACHE: std::sync::LazyLock<RwLock<HashMap<PathBuf, serde_yml::Value>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

/// Files currently being watched for changes.
static WATCHER: std::sync::LazyLock<RwLock<HashMap<PathBuf, ()>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

/// Global file watcher instance (started on first use).
static FILE_WATCHER: std::sync::LazyLock<Arc<Mutex<Option<RecommendedWatcher>>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(None)));

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

/// Invalidate the cache entry for a specific config file
/// and notify subscribers of the change.
fn invalidate_cache(path: &PathBuf) {
    if let Ok(mut cache) = CACHE.write() {
        cache.remove(path);
        tracing::debug!("config cache invalidated for: {}", path.display());
    }
    // Notify subscribers (ignore error if no active receivers)
    let _ = CHANGE_TX.send(path.clone());
}

/// Start the file watcher if not already running.
fn ensure_watcher() {
    let mut watcher_guard = FILE_WATCHER.lock().unwrap();
    if watcher_guard.is_some() {
        return;
    }

    // Create the watcher
    let watcher = RecommendedWatcher::new(
        move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                for path in event.paths {
                    // Check if it's a config file we care about
                    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

                    if filename == "config.yml" || filename == ".orch.yml" {
                        invalidate_cache(&path);
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
                    let _ = watcher.watch(path, RecursiveMode::NonRecursive);
                    tracing::debug!("now watching config file: {}", path.display());
                }
            }
        }
    }
}

/// Resolve the global config path: `~/.orch/config.yml`
fn global_config_path() -> anyhow::Result<PathBuf> {
    crate::home::config_path()
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
    let project_path = PathBuf::from(".orch.yml");
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

/// Get all project repo slugs (owner/repo) from the registered projects.
///
/// Reads `projects:` paths from global config, then reads `gh.repo` from
/// each project's `.orch.yml`. Falls back to the global `gh.repo` key
/// for backward compatibility.
pub fn get_projects() -> anyhow::Result<Vec<String>> {
    let paths = get_project_paths()?;

    if !paths.is_empty() {
        let mut repos = Vec::new();
        for path_str in &paths {
            let path = std::path::PathBuf::from(path_str);
            let orch_yml = path.join(".orch.yml");

            // Try .orch.yml first, then .orchestrator.yml for backward compat
            let config_file = if orch_yml.exists() {
                orch_yml
            } else {
                let legacy = path.join(".orchestrator.yml");
                if legacy.exists() {
                    legacy
                } else {
                    tracing::warn!(
                        path = %path_str,
                        "project has no .orch.yml, skipping"
                    );
                    continue;
                }
            };

            if let Ok(content) = std::fs::read_to_string(&config_file) {
                if let Ok(doc) = serde_yml::from_str::<serde_yml::Value>(&content) {
                    if let Some(repo) = doc
                        .get("gh")
                        .and_then(|gh| gh.get("repo"))
                        .and_then(|r| r.as_str())
                    {
                        repos.push(repo.to_string());
                        continue;
                    }
                }
            }

            tracing::warn!(
                path = %path_str,
                "could not read gh.repo from project config"
            );
        }

        if !repos.is_empty() {
            return Ok(repos);
        }
    }

    // Fall back to global gh.repo for backward compatibility
    if let Ok(repo) = get("gh.repo") {
        if !repo.is_empty() {
            return Ok(vec![repo]);
        }
    }

    // Try legacy top-level repo key
    if let Ok(repo) = get("repo") {
        if !repo.is_empty() {
            return Ok(vec![repo]);
        }
    }

    anyhow::bail!("no projects configured — run `orch project add <path>` or set projects in ~/.orch/config.yml")
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
    let doc: serde_yml::Value = serde_yml::from_str(&content)?;

    doc.get("gh")
        .and_then(|gh| gh.get("repo"))
        .and_then(|r| r.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("gh.repo not set in {}", config_file.display()))
}

/// Resolve project path from CWD by finding the nearest `.orch.yml`.
///
/// Walks up from the current directory looking for `.orch.yml` or `.orchestrator.yml`.
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

/// Resolve a projects list from a config file.
fn resolve_projects_list(path: &PathBuf, key: &str) -> anyhow::Result<Vec<String>> {
    // First, ensure we're watching this file for changes
    watch_file(path);

    let root = {
        // Try to get from cache first (read lock)
        if let Ok(cache) = CACHE.read() {
            if let Some(cached) = cache.get(path) {
                return extract_projects_list(cached, key);
            }
        }

        // Not in cache, load and cache it (write lock)
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let parsed: serde_yml::Value =
            serde_yml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;

        if let Ok(mut cache) = CACHE.write() {
            cache.insert(path.clone(), parsed.clone());
        }

        parsed
    };

    // Try to extract projects list - if key doesn't exist, return empty vector
    match extract_projects_list(&root, key) {
        Ok(projects) => Ok(projects),
        Err(_) => Ok(vec![]),
    }
}

/// Extract a projects list from a YAML tree.
fn extract_projects_list(root: &serde_yml::Value, key: &str) -> anyhow::Result<Vec<String>> {
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
        serde_yml::Value::Sequence(seq) => {
            let mut projects = Vec::new();
            for item in seq {
                if let serde_yml::Value::String(s) = item {
                    projects.push(s.clone());
                } else if let serde_yml::Value::Mapping(map) = item {
                    // Support map format: { repo: "owner/repo", project_dir: "/path" }
                    if let Some(serde_yml::Value::String(repo)) = map.get("repo") {
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
    let root = {
        // First, ensure we're watching this file for changes
        watch_file(path);

        // Try to get from cache first (read lock)
        if let Ok(cache) = CACHE.read() {
            if let Some(cached) = cache.get(path) {
                return extract_value(cached, key);
            }
        }

        // Not in cache, load and cache it (write lock)
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let parsed: serde_yml::Value =
            serde_yml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;

        if let Ok(mut cache) = CACHE.write() {
            cache.insert(path.clone(), parsed.clone());
        }

        parsed
    };

    extract_value(&root, key)
}

/// Extract a value from a YAML tree by dot-separated key.
fn extract_value(root: &serde_yml::Value, key: &str) -> anyhow::Result<String> {
    let mut current = root;
    for part in key.split('.') {
        current = current
            .get(part)
            .with_context(|| format!("key not found: {key}"))?;
    }

    match current {
        serde_yml::Value::String(s) => Ok(s.clone()),
        serde_yml::Value::Number(n) => Ok(n.to_string()),
        serde_yml::Value::Bool(b) => Ok(b.to_string()),
        serde_yml::Value::Null => Ok(String::new()),
        _ => Ok(serde_yml::to_string(current)?),
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
            serde_yml::from_str::<serde_yml::Value>("projects:\n  - a\n  - b\n  - c\n").unwrap();
        let projects = extract_projects_list(&yaml, "projects").unwrap();
        assert_eq!(projects, vec!["a", "b", "c"]);
    }

    #[test]
    fn extract_projects_list_from_mapping_sequence() {
        let yaml = serde_yml::from_str::<serde_yml::Value>("projects:\n  - repo: a\n  - repo: b\n")
            .unwrap();
        let projects = extract_projects_list(&yaml, "projects").unwrap();
        assert_eq!(projects, vec!["a", "b"]);
    }

    #[test]
    fn extract_projects_list_empty_for_missing() {
        let yaml = serde_yml::from_str::<serde_yml::Value>("foo: bar").unwrap();
        let projects = extract_projects_list(&yaml, "projects").unwrap();
        assert!(projects.is_empty());
    }

    #[test]
    fn subscribe_receives_change_notifications() {
        let mut rx = subscribe();
        // Use a unique path to avoid interference from parallel tests
        let path = PathBuf::from(format!("/tmp/test-config-{}.yml", std::process::id()));

        // Manually call invalidate_cache to simulate a file change
        invalidate_cache(&path);

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
        // Use a unique path to avoid interference from parallel tests
        let path = PathBuf::from(format!("/tmp/test-config-multi-{}.yml", std::process::id()));

        invalidate_cache(&path);

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
}

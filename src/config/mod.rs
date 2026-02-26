//! Config reader — loads YAML config files and resolves dot-separated keys.
//!
//! Reads `~/.orchestrator/config.yml` (global) and `.orchestrator.yml` (project).
//! Project config overrides global config for the same key.
//!
//! Features:
//! - In-memory cache: parsed YAML is cached for the process lifetime
//! - File watching: uses notify crate to watch for config file changes
//! - Hot reload: cache is invalidated when config files change

use anyhow::Context;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

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

/// Invalidate the cache entry for a specific config file.
fn invalidate_cache(path: &PathBuf) {
    if let Ok(mut cache) = CACHE.write() {
        cache.remove(path);
        tracing::debug!("config cache invalidated for: {}", path.display());
    }
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

                    if filename == "config.yml" || filename == ".orchestrator.yml" {
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

/// Resolve the global config path: `~/.orchestrator/config.yml`
fn global_config_path() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".orchestrator").join("config.yml"))
}

/// Get a config value by dot-separated key (e.g. "agents.claude.model").
///
/// Lookup order:
/// 1. `.orchestrator.yml` in the current directory (project config)
/// 2. `~/.orchestrator/config.yml` (global config)
///
/// Returns the first match as a string.
/// Files are parsed once and cached for the process lifetime.
pub fn get(key: &str) -> anyhow::Result<String> {
    // Try project config first
    let project_path = PathBuf::from(".orchestrator.yml");
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

/// Get all projects configured in the global config.
///
/// Returns a list of project repo slugs (owner/repo).
/// Falls back to the single `repo` key if `projects` is not defined.
pub fn get_projects() -> anyhow::Result<Vec<String>> {
    let global_path = global_config_path()?;

    // Try to get projects list first
    if global_path.exists() {
        if let Ok(projects) = resolve_projects_list(&global_path, "projects") {
            if !projects.is_empty() {
                return Ok(projects);
            }
        }
    }

    // Fall back to single repo key
    let repo = get("repo")?;
    Ok(vec![repo])
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
}

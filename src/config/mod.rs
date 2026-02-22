//! Config reader — loads YAML config files and resolves dot-separated keys.
//!
//! Reads `~/.orchestrator/config.yml` (global) and `.orchestrator.yml` (project).
//! Project config overrides global config for the same key.

use anyhow::Context;
use std::path::PathBuf;

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

/// Resolve a dot-separated key from a YAML file.
fn resolve_key(path: &PathBuf, key: &str) -> anyhow::Result<String> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let root: serde_yml::Value =
        serde_yml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;

    let mut current = &root;
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
}

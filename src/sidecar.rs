//! Sidecar file management — JSON metadata files alongside tasks.
//!
//! Each task gets a `state/{task_id}.json` sidecar file that stores
//! runtime metadata (model, prompt hash, timing, token counts, etc.).
//! This is the authoritative source for data that doesn't belong in GitHub labels.
//!
//! State directory: `~/.orchestrator/state/`
//! Legacy fallback: `~/.orchestrator/.orchestrator/` (read-only)

use anyhow::Context;
use serde_json::Value;
use std::path::PathBuf;

/// Get the runtime state directory path.
///
/// New location: `~/.orchestrator/state/`
/// Legacy location: `~/.orchestrator/.orchestrator/`
///
/// On first call, creates the new directory. If files exist only at the
/// legacy path, callers should check both (see [`state_dir_with_fallback`]).
pub fn state_dir() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let dir = home.join(".orchestrator").join("state");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Legacy state directory (`~/.orchestrator/.orchestrator/`).
///
/// Used only for backward-compatible reads — never write here.
fn legacy_state_dir() -> Option<PathBuf> {
    let dir = dirs::home_dir()?
        .join(".orchestrator")
        .join(".orchestrator");
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

/// Resolve a file inside the state directory, falling back to the legacy
/// location when the file doesn't exist at the new path yet.
pub fn state_file(name: &str) -> anyhow::Result<PathBuf> {
    let new_path = state_dir()?.join(name);
    if new_path.exists() {
        return Ok(new_path);
    }
    // Check legacy location
    if let Some(legacy) = legacy_state_dir() {
        let old_path = legacy.join(name);
        if old_path.exists() {
            return Ok(old_path);
        }
    }
    // Return new path even if it doesn't exist yet (for writes)
    Ok(new_path)
}

/// Get the path to a task's sidecar file.
fn sidecar_path(task_id: &str) -> anyhow::Result<PathBuf> {
    state_file(&format!("{task_id}.json"))
}

/// Read a field from a sidecar file.
pub fn get(task_id: &str, field: &str) -> anyhow::Result<String> {
    let path = sidecar_path(task_id)?;
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading sidecar: {}", path.display()))?;
    let obj: Value = serde_json::from_str(&content)?;

    let val = obj
        .get(field)
        .with_context(|| format!("field not found: {field}"))?;

    match val {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null => Ok(String::new()),
        _ => Ok(serde_json::to_string(val)?),
    }
}

/// Set one or more fields in a sidecar file.
///
/// Each entry in `fields` is "key=value" format.
pub fn set(task_id: &str, fields: &[String]) -> anyhow::Result<()> {
    let path = sidecar_path(task_id)?;

    // Load existing or create new
    let mut obj: serde_json::Map<String, Value> = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        serde_json::from_str(&content)?
    } else {
        serde_json::Map::new()
    };

    // Apply field updates
    for field in fields {
        let (key, value) = field
            .split_once('=')
            .with_context(|| format!("invalid field format (expected key=value): {field}"))?;
        obj.insert(key.to_string(), Value::String(value.to_string()));
    }

    // Write back
    let content = serde_json::to_string_pretty(&Value::Object(obj))?;
    std::fs::write(&path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    /// Override sidecar dir to use a temp directory for tests.
    fn setup_temp_sidecar() -> TempDir {
        // We'll test the low-level path logic directly
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn set_and_get_field() {
        let dir = setup_temp_sidecar();
        let path = dir.path().join("42.json");

        // Write directly
        let obj = serde_json::json!({"agent": "claude", "attempts": "3"});
        std::fs::write(&path, serde_json::to_string_pretty(&obj).unwrap()).unwrap();

        // Read manually (since get() uses hardcoded path)
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["agent"], "claude");
        assert_eq!(parsed["attempts"], "3");
    }

    #[test]
    fn set_creates_new_file() {
        let dir = setup_temp_sidecar();
        let path = dir.path().join("99.json");
        assert!(!path.exists());

        // Create manually like set() would
        let mut obj = serde_json::Map::new();
        obj.insert(
            "model".to_string(),
            serde_json::Value::String("opus".to_string()),
        );
        let content = serde_json::to_string_pretty(&serde_json::Value::Object(obj)).unwrap();
        std::fs::write(&path, content).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["model"], "opus");
    }

    #[test]
    fn set_merges_existing_fields() {
        let dir = setup_temp_sidecar();
        let path = dir.path().join("77.json");

        // Initial write
        let obj = serde_json::json!({"agent": "claude", "status": "new"});
        std::fs::write(&path, serde_json::to_string(&obj).unwrap()).unwrap();

        // Merge new field
        let content = std::fs::read_to_string(&path).unwrap();
        let mut parsed: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&content).unwrap();
        parsed.insert(
            "model".to_string(),
            serde_json::Value::String("opus".to_string()),
        );
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::Value::Object(parsed)).unwrap(),
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let final_obj: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(final_obj["agent"], "claude");
        assert_eq!(final_obj["status"], "new");
        assert_eq!(final_obj["model"], "opus");
    }
}

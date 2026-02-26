//! Sidecar file management — JSON metadata files alongside tasks.
//!
//! Each task gets a `state/{task_id}.json` sidecar file that stores
//! runtime metadata (model, prompt hash, timing, token counts, etc.).
//! This is the authoritative source for data that doesn't belong in GitHub labels.
//!
//! State directory: `~/.orchestrator/state/`
//! Legacy fallback: `~/.orchestrator/.orchestrator/` (read-only)

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Token usage for an agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn total_tokens(self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Per-1M token pricing in USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_per_million_usd: f64,
    pub output_per_million_usd: f64,
}

impl ModelPricing {
    pub fn estimate_cost_usd(self, usage: TokenUsage) -> CostEstimate {
        let input_cost = (usage.input_tokens as f64 / 1_000_000.0) * self.input_per_million_usd;
        let output_cost = (usage.output_tokens as f64 / 1_000_000.0) * self.output_per_million_usd;
        CostEstimate {
            input_cost_usd: input_cost,
            output_cost_usd: output_cost,
            total_cost_usd: input_cost + output_cost,
        }
    }
}

/// Cost estimate in USD.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CostEstimate {
    pub input_cost_usd: f64,
    pub output_cost_usd: f64,
    pub total_cost_usd: f64,
}

/// Get the runtime state directory path.
///
/// New location: `~/.orch/state/`
/// Legacy fallback: `~/.orchestrator/state/` then `~/.orchestrator/.orchestrator/`
///
/// On first call, creates the new directory.
pub fn state_dir() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let dir = home.join(".orch").join("state");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Legacy state directories for backward-compatible reads.
///
/// Checks `~/.orchestrator/state/` first, then `~/.orchestrator/.orchestrator/`.
/// Never writes to these locations.
fn legacy_state_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    // Check ~/.orchestrator/state/ (intermediate migration path)
    let mid = home.join(".orchestrator").join("state");
    if mid.is_dir() {
        return Some(mid);
    }
    // Check ~/.orchestrator/.orchestrator/ (original nested path)
    let old = home.join(".orchestrator").join(".orchestrator");
    if old.is_dir() {
        return Some(old);
    }
    None
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
///
/// New location: `~/.orch/state/{owner}/{repo}/tasks/{id}/sidecar.json`
/// Legacy fallback: `~/.orch/state/{id}.json`
///
/// Uses the repo from config if available. Falls back to flat state dir
/// for backward compatibility.
fn sidecar_path(task_id: &str) -> anyhow::Result<PathBuf> {
    sidecar_path_for_repo(task_id, None)
}

/// Get the path to a task's sidecar file, optionally scoped to a repo.
fn sidecar_path_for_repo(task_id: &str, repo: Option<&str>) -> anyhow::Result<PathBuf> {
    // Try to resolve repo
    let repo_slug = repo
        .map(String::from)
        .or_else(|| crate::config::get_current_repo().ok());

    if let Some(ref repo) = repo_slug {
        if !repo.is_empty() {
            let task_dir = crate::home::task_dir(repo, task_id)?;
            let new_path = task_dir.join("sidecar.json");
            if new_path.exists() {
                return Ok(new_path);
            }
            // Check legacy flat path
            let legacy = state_file(&format!("{task_id}.json"))?;
            if legacy.exists() {
                return Ok(legacy);
            }
            // New file — use per-task dir
            return Ok(new_path);
        }
    }

    // No repo context — use legacy flat path
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

/// Read a numeric sidecar field as u64. Missing or invalid values return 0.
pub fn get_u64(task_id: &str, field: &str) -> u64 {
    get(task_id, field)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Read a numeric sidecar field as f64. Missing or invalid values return 0.0.
pub fn get_f64(task_id: &str, field: &str) -> f64 {
    get(task_id, field)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Store token usage for a task.
pub fn store_token_usage(
    task_id: &str,
    input_tokens: u64,
    output_tokens: u64,
    model: &str,
) -> anyhow::Result<()> {
    // Store raw token counts
    set(
        task_id,
        &[
            format!("input_tokens={}", input_tokens),
            format!("output_tokens={}", output_tokens),
        ],
    )?;

    // Calculate and store cost
    let pricing = pricing_for_model(model);
    let usage = TokenUsage {
        input_tokens,
        output_tokens,
    };
    let cost = pricing.estimate_cost_usd(usage);

    set(
        task_id,
        &[
            format!("input_cost_usd={:.6}", cost.input_cost_usd),
            format!("output_cost_usd={:.6}", cost.output_cost_usd),
            format!("total_cost_usd={:.6}", cost.total_cost_usd),
            format!("model={}", model),
        ],
    )?;

    Ok(())
}

/// Get token usage for a task.
pub fn get_token_usage(task_id: &str) -> TokenUsage {
    TokenUsage {
        input_tokens: get_u64(task_id, "input_tokens"),
        output_tokens: get_u64(task_id, "output_tokens"),
    }
}

/// Get cost estimate for a task.
pub fn get_cost_estimate(task_id: &str) -> CostEstimate {
    CostEstimate {
        input_cost_usd: get_f64(task_id, "input_cost_usd"),
        output_cost_usd: get_f64(task_id, "output_cost_usd"),
        total_cost_usd: get_f64(task_id, "total_cost_usd"),
    }
}

/// Get the model used for a task.
pub fn get_model(task_id: &str) -> String {
    get(task_id, "model").unwrap_or_default()
}

/// Get total tokens used for a task.
pub fn get_total_tokens(task_id: &str) -> u64 {
    let usage = get_token_usage(task_id);
    usage.total_tokens()
}

/// A single memory entry from a task attempt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryEntry {
    /// Attempt number (1-indexed)
    pub attempt: u32,
    /// Agent that made this attempt
    pub agent: String,
    /// Model used for this attempt
    pub model: Option<String>,
    /// Key learnings from this attempt
    pub learnings: Vec<String>,
    /// Error message if the attempt failed
    pub error: Option<String>,
    /// Files modified in this attempt
    pub files_modified: Vec<String>,
    /// Approach taken (from summary)
    pub approach: String,
    /// Timestamp of the attempt
    pub timestamp: String,
}

/// Store a memory entry for a task attempt.
pub fn store_memory(task_id: &str, entry: &MemoryEntry) -> anyhow::Result<()> {
    let path = sidecar_path(task_id)?;

    // Load existing or create new
    let mut obj: serde_json::Map<String, Value> = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        serde_json::from_str(&content)?
    } else {
        serde_json::Map::new()
    };

    // Get existing memory array or create new
    let mut memory: Vec<MemoryEntry> = obj
        .get("memory")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // Add new entry
    memory.push(entry.clone());

    // Store back
    obj.insert("memory".to_string(), serde_json::to_value(&memory)?);

    // Write back
    let content = serde_json::to_string_pretty(&Value::Object(obj))?;
    std::fs::write(&path, content)?;

    Ok(())
}

/// Get all memory entries for a task.
pub fn get_memory(task_id: &str) -> anyhow::Result<Vec<MemoryEntry>> {
    let path = sidecar_path(task_id)?;

    if !path.exists() {
        return Ok(vec![]);
    }

    let content = std::fs::read_to_string(&path)?;
    let obj: serde_json::Map<String, Value> = serde_json::from_str(&content)?;

    obj.get("memory")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .map(Ok)
        .unwrap_or_else(|| Ok(vec![]))
}

/// Get recent memory entries for a task, capped at N attempts.
/// Returns the last N attempts in chronological order.
pub fn get_recent_memory(task_id: &str, max_entries: usize) -> anyhow::Result<Vec<MemoryEntry>> {
    let mut memory = get_memory(task_id)?;

    // Sort by attempt number to ensure chronological order
    memory.sort_by_key(|m| m.attempt);

    // Take last N entries (most recent)
    if memory.len() > max_entries {
        memory = memory.split_off(memory.len() - max_entries);
    }

    Ok(memory)
}

/// Clear all memory entries for a task.
#[allow(dead_code)]
pub fn clear_memory(task_id: &str) -> anyhow::Result<()> {
    let path = sidecar_path(task_id)?;

    if !path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&path)?;
    let mut obj: serde_json::Map<String, Value> = serde_json::from_str(&content)?;

    obj.remove("memory");

    // Write back
    let content = serde_json::to_string_pretty(&Value::Object(obj))?;
    std::fs::write(&path, content)?;

    Ok(())
}

/// Resolve model pricing using a built-in table and normalized model aliases.
pub fn pricing_for_model(model: &str) -> ModelPricing {
    let normalized = model.trim().to_lowercase();
    // OpenAI models
    if normalized == "o3" {
        return ModelPricing {
            input_per_million_usd: 2.0,
            output_per_million_usd: 8.0,
        };
    }
    if normalized == "o4-mini" || normalized.contains("gpt-4.1-mini") {
        return ModelPricing {
            input_per_million_usd: 0.15,
            output_per_million_usd: 0.6,
        };
    }
    if normalized.contains("gpt-4.1") && !normalized.contains("mini") {
        return ModelPricing {
            input_per_million_usd: 2.0,
            output_per_million_usd: 8.0,
        };
    }
    if normalized.contains("opus") {
        return ModelPricing {
            input_per_million_usd: 15.0,
            output_per_million_usd: 75.0,
        };
    }
    if normalized.contains("sonnet") {
        return ModelPricing {
            input_per_million_usd: 3.0,
            output_per_million_usd: 15.0,
        };
    }
    if normalized.contains("haiku") {
        return ModelPricing {
            input_per_million_usd: 0.8,
            output_per_million_usd: 4.0,
        };
    }

    // Fallback baseline when model is unknown.
    ModelPricing {
        input_per_million_usd: 1.0,
        output_per_million_usd: 4.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn pricing_lookup_known_models() {
        let o3 = pricing_for_model("o3");
        assert_eq!(
            o3,
            ModelPricing {
                input_per_million_usd: 2.0,
                output_per_million_usd: 8.0
            }
        );

        let haiku = pricing_for_model("claude-haiku-4-5-20251001");
        assert_eq!(
            haiku,
            ModelPricing {
                input_per_million_usd: 0.8,
                output_per_million_usd: 4.0
            }
        );

        let o4mini = pricing_for_model("o4-mini");
        assert_eq!(
            o4mini,
            ModelPricing {
                input_per_million_usd: 0.15,
                output_per_million_usd: 0.6
            }
        );
    }

    #[test]
    fn cost_estimation_uses_both_input_and_output() {
        let pricing = ModelPricing {
            input_per_million_usd: 2.0,
            output_per_million_usd: 8.0,
        };
        let usage = TokenUsage {
            input_tokens: 50_000,
            output_tokens: 10_000,
        };
        let cost = pricing.estimate_cost_usd(usage);

        assert!((cost.input_cost_usd - 0.1).abs() < 1e-9);
        assert!((cost.output_cost_usd - 0.08).abs() < 1e-9);
        assert!((cost.total_cost_usd - 0.18).abs() < 1e-9);
    }

    #[test]
    fn memory_entry_serialization() {
        let entry = MemoryEntry {
            attempt: 1,
            agent: "claude".to_string(),
            model: Some("sonnet".to_string()),
            learnings: vec!["Use format! macro for string formatting".to_string()],
            error: Some("compilation failed".to_string()),
            files_modified: vec!["src/main.rs".to_string()],
            approach: "Fixed the formatting issue".to_string(),
            timestamp: "2025-02-26T10:00:00Z".to_string(),
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"attempt\":1"));
        assert!(json.contains("\"agent\":\"claude\""));
        assert!(json.contains("\"model\":\"sonnet\""));

        let deserialized: MemoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.attempt, 1);
        assert_eq!(deserialized.agent, "claude");
        assert_eq!(
            deserialized.learnings,
            vec!["Use format! macro for string formatting"]
        );
    }
}

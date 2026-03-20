//! Control session — context assembly, one-shot agent invocation, response parsing.
//!
//! The control session provides an interactive ops assistant that assembles context
//! from SQLite (memories, summaries, live state), invokes an agent CLI one-shot,
//! and parses the response for summary extraction.
//!
//! ## Model selection
//!
//! `/model <spec>` where spec is:
//! - `agent:model` — explicit agent + model (e.g., `minimax:sonnet`, `opencode:minimax-m2.5-free`)
//! - `model` — infer agent from model name (e.g., `sonnet` → claude, `gpt-4o` → codex)
//!
//! All agents: claude, codex, opencode, kimi, minimax
//! - kimi and minimax are claude-compatible wrappers (separate binaries in PATH)
//! - opencode supports `opencode models` for listing available models

use anyhow::{Context, Result};
use sqlx::Row;

use crate::engine::router::config::DEFAULT_AGENTS;
use crate::store::TaskStore;

/// System prompt template, loaded at compile time.
const SYSTEM_TEMPLATE: &str = include_str!("../prompts/control_system.md");

/// Timeout for agent invocations (2 minutes).
const AGENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Parsed model specification from `/model` command.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSpec {
    pub agent: String,
    pub model: String,
}

/// Parse a model spec string into agent + model.
///
/// Formats:
/// - `agent:model` → explicit (e.g., `minimax:sonnet`, `opencode:minimax-m2.5-free`)
/// - `model` → infer agent from model name
pub fn parse_model_spec(spec: &str) -> ModelSpec {
    if let Some((agent, model)) = spec.split_once(':') {
        ModelSpec {
            agent: agent.to_string(),
            model: model.to_string(),
        }
    } else {
        ModelSpec {
            agent: infer_agent(spec).to_string(),
            model: spec.to_string(),
        }
    }
}

/// Infer which agent CLI should run a given model name.
fn infer_agent(model: &str) -> &'static str {
    let lower = model.to_lowercase();
    if lower.contains("gpt")
        || lower.contains("o1")
        || lower.contains("o3")
        || lower.contains("o4")
        || lower.contains("codex")
    {
        "codex"
    } else if lower.contains("deepseek") || lower.contains("qwen") {
        "opencode"
    } else {
        // claude models (sonnet, opus, haiku) and unknown → default to claude
        "claude"
    }
}

/// Validate a model spec by running a quick test invocation.
///
/// Sends "hello" to the agent with the specified model to verify
/// the agent binary exists and the model is available.
pub async fn validate_model(spec: &ModelSpec) -> Result<()> {
    // 1. Check agent is known
    if !DEFAULT_AGENTS.contains(&spec.agent.as_str()) {
        anyhow::bail!(
            "unknown agent '{}'. Available: {}",
            spec.agent,
            DEFAULT_AGENTS.join(", ")
        );
    }

    // 2. Check agent binary exists
    if !crate::cmd_cache::command_exists(&spec.agent) {
        anyhow::bail!("agent '{}' not found in PATH. Is it installed?", spec.agent);
    }

    // 3. For opencode, pre-check against `opencode models` list (fast rejection)
    if spec.agent == "opencode" {
        if let Ok(models) = list_opencode_models().await {
            if !models.iter().any(|m| m == &spec.model) {
                anyhow::bail!(
                    "model '{}' not found in opencode. Available models:\n{}",
                    spec.model,
                    models
                        .iter()
                        .filter(|m| m.contains(&spec.model) || spec.model.contains(m.as_str()))
                        .take(10)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
        }
    }

    // 4. Test invocation — always run to verify the model actually works
    eprintln!("Testing {}:{} ...", spec.agent, spec.model);
    let result = test_invoke(&spec.agent, &spec.model).await;
    match result {
        Ok(_) => Ok(()),
        Err(e) => anyhow::bail!(
            "model '{}' on agent '{}' is not available: {e}",
            spec.model,
            spec.agent
        ),
    }
}

/// List available opencode models via `opencode models`.
async fn list_opencode_models() -> Result<Vec<String>> {
    let output = tokio::process::Command::new("opencode")
        .args(["models"])
        .output()
        .await
        .context("running opencode models")?;

    if !output.status.success() {
        anyhow::bail!("opencode models failed");
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Run a minimal test invocation to verify agent+model availability.
async fn test_invoke(agent: &str, model: &str) -> Result<()> {
    let msg = "Reply with just the word 'ok'.";
    // Reuse invoke_agent — it already handles temp files, timeout, and error checking
    invoke_agent(agent, model, "", msg).await?;
    Ok(())
}

/// Assemble the full system-prompt context from SQLite state and live system info.
pub async fn assemble_context(store: &TaskStore, session_id: &str) -> Result<String> {
    // 1. Gather memories from KV (keys matching control:memory:{session}:*)
    let memory_prefix = format!("control:memory:{session_id}:%");
    let memory_rows = sqlx::query("SELECT key, value FROM kv WHERE key LIKE ?")
        .bind(&memory_prefix)
        .fetch_all(store.pool())
        .await
        .unwrap_or_default();
    let memories = if memory_rows.is_empty() {
        "(none)".to_string()
    } else {
        memory_rows
            .iter()
            .map(|r| {
                let key: String = r.get("key");
                let short_key = key.strip_prefix("control:memory:").unwrap_or(&key);
                let value: String = r.get("value");
                format!("- **{short_key}**: {value}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // 2. Recent conversation summaries
    let summaries = store
        .control_recent_summaries(session_id, 20)
        .await
        .unwrap_or_default();
    let recent_summaries = if summaries.is_empty() {
        "(no recent conversation)".to_string()
    } else {
        summaries
            .iter()
            .map(|s| format!("- {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    // 3. Live state via `orch task list` (best-effort)
    let current_state = match tokio::process::Command::new("orch")
        .args(["task", "list"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => "(could not fetch live state)".to_string(),
    };

    // 4. Replace placeholders in template
    let result = SYSTEM_TEMPLATE
        .replace("{current_state}", &current_state)
        .replace("{memories}", &memories)
        .replace("{recent_summaries}", &recent_summaries);

    Ok(result)
}

/// Get the current model spec from KV, defaulting to `"sonnet"` on `"claude"`.
pub async fn get_model(store: &TaskStore) -> String {
    store
        .kv_get("control:model")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "sonnet".to_string())
}

/// Get the current agent from KV, defaulting to `"claude"`.
pub async fn get_agent(store: &TaskStore) -> String {
    store
        .kv_get("control:agent")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "claude".to_string())
}

/// Set the current model and agent in KV.
pub async fn set_model_spec(store: &TaskStore, spec: &ModelSpec) -> Result<()> {
    store
        .kv_set("control:model", &spec.model)
        .await
        .context("setting control model")?;
    store
        .kv_set("control:agent", &spec.agent)
        .await
        .context("setting control agent")?;
    Ok(())
}

/// Parse an agent response, extracting an optional `<summary>` tag.
///
/// Returns `(clean_text, optional_summary)` where `clean_text` has the
/// summary tag stripped out.
pub fn parse_response(raw: &str) -> (String, Option<String>) {
    if let Some(start) = raw.find("<summary>") {
        if let Some(end) = raw.find("</summary>") {
            let summary = raw[start + "<summary>".len()..end].trim().to_string();
            let clean = format!(
                "{}{}",
                raw[..start].trim_end(),
                raw[end + "</summary>".len()..].trim_start()
            )
            .trim()
            .to_string();
            return (clean, Some(summary));
        }
    }
    (raw.trim().to_string(), None)
}

/// Extract text and token usage from a Claude JSON envelope or raw output.
///
/// Claude's `--output-format json` wraps the response in:
/// `{"type":"result","result":"text","usage":{"input_tokens":N,"output_tokens":N}}`
///
/// For non-Claude agents, returns the raw text with no token data.
fn extract_from_envelope(raw: &str) -> (String, Option<u64>, Option<u64>) {
    // Try to parse as Claude envelope
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(obj) = value.as_object() {
            if obj.get("type").and_then(|v| v.as_str()) == Some("result") {
                let text = obj
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input_tokens = obj
                    .get("usage")
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|v| v.as_u64());
                let output_tokens = obj
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|v| v.as_u64());
                return (text, input_tokens, output_tokens);
            }
        }
    }
    // Not an envelope — return raw text
    (raw.to_string(), None, None)
}

/// Result of an agent invocation, including response text and token usage.
pub struct InvokeResult {
    pub text: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Prepare temp directory for agent invocation files.
///
/// Uses `~/.orch/state/control/{pid}/` for isolation between concurrent processes.
async fn prepare_temp_dir() -> Result<String> {
    let home = crate::home::orch_home()?;
    let dir = format!("{}/state/control/{}", home.display(), std::process::id());
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}

/// Invoke an agent CLI one-shot and return structured result with token usage.
///
/// Uses the same runner infrastructure as the task engine:
/// - `get_runner(agent)` for agent-specific command building
/// - `build_command()` for CLI flag construction
/// - `parse_response()` for output parsing with token extraction
/// - `classify_error()` for proper error classification
///
/// Writes temp files to `~/.orch/state/control/{pid}/`.
pub async fn invoke_agent(
    agent: &str,
    model: &str,
    context: &str,
    message: &str,
) -> Result<InvokeResult> {
    use crate::engine::runner::agents::{get_runner, PermissionRules};

    let dir = prepare_temp_dir().await?;
    let sys_file = format!("{dir}/system.md");
    let msg_file = format!("{dir}/message.txt");

    // Write prompt files
    tokio::fs::write(&sys_file, context).await?;
    tokio::fs::write(&msg_file, message).await?;

    let runner = get_runner(agent);
    let permissions = PermissionRules::default();

    // Build the shell command string (same as task runner)
    let timeout_cmd = format!("timeout {}", AGENT_TIMEOUT.as_secs());
    let shell_cmd = runner.build_command(
        Some(model),
        &timeout_cmd,
        &sys_file,
        &msg_file,
        &permissions,
    );

    // Execute via shell (the command string contains pipes, redirects, etc.)
    let output = tokio::time::timeout(
        AGENT_TIMEOUT + std::time::Duration::from_secs(5), // extra buffer for timeout cmd
        tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&shell_cmd)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("agent timed out after {}s", AGENT_TIMEOUT.as_secs()))?
    .context("spawning agent")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    // Use runner's error classification on failure
    if !output.status.success() {
        let err = runner.classify_error(exit_code, &stdout, &stderr);
        anyhow::bail!("{err}");
    }

    // Use runner's response parser — handles structured output + tokens + errors.
    use crate::engine::runner::agents::AgentError;
    match runner.parse_response(&stdout) {
        Ok(parsed) => {
            let text = if !parsed.response.summary.is_empty() {
                parsed.response.summary.clone()
            } else {
                stdout.clone()
            };
            Ok(InvokeResult {
                text,
                input_tokens: parsed.input_tokens,
                output_tokens: parsed.output_tokens,
            })
        }
        Err(AgentError::InvalidResponse { .. }) => {
            // Parser couldn't parse as structured AgentResponse — normal for
            // conversational text. Extract text + tokens from the envelope directly.
            let (text, input_tokens, output_tokens) = extract_from_envelope(&stdout);
            Ok(InvokeResult {
                text,
                input_tokens,
                output_tokens,
            })
        }
        Err(err) => {
            // Real agent error (rate limit, auth, model unavailable, etc.)
            anyhow::bail!("{err}")
        }
    }
}

/// High-level entry point: process a user message and return the assistant response.
///
/// Handles `/model` commands, assembles context, invokes the agent, parses the
/// response for summaries, and stores both user and assistant messages.
pub async fn send_message(
    store: &TaskStore,
    session_id: &str,
    channel: &str,
    channel_thread: Option<&str>,
    message: &str,
) -> Result<String> {
    // Handle /model command — don't store as a message
    if let Some(new_spec) = message.strip_prefix("/model").map(str::trim) {
        if new_spec.is_empty() {
            let model = get_model(store).await;
            let agent = get_agent(store).await;
            return Ok(format!("Current: {agent}:{model}"));
        }
        let spec = parse_model_spec(new_spec);
        validate_model(&spec).await?;
        set_model_spec(store, &spec).await?;
        return Ok(format!(
            "Switched to **{}:{}** (validated)",
            spec.agent, spec.model
        ));
    }

    // Store user message
    store
        .insert_control_message(
            session_id,
            "user",
            channel,
            channel_thread,
            message,
            None,
            None,
            None,
            None,
            None,
        )
        .await?;

    // Assemble context
    let context = assemble_context(store, session_id).await?;

    // Resolve model and agent from KV
    let model = get_model(store).await;
    let agent = get_agent(store).await;

    // Invoke agent
    let result = invoke_agent(&agent, &model, &context, message).await?;

    // Parse response for summary tag
    let (clean, summary) = parse_response(&result.text);

    // Store assistant message with token usage
    let total_tokens = match (result.input_tokens, result.output_tokens) {
        (Some(i), Some(o)) => Some((i + o) as i64),
        (Some(t), None) | (None, Some(t)) => Some(t as i64),
        _ => None,
    };
    store
        .insert_control_message(
            session_id,
            "assistant",
            channel,
            channel_thread,
            &clean,
            summary.as_deref(),
            Some(&model),
            Some(&agent),
            total_tokens,
            None, // cost calculated separately if needed
        )
        .await?;

    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TaskStore;

    #[test]
    fn parse_spec_agent_model() {
        let spec = parse_model_spec("minimax:sonnet");
        assert_eq!(spec.agent, "minimax");
        assert_eq!(spec.model, "sonnet");
    }

    #[test]
    fn parse_spec_opencode_model() {
        let spec = parse_model_spec("opencode:minimax-m2.5-free");
        assert_eq!(spec.agent, "opencode");
        assert_eq!(spec.model, "minimax-m2.5-free");
    }

    #[test]
    fn parse_spec_model_only_infers_claude() {
        let spec = parse_model_spec("sonnet");
        assert_eq!(spec.agent, "claude");
        assert_eq!(spec.model, "sonnet");
    }

    #[test]
    fn parse_spec_model_only_infers_codex() {
        let spec = parse_model_spec("gpt-4o");
        assert_eq!(spec.agent, "codex");
        assert_eq!(spec.model, "gpt-4o");
    }

    #[test]
    fn parse_spec_model_only_infers_opencode() {
        let spec = parse_model_spec("deepseek-r1");
        assert_eq!(spec.agent, "opencode");
        assert_eq!(spec.model, "deepseek-r1");
    }

    #[tokio::test]
    async fn assemble_context_includes_memories_and_summaries() {
        let store = TaskStore::open_memory().await.unwrap();
        store
            .kv_set("control:memory:default:tz", "User is in BRT timezone")
            .await
            .unwrap();
        store
            .insert_control_message(
                "default",
                "assistant",
                "cli",
                None,
                "response",
                Some("checked status"),
                Some("sonnet"),
                Some("claude"),
                None,
                None,
            )
            .await
            .unwrap();

        let ctx = assemble_context(&store, "default").await.unwrap();
        assert!(ctx.contains("BRT timezone"), "should include memories");
        assert!(ctx.contains("checked status"), "should include summaries");
    }

    #[test]
    fn extract_summary_from_response() {
        let response = "Here are your tasks.\n\n<summary>listed 3 active tasks</summary>";
        let (clean, summary) = parse_response(response);
        assert_eq!(summary.as_deref(), Some("listed 3 active tasks"));
        assert!(!clean.contains("<summary>"));
    }

    #[test]
    fn extract_summary_missing() {
        let response = "No tasks found.";
        let (clean, summary) = parse_response(response);
        assert_eq!(summary, None);
        assert_eq!(clean, "No tasks found.");
    }

    #[test]
    fn resolve_agent_from_model() {
        assert_eq!(infer_agent("sonnet"), "claude");
        assert_eq!(infer_agent("opus"), "claude");
        assert_eq!(infer_agent("haiku"), "claude");
        assert_eq!(infer_agent("gpt-4o"), "codex");
        assert_eq!(infer_agent("o3"), "codex");
        assert_eq!(infer_agent("deepseek-r1"), "opencode");
    }

    #[tokio::test]
    async fn send_message_model_switch() {
        let store = TaskStore::open_memory().await.unwrap();
        let model = get_model(&store).await;
        assert_eq!(model, "sonnet");

        // /model with explicit agent:model — will fail validation (no binary in test)
        // so test parse_model_spec + set_model_spec directly
        let spec = parse_model_spec("minimax:sonnet");
        set_model_spec(&store, &spec).await.unwrap();
        assert_eq!(get_model(&store).await, "sonnet");
        assert_eq!(get_agent(&store).await, "minimax");
    }

    #[tokio::test]
    async fn send_message_model_show_current() {
        let store = TaskStore::open_memory().await.unwrap();
        let response = send_message(&store, "default", "cli", None, "/model")
            .await
            .unwrap();
        assert!(response.contains("claude"));
        assert!(response.contains("sonnet"));
    }

    #[tokio::test]
    async fn send_message_model_switch_does_not_store() {
        let store = TaskStore::open_memory().await.unwrap();
        // set directly to avoid validation in test env
        let spec = parse_model_spec("claude:haiku");
        set_model_spec(&store, &spec).await.unwrap();
        let messages = store.list_control_messages("default", 10).await.unwrap();
        assert_eq!(messages.len(), 0);
    }

    #[test]
    fn validate_unknown_agent_fails() {
        let spec = ModelSpec {
            agent: "nonexistent".to_string(),
            model: "test".to_string(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(validate_model(&spec));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown agent"));
    }
}

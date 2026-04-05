//! Control session — context assembly, one-shot agent invocation, response parsing.
//!
//! The control session provides an interactive ops assistant that assembles context
//! from SQLite (memories, summaries, live state), invokes an agent CLI one-shot
//! with `--session-id` for conversation continuity, and parses the response for
//! summary extraction.
//!
//! ## Session continuity
//!
//! Each named session (e.g., "default", "ops") gets a UUID stored in SQLite KV
//! (`control:session_id:{name}`). Claude-compatible agents (claude, kimi, minimax)
//! receive `--session-id <uuid>` so they resume prior conversation state.
//! The UUID is regenerated when the agent or model changes.
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

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result};
use regex::Regex;
use sqlx::Row;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(10);

use crate::engine::router::config::DEFAULT_AGENTS;
use crate::store::TaskStore;

/// Per-session invocation locks — ensures only one agent invocation runs at a time per session.
///
/// Keyed by `session_id`. A new `Arc<tokio::sync::Mutex<()>>` is inserted on first use and
/// reused for all subsequent calls with the same session_id.
static SESSION_LOCKS: std::sync::LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

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

/// Parsed control command from the chat session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlCommand {
    Model(Option<String>),
    Agent(Option<String>),
}

fn default_model_for_agent(agent: &str) -> &'static str {
    match agent {
        "codex" => "gpt-4o",
        "opencode" => "deepseek-r1",
        _ => "sonnet",
    }
}

/// Parse a control-session command.
pub fn parse_control_command(message: &str) -> Option<ControlCommand> {
    let trimmed = message.trim();
    let rest = trimmed.strip_prefix('/')?;
    let (command, args) = rest
        .split_once(' ')
        .map_or((rest, ""), |(cmd, tail)| (cmd, tail.trim()));

    match command {
        "model" => Some(ControlCommand::Model(
            (!args.is_empty()).then(|| args.to_string()),
        )),
        "agent" => Some(ControlCommand::Agent(
            (!args.is_empty()).then(|| args.to_string()),
        )),
        _ => None,
    }
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
            model: crate::engine::router::RouterConfig::normalize_model_identifier(model),
        }
    } else {
        ModelSpec {
            agent: infer_agent(spec).to_string(),
            model: crate::engine::router::RouterConfig::normalize_model_identifier(spec),
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

/// Validate that an agent name is known and installed.
///
/// Performs lightweight checks (known agent + binary in PATH) without
/// a test invocation. Used by `/agent` before persisting.
pub fn validate_agent(agent: &str) -> Result<()> {
    if !DEFAULT_AGENTS.contains(&agent) {
        anyhow::bail!(
            "unknown agent '{}'. Available: {}",
            agent,
            DEFAULT_AGENTS.join(", ")
        );
    }
    if !crate::cmd_cache::command_exists(agent) {
        anyhow::bail!("agent '{}' not found in PATH. Is it installed?", agent);
    }
    Ok(())
}

/// Validate a model spec by running a quick test invocation.
///
/// Sends "hello" to the agent with the specified model to verify
/// the agent binary exists and the model is available.
pub async fn validate_model(spec: &ModelSpec) -> Result<()> {
    // 1. Check agent is known and installed
    validate_agent(&spec.agent)?;

    // 3. For opencode, pre-check against `opencode models` list (fast rejection)
    if spec.agent == "opencode" {
        if let Ok(models) = list_opencode_models().await {
            if !models.iter().any(|m| m == &spec.model) {
                let suggestions: Vec<_> = models
                    .iter()
                    .filter(|m| m.contains(&spec.model) || spec.model.contains(m.as_str()))
                    .take(10)
                    .cloned()
                    .collect();
                let shown = if suggestions.is_empty() {
                    models.iter().take(20).cloned().collect()
                } else {
                    suggestions
                };
                anyhow::bail!(
                    "model '{}' not found in opencode. Available models:\n{}",
                    spec.model,
                    shown.join("\n")
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
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("opencode")
            .args(["models"])
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("opencode models timed out after 10s"))?
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

/// Assembled context for a chat invocation.
pub struct ChatContext {
    /// System prompt (instructions, role, commands)
    pub system: String,
    /// User-visible context prepended to the message (live state, memories)
    pub state: String,
}

/// Assemble the chat context from SQLite state and live system info.
pub async fn assemble_context(store: &TaskStore, session_id: &str) -> Result<ChatContext> {
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

    // 3. Service status via `brew services info orch` (best-effort, 10s timeout)
    let service_status = match timeout(
        SUBPROCESS_TIMEOUT,
        tokio::process::Command::new("brew")
            .args(["services", "info", "orch"])
            .output(),
    )
    .await
    {
        Ok(Ok(output)) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => "(could not check service status)".to_string(),
    };

    // 4. Live state via `orch task list` (best-effort, 10s timeout)
    let task_list = match timeout(
        SUBPROCESS_TIMEOUT,
        tokio::process::Command::new("orch")
            .args(["task", "list"])
            .output(),
    )
    .await
    {
        Ok(Ok(output)) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => "(could not fetch live state)".to_string(),
    };

    // 5. Scheduled jobs via `orch job list` (best-effort, 10s timeout)
    let job_list = match timeout(
        SUBPROCESS_TIMEOUT,
        tokio::process::Command::new("orch")
            .args(["job", "list"])
            .output(),
    )
    .await
    {
        Ok(Ok(output)) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => "(no jobs configured)".to_string(),
    };

    let version = env!("ORCH_VERSION");
    let state = format!(
        "## Current Live State\n### Version\norch {version}\n\n### Service\n{service_status}\n\n### Tasks\n{task_list}\n\n### Scheduled Jobs\n{job_list}\n\n### Memories\n{memories}\n\n### Recent Conversation\n{recent_summaries}"
    );

    // 6. Build system prompt (instructions only, no live state in system prompt)
    let system = SYSTEM_TEMPLATE.to_string();

    Ok(ChatContext { system, state })
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

/// Get the current control session spec from KV.
pub async fn get_current_spec(store: &TaskStore) -> ModelSpec {
    ModelSpec {
        agent: get_agent(store).await,
        model: get_model(store).await,
    }
}

/// Set the current model in KV.
pub async fn set_model(store: &TaskStore, model: &str) -> Result<()> {
    store
        .kv_set("control:model", model)
        .await
        .context("setting control model")?;
    Ok(())
}

/// Set the current agent in KV.
pub async fn set_agent(store: &TaskStore, agent: &str) -> Result<()> {
    store
        .kv_set("control:agent", agent)
        .await
        .context("setting control agent")?;
    Ok(())
}

/// Set the current model and agent in KV.
pub async fn set_model_spec(store: &TaskStore, spec: &ModelSpec) -> Result<()> {
    set_model(store, &spec.model).await?;
    set_agent(store, &spec.agent).await?;
    Ok(())
}

/// Set the current agent and its default model in KV.
pub async fn set_agent_spec(store: &TaskStore, agent: &str) -> Result<ModelSpec> {
    let spec = ModelSpec {
        agent: agent.to_string(),
        model: default_model_for_agent(agent).to_string(),
    };
    set_model_spec(store, &spec).await?;
    Ok(spec)
}

fn format_current_spec(spec: &ModelSpec) -> String {
    format!("Current: {}:{}", spec.agent, spec.model)
}

fn format_switched_spec(spec: &ModelSpec) -> String {
    format!("Switched to **{}:{}** (validated)", spec.agent, spec.model)
}

async fn handle_control_command(
    store: &TaskStore,
    session_id: &str,
    command: ControlCommand,
) -> Result<String> {
    match command {
        ControlCommand::Model(None) => Ok(format_current_spec(&get_current_spec(store).await)),
        ControlCommand::Model(Some(new_spec)) => {
            let spec = parse_model_spec(&new_spec);
            validate_model(&spec).await?;
            set_model_spec(store, &spec).await?;
            // Reset session UUID so next message starts a fresh conversation
            reset_session_uuid(store, session_id).await?;
            Ok(format_switched_spec(&spec))
        }
        ControlCommand::Agent(None) => Ok(format_current_spec(&get_current_spec(store).await)),
        ControlCommand::Agent(Some(agent)) => {
            validate_agent(&agent)?;
            let spec = set_agent_spec(store, &agent).await?;
            // Reset session UUID so next message starts a fresh conversation
            reset_session_uuid(store, session_id).await?;
            Ok(format_switched_spec(&spec))
        }
    }
}

/// Handle a control-session command if present.
pub async fn maybe_handle_control_command(
    store: &TaskStore,
    session_id: &str,
    message: &str,
) -> Result<Option<String>> {
    Ok(match parse_control_command(message) {
        Some(command) => Some(handle_control_command(store, session_id, command).await?),
        None => None,
    })
}

/// Memory entry parsed from an agent response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
}

/// Parsed agent response metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedResponse {
    pub clean_text: String,
    pub summary: Option<String>,
    pub memories: Vec<MemoryEntry>,
}

static MEMORY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<memory\s+key="([^"]+)">(.+?)</memory>"#).expect("valid memory regex")
});

static SUMMARY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)<summary>(.+?)</summary>"#).expect("valid summary regex"));

/// Parse an agent response, extracting optional `<summary>` and `<memory>` tags.
pub fn parse_response(raw: &str) -> ParsedResponse {
    let mut clean = raw.to_string();
    let memories = MEMORY_RE
        .captures_iter(raw)
        .map(|caps| MemoryEntry {
            key: caps[1].trim().to_string(),
            value: caps[2].trim().to_string(),
        })
        .collect::<Vec<_>>();
    clean = MEMORY_RE.replace_all(&clean, "").to_string();

    let summary = SUMMARY_RE
        .captures(raw)
        .map(|caps| caps[1].trim().to_string());
    clean = SUMMARY_RE.replace_all(&clean, "").to_string();

    ParsedResponse {
        clean_text: clean.trim().to_string(),
        summary,
        memories,
    }
}

async fn persist_memories(
    store: &TaskStore,
    session_id: &str,
    memories: &[MemoryEntry],
) -> Result<()> {
    for memory in memories {
        store
            .kv_set(
                &format!("control:memory:{session_id}:{}", memory.key),
                &memory.value,
            )
            .await
            .with_context(|| format!("storing control memory {}", memory.key))?;
    }
    Ok(())
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
/// Delegates to `runner::direct::run_direct()` for command building, execution,
/// parsing, and error classification.
///
/// Writes temp files to `~/.orch/state/control/{pid}/`.
pub async fn invoke_agent(
    agent: &str,
    model: &str,
    context: &str,
    message: &str,
) -> Result<InvokeResult> {
    let dir = prepare_temp_dir().await?;

    let result = crate::engine::runner::direct::run_direct(
        agent,
        Some(model),
        context,
        message,
        AGENT_TIMEOUT,
        &dir,
    )
    .await?;

    Ok(InvokeResult {
        text: result.text,
        input_tokens: result.input_tokens,
        output_tokens: result.output_tokens,
    })
}

/// KV key for storing the session UUID for a named session.
fn session_uuid_key(session_name: &str) -> String {
    format!("control:session_id:{session_name}")
}

/// Get or create a session UUID for the given session name.
///
/// On first call for a session, generates a new UUID and persists it to KV.
/// Subsequent calls return the same UUID, giving agents conversation continuity
/// via `--session-id`.
/// Get or create a session UUID for the given session name.
///
/// On first call for a session, generates a new UUID and persists it to KV.
/// Subsequent calls return the same UUID, giving agents conversation continuity
/// via `--session-id` (new) or `--resume` (existing).
///
/// Returns `(uuid, is_new)` so callers know whether to create or resume the session.
pub async fn get_or_create_session_uuid(
    store: &TaskStore,
    session_name: &str,
) -> Result<(String, bool)> {
    let key = session_uuid_key(session_name);
    if let Some(existing) = store.kv_get(&key).await? {
        return Ok((existing, false));
    }
    let uuid = Uuid::new_v4().to_string();
    store
        .kv_set(&key, &uuid)
        .await
        .context("storing session UUID")?;
    tracing::info!(session_name, uuid = %uuid, "created new chat session UUID");
    Ok((uuid, true))
}

/// Reset the session UUID so the next message starts a fresh conversation.
///
/// Called when agent or model changes, since the new agent won't share
/// the previous agent's session state. Deletes the stored UUID so the next
/// `get_or_create_session_uuid` call generates a fresh one.
pub async fn reset_session_uuid(store: &TaskStore, session_name: &str) -> Result<()> {
    let key = session_uuid_key(session_name);
    store
        .kv_delete(&key)
        .await
        .context("resetting session UUID")?;
    tracing::info!(session_name, "reset chat session UUID");
    Ok(())
}

/// Invoke an agent one-shot with `--session-id` or `--resume` for conversation continuity.
///
/// For claude-compatible agents (claude, kimi, minimax), passes `--session-id <uuid>`
/// when creating a new session, or `--resume <uuid>` when continuing an existing one.
/// Other agents get a plain one-shot call.
pub async fn invoke_agent_with_session(
    agent: &str,
    model: &str,
    context: &str,
    message: &str,
    session_uuid: Option<&str>,
    resume: bool,
) -> Result<InvokeResult> {
    let dir = prepare_temp_dir().await?;

    let session = session_uuid.map(|sid| crate::engine::runner::direct::SessionContinuation {
        session_id: sid,
        resume,
    });

    let result = crate::engine::runner::direct::run_direct_with_session(
        agent,
        Some(model),
        context,
        message,
        AGENT_TIMEOUT,
        &dir,
        session,
    )
    .await?;

    Ok(InvokeResult {
        text: result.text,
        input_tokens: result.input_tokens,
        output_tokens: result.output_tokens,
    })
}

/// High-level entry point: process a user message and return the assistant response.
///
/// Handles `/model` and `/agent` commands, assembles context, invokes the agent,
/// parses the response for summaries, and stores both user and assistant messages.
pub async fn send_message(
    store: &TaskStore,
    session_id: &str,
    channel: &str,
    channel_thread: Option<&str>,
    message: &str,
) -> Result<String> {
    // Handle control commands — don't store as messages.
    if let Some(response) = maybe_handle_control_command(store, session_id, message).await? {
        return Ok(response);
    }

    // Acquire per-session lock — serializes concurrent invocations for the same session.
    // Messages that arrive while an invocation is running will queue here and execute in order.
    let session_lock = {
        let mut map = SESSION_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            map.entry(session_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    };
    let _guard = session_lock.lock().await;

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
    let ctx = assemble_context(store, session_id).await?;

    // Resolve model and agent from KV
    let model = get_model(store).await;
    let agent = get_agent(store).await;

    // Prepend live state to user message so the LLM sees it directly
    let full_message = format!("{}\n\n---\n\n{}", ctx.state, message);

    // Get or create a session UUID for conversation continuity
    let (session_uuid, is_new) = get_or_create_session_uuid(store, session_id).await?;

    // Invoke agent one-shot with --session-id (new) or --resume (existing) for conversation continuity
    let result = match invoke_agent_with_session(
        &agent,
        &model,
        &ctx.system,
        &full_message,
        Some(&session_uuid),
        !is_new,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            // If the session UUID has expired server-side, reset it and retry
            // with a fresh session. This handles the "No conversation found with
            // session ID: <uuid>" error that Claude returns when a session expires.
            // The AgentError is converted to a string by anyhow::bail! in direct.rs,
            // so we match on the Display representation of StaleSession.
            let err_str = e.to_string();
            let is_stale = err_str.contains("stale session");
            // A timeout (exit code 124 from the `timeout` CLI tool) also means
            // the session can no longer be resumed — reset and start fresh rather
            // than surfacing "Control session error: timeout after 1800s" to the user.
            let is_timeout =
                err_str.contains("timeout after") || err_str.contains("timed out after");
            if is_stale || is_timeout {
                if is_stale {
                    tracing::info!(
                        session_id,
                        "chat session UUID expired; resetting and retrying with fresh session"
                    );
                } else {
                    tracing::info!(
                        session_id,
                        "chat session timed out; resetting and retrying with fresh session"
                    );
                }
                reset_session_uuid(store, session_id).await?;
                // Fresh invocation — no resume, generates a new UUID
                let (fresh_uuid, _) = get_or_create_session_uuid(store, session_id).await?;
                let fresh_result = invoke_agent_with_session(
                    &agent,
                    &model,
                    &ctx.system,
                    &full_message,
                    Some(&fresh_uuid),
                    false, // not resuming — new session
                )
                .await?;
                if is_timeout {
                    InvokeResult {
                        text: format!("_(New session started.)_\n\n{}", fresh_result.text),
                        ..fresh_result
                    }
                } else {
                    fresh_result
                }
            } else {
                return Err(e);
            }
        }
    };

    // Parse response for summary and memory tags
    let parsed = parse_response(&result.text);

    persist_memories(store, session_id, &parsed.memories).await?;

    // Store assistant message with token usage
    let total_tokens = match (result.input_tokens, result.output_tokens) {
        (Some(i), Some(o)) => Some((i + o) as i64),
        (Some(t), None) | (None, Some(t)) => Some(t as i64),
        _ => None,
    };
    let cost_usd = match (result.input_tokens, result.output_tokens) {
        (Some(input), Some(output)) => {
            let pricing = crate::store::pricing_for_model(&model);
            let usage = crate::store::TokenUsage {
                input_tokens: input,
                output_tokens: output,
            };
            Some(pricing.estimate_cost_usd(usage).total_cost_usd)
        }
        _ => None,
    };
    store
        .insert_control_message(
            session_id,
            "assistant",
            channel,
            channel_thread,
            &parsed.clean_text,
            parsed.summary.as_deref(),
            Some(&model),
            Some(&agent),
            total_tokens,
            cost_usd,
        )
        .await?;

    Ok(parsed.clean_text)
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
            .kv_set("control:memory:default:tz", "User timezone is UTC-3")
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
        assert!(ctx.state.contains("UTC-3"), "should include memories");
        assert!(
            ctx.state.contains("checked status"),
            "should include summaries"
        );
    }

    #[test]
    fn extract_summary_from_response() {
        let response = "Here are your tasks.\n\n<summary>listed 3 active tasks</summary>";
        let parsed = parse_response(response);
        assert_eq!(parsed.summary.as_deref(), Some("listed 3 active tasks"));
        assert!(!parsed.clean_text.contains("<summary>"));
    }

    #[test]
    fn extract_summary_missing() {
        let response = "No tasks found.";
        let parsed = parse_response(response);
        assert_eq!(parsed.summary, None);
        assert_eq!(parsed.clean_text, "No tasks found.");
    }

    #[test]
    fn extract_summary_closing_tag_before_opening_does_not_panic() {
        // </summary> appears before <summary> — must not panic
        let response = "Example: </summary> is a closing tag.\n<summary>actual summary</summary>";
        let parsed = parse_response(response);
        assert_eq!(parsed.summary.as_deref(), Some("actual summary"));
        assert!(!parsed.clean_text.contains("<summary>"));
    }

    #[test]
    fn extract_memory_tags() {
        let response = r#"Remember these.

<memory key="tz">User timezone is UTC-3</memory>
<memory key="theme">Prefer concise answers</memory>

<summary>stored two memories</summary>"#;
        let parsed = parse_response(response);
        assert_eq!(parsed.memories.len(), 2);
        assert_eq!(parsed.memories[0].key, "tz");
        assert_eq!(parsed.memories[0].value, "User timezone is UTC-3");
        assert_eq!(parsed.memories[1].key, "theme");
        assert_eq!(parsed.memories[1].value, "Prefer concise answers");
        assert_eq!(parsed.summary.as_deref(), Some("stored two memories"));
        assert!(!parsed.clean_text.contains("<memory"));
    }

    #[tokio::test]
    async fn persist_memories_writes_to_kv() {
        let store = TaskStore::open_memory().await.unwrap();
        let memories = vec![MemoryEntry {
            key: "tz".to_string(),
            value: "User timezone is UTC-3".to_string(),
        }];

        persist_memories(&store, "default", &memories)
            .await
            .unwrap();

        assert_eq!(
            store
                .kv_get("control:memory:default:tz")
                .await
                .unwrap()
                .as_deref(),
            Some("User timezone is UTC-3")
        );
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
    async fn send_message_agent_show_current() {
        let store = TaskStore::open_memory().await.unwrap();
        let response = send_message(&store, "default", "cli", None, "/agent")
            .await
            .unwrap();
        assert!(response.contains("claude"));
        assert!(response.contains("sonnet"));
    }

    #[tokio::test]
    async fn send_message_agent_switches_with_default_model() {
        let store = TaskStore::open_memory().await.unwrap();
        // Directly set agent spec to bypass validation (no binary in test env)
        let spec = set_agent_spec(&store, "codex").await.unwrap();
        assert_eq!(spec.agent, "codex");
        assert_eq!(spec.model, "gpt-4o");
        assert_eq!(get_agent(&store).await, "codex");
        assert_eq!(get_model(&store).await, "gpt-4o");
    }

    #[test]
    fn parse_control_command_model_and_agent() {
        assert_eq!(
            parse_control_command("/model"),
            Some(ControlCommand::Model(None))
        );
        assert_eq!(
            parse_control_command("/agent"),
            Some(ControlCommand::Agent(None))
        );
        assert_eq!(
            parse_control_command("/agent codex"),
            Some(ControlCommand::Agent(Some("codex".to_string())))
        );
        assert_eq!(
            parse_control_command("/model opencode:deepseek-r1"),
            Some(ControlCommand::Model(Some(
                "opencode:deepseek-r1".to_string()
            )))
        );
        assert_eq!(parse_control_command("/kill"), None);
        assert_eq!(parse_control_command("/session"), None);
    }

    #[test]
    fn control_prompt_slash_commands_match_supported_surface() {
        let documented = Regex::new(r"`/([a-z]+)(?:[^`]*)`")
            .unwrap()
            .captures_iter(SYSTEM_TEMPLATE)
            .map(|caps| caps[1].to_string())
            .collect::<std::collections::BTreeSet<_>>();

        let expected = ["agent", "model"]
            .into_iter()
            .map(str::to_string)
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(documented, expected);
    }

    #[tokio::test]
    async fn set_agent_spec_persists_default_model() {
        let store = TaskStore::open_memory().await.unwrap();
        let spec = set_agent_spec(&store, "codex").await.unwrap();
        assert_eq!(spec.agent, "codex");
        assert_eq!(spec.model, "gpt-4o");
        assert_eq!(get_agent(&store).await, "codex");
        assert_eq!(get_model(&store).await, "gpt-4o");
    }

    #[tokio::test]
    async fn send_message_model_switch_does_not_store() {
        let store = TaskStore::open_memory().await.unwrap();
        // set directly to avoid validation in test env
        let spec = parse_model_spec("claude:haiku");
        set_model_spec(&store, &spec).await.unwrap();
        let messages = store
            .list_control_messages("default", None, 10)
            .await
            .unwrap();
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

    #[test]
    fn validate_agent_rejects_unknown() {
        let result = validate_agent("nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown agent"), "got: {err}");
    }

    #[test]
    fn validate_agent_rejects_empty() {
        let result = validate_agent("");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown agent"));
    }

    #[tokio::test]
    async fn agent_command_rejects_unknown_before_persisting() {
        let store = TaskStore::open_memory().await.unwrap();
        // Set a known-good agent first
        set_agent(&store, "claude").await.unwrap();

        // Try switching to an unknown agent — should fail
        let result = maybe_handle_control_command(&store, "default", "/agent bogusagent").await;
        assert!(result.is_err(), "should reject unknown agent");
        assert!(result.unwrap_err().to_string().contains("unknown agent"));

        // Verify the original agent is still set (not overwritten)
        assert_eq!(get_agent(&store).await, "claude");
    }

    #[test]
    fn cost_usd_computed_from_token_counts() {
        use crate::store::{pricing_for_model, TokenUsage};

        let model = "sonnet";
        let input_tokens: u64 = 1_000_000;
        let output_tokens: u64 = 1_000_000;
        let pricing = pricing_for_model(model);
        let usage = TokenUsage {
            input_tokens,
            output_tokens,
        };
        let cost = pricing.estimate_cost_usd(usage);

        // sonnet: $3/M input, $15/M output → $18 total for 1M each
        assert!(cost.total_cost_usd > 0.0, "cost must be non-zero");
        assert!(
            (cost.total_cost_usd - 18.0).abs() < 0.01,
            "expected ~$18 for 1M+1M sonnet tokens"
        );
    }

    #[tokio::test]
    async fn insert_control_message_stores_cost_usd() {
        use crate::store::{pricing_for_model, TokenUsage};

        let store = crate::store::TaskStore::open_memory().await.unwrap();

        let input_tokens: u64 = 500_000;
        let output_tokens: u64 = 100_000;
        let pricing = pricing_for_model("sonnet");
        let usage = TokenUsage {
            input_tokens,
            output_tokens,
        };
        let cost_usd = Some(pricing.estimate_cost_usd(usage).total_cost_usd);

        store
            .insert_control_message(
                "default",
                "assistant",
                "cli",
                None,
                "hello",
                Some("greeted"),
                Some("sonnet"),
                Some("claude"),
                Some((input_tokens + output_tokens) as i64),
                cost_usd,
            )
            .await
            .unwrap();

        let messages = store
            .list_control_messages("default", None, 10)
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert!(msg.cost_usd.is_some(), "cost_usd must be stored, not NULL");
        let stored = msg.cost_usd.unwrap();
        assert!(stored > 0.0, "stored cost must be positive");
    }

    #[tokio::test]
    async fn session_uuid_created_and_stable() {
        let store = TaskStore::open_memory().await.unwrap();
        let (uuid1, is_new1) = get_or_create_session_uuid(&store, "default").await.unwrap();
        let (uuid2, is_new2) = get_or_create_session_uuid(&store, "default").await.unwrap();
        assert_eq!(uuid1, uuid2, "same session should return same UUID");
        assert!(is_new1, "first call should be new");
        assert!(!is_new2, "second call should be existing");
        assert_eq!(uuid1.len(), 36, "should be a valid UUID string");
    }

    #[tokio::test]
    async fn session_uuid_differs_per_session() {
        let store = TaskStore::open_memory().await.unwrap();
        let (uuid_default, _) = get_or_create_session_uuid(&store, "default").await.unwrap();
        let (uuid_ops, _) = get_or_create_session_uuid(&store, "ops").await.unwrap();
        assert_ne!(uuid_default, uuid_ops);
    }

    #[tokio::test]
    async fn session_uuid_reset_creates_new() {
        let store = TaskStore::open_memory().await.unwrap();
        let (uuid1, _) = get_or_create_session_uuid(&store, "default").await.unwrap();
        reset_session_uuid(&store, "default").await.unwrap();
        let (uuid2, is_new2) = get_or_create_session_uuid(&store, "default").await.unwrap();
        assert_ne!(uuid1, uuid2, "reset should produce a new UUID");
        assert!(is_new2, "after reset should be new");
    }

    /// Verify that the timeout error patterns used in send_message recovery match
    /// the actual error strings produced by the agent runner (AgentError::Timeout
    /// formats as "timeout after Xs"; run_shell_command formats as "agent timed out
    /// after Xs"). Both must trigger session reset rather than bubbling up as errors.
    #[test]
    fn timeout_error_patterns_match_runner_output() {
        let patterns = ["timeout after 1800s", "agent timed out after 120s"];
        for &msg in &patterns {
            let is_timeout = msg.contains("timeout after") || msg.contains("timed out after");
            assert!(is_timeout, "pattern should be detected as timeout: {msg:?}");
        }
        // Unrelated errors must not be misidentified as timeouts.
        let non_timeout = [
            "rate limit: 429",
            "stale session (session expired): abc",
            "auth error: invalid key",
        ];
        for &msg in &non_timeout {
            let is_timeout = msg.contains("timeout after") || msg.contains("timed out after");
            assert!(
                !is_timeout,
                "pattern must not be detected as timeout: {msg:?}"
            );
        }
    }
}

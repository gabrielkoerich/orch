//! Control session — context assembly, one-shot agent invocation, response parsing.
//!
//! The control session provides an interactive ops assistant that assembles context
//! from SQLite (memories, summaries, live state), invokes an agent CLI one-shot,
//! and parses the response for summary extraction.

use anyhow::{Context, Result};
use sqlx::Row;

use crate::store::TaskStore;

/// System prompt template, loaded at compile time.
const SYSTEM_TEMPLATE: &str = include_str!("../prompts/control_system.md");

/// Map a model name to the agent CLI that should execute it.
pub fn agent_for_model(model: &str) -> &'static str {
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
        "claude"
    }
}

/// Assemble the full system-prompt context from SQLite state and live system info.
pub async fn assemble_context(store: &TaskStore) -> Result<String> {
    // 1. Gather memories from KV (keys matching control:memory:*)
    let memory_rows = sqlx::query("SELECT key, value FROM kv WHERE key LIKE 'control:memory:%'")
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
    let summaries = store.control_recent_summaries(20).await.unwrap_or_default();
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

/// Get the current model from KV, defaulting to `"sonnet"`.
pub async fn get_model(store: &TaskStore) -> String {
    store
        .kv_get("control:model")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "sonnet".to_string())
}

/// Set the current model in KV.
pub async fn set_model(store: &TaskStore, model: &str) -> Result<()> {
    store
        .kv_set("control:model", model)
        .await
        .context("setting control model")
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

/// Invoke an agent CLI one-shot and return its stdout.
pub async fn invoke_agent(
    agent: &str,
    model: &str,
    context: &str,
    message: &str,
) -> Result<String> {
    let dir = "/tmp/orch-control";
    tokio::fs::create_dir_all(dir).await?;

    let sys_file = format!("{dir}/system.md");
    let msg_file = format!("{dir}/message.txt");
    let combined_file = format!("{dir}/combined.txt");

    match agent {
        "claude" => {
            tokio::fs::write(&sys_file, context).await?;
            tokio::fs::write(&msg_file, message).await?;

            let output = tokio::process::Command::new("claude")
                .args([
                    "-p",
                    "--model",
                    model,
                    "--permission-mode",
                    "bypassPermissions",
                    "--output-format",
                    "text",
                    "--append-system-prompt",
                    &sys_file,
                ])
                .stdin(std::process::Stdio::from(
                    std::fs::File::open(&msg_file)
                        .context("opening message file for claude stdin")?,
                ))
                .output()
                .await
                .context("spawning claude")?;

            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
        "codex" => {
            let combined = format!("{context}\n\n---\n\nUser message:\n{message}");
            tokio::fs::write(&combined_file, &combined).await?;

            let output = tokio::process::Command::new("codex")
                .args(["--model", model, "--full-auto", "-q"])
                .stdin(std::process::Stdio::from(
                    std::fs::File::open(&combined_file)
                        .context("opening combined file for codex stdin")?,
                ))
                .output()
                .await
                .context("spawning codex")?;

            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
        "opencode" => {
            let combined = format!("{context}\n\n---\n\nUser message:\n{message}");
            tokio::fs::write(&combined_file, &combined).await?;

            let output = tokio::process::Command::new("opencode")
                .args(["run", "--format", "text", "-m", model, "-"])
                .stdin(std::process::Stdio::from(
                    std::fs::File::open(&combined_file)
                        .context("opening combined file for opencode stdin")?,
                ))
                .output()
                .await
                .context("spawning opencode")?;

            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
        other => anyhow::bail!("unknown agent: {other}"),
    }
}

/// High-level entry point: process a user message and return the assistant response.
///
/// Handles `/model` commands, assembles context, invokes the agent, parses the
/// response for summaries, and stores both user and assistant messages.
pub async fn send_message(
    store: &TaskStore,
    channel: &str,
    channel_thread: Option<&str>,
    message: &str,
) -> Result<String> {
    // Handle /model command — don't store as a message
    if let Some(new_model) = message.strip_prefix("/model ") {
        let new_model = new_model.trim();
        set_model(store, new_model).await?;
        let agent = agent_for_model(new_model);
        return Ok(format!(
            "Model switched to **{new_model}** (agent: {agent})"
        ));
    }

    // Store user message
    store
        .insert_control_message(
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
    let context = assemble_context(store).await?;

    // Resolve model and agent
    let model = get_model(store).await;
    let agent = agent_for_model(&model);

    // Invoke agent
    let raw_response = invoke_agent(agent, &model, &context, message).await?;

    // Parse response
    let (clean, summary) = parse_response(&raw_response);

    // Store assistant message
    store
        .insert_control_message(
            "assistant",
            channel,
            channel_thread,
            &clean,
            summary.as_deref(),
            Some(&model),
            Some(agent),
            None,
            None,
        )
        .await?;

    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TaskStore;

    #[tokio::test]
    async fn assemble_context_includes_memories_and_summaries() {
        let store = TaskStore::open_memory().await.unwrap();
        store
            .kv_set("control:memory:tz", "User is in BRT timezone")
            .await
            .unwrap();
        store
            .insert_control_message(
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

        let ctx = assemble_context(&store).await.unwrap();
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
        assert_eq!(agent_for_model("sonnet"), "claude");
        assert_eq!(agent_for_model("opus"), "claude");
        assert_eq!(agent_for_model("haiku"), "claude");
        assert_eq!(agent_for_model("gpt-4o"), "codex");
        assert_eq!(agent_for_model("o3"), "codex");
        assert_eq!(agent_for_model("deepseek-r1"), "opencode");
    }

    #[tokio::test]
    async fn send_message_model_switch() {
        let store = TaskStore::open_memory().await.unwrap();
        let model = get_model(&store).await;
        assert_eq!(model, "sonnet");

        let response = send_message(&store, "cli", None, "/model opus")
            .await
            .unwrap();
        assert!(response.contains("opus"));
        assert_eq!(get_model(&store).await, "opus");
    }

    #[tokio::test]
    async fn send_message_model_switch_does_not_store() {
        let store = TaskStore::open_memory().await.unwrap();
        let _ = send_message(&store, "cli", None, "/model haiku")
            .await
            .unwrap();
        let messages = store.list_control_messages(10, 0).await.unwrap();
        assert_eq!(messages.len(), 0);
    }
}

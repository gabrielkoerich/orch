//! Persistent chat session — reuse a long-lived agent process in tmux.
//!
//! Instead of spawning a new agent process per message (10-20s cold start),
//! this module keeps an interactive agent session alive in tmux and sends
//! subsequent messages via `tmux send-keys` + pane capture diffing.
//!
//! ## Session lifecycle
//!
//! 1. First message → spawn agent in `orch-chat-{session_id}` tmux session
//! 2. Subsequent messages → `send-keys` to existing session
//! 3. Response captured via pane output diff (before/after)
//! 4. Session killed after idle timeout (default 10 min)
//!
//! ## Agent support
//!
//! - **claude**: Interactive mode (no `-p`), `--append-system-prompt`
//! - **codex**: Interactive mode, system prompt injected as first message
//! - **opencode**: Interactive mode, system prompt injected as first message

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::OnceCell;

use crate::tmux::TmuxManager;

/// Default idle timeout before killing a persistent session.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(600); // 10 min

/// How often to poll for response completion.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long output must be stable before we consider the response complete.
const STABLE_THRESHOLD: Duration = Duration::from_secs(3);

/// Maximum time to wait for a response before giving up.
const MAX_RESPONSE_WAIT: Duration = Duration::from_secs(300); // 5 min

/// Minimum time to wait before checking for completion (let agent start).
const MIN_RESPONSE_TIME: Duration = Duration::from_secs(1);

/// Internal state for a persistent chat session.
struct SessionState {
    tmux_session: String,
    agent: String,
    model: String,
    last_activity: Instant,
    idle_timeout: Duration,
}

type SessionHandle = std::sync::Arc<tokio::sync::Mutex<SessionState>>;
type SessionCell = std::sync::Arc<OnceCell<SessionHandle>>;

/// Global registry of persistent chat sessions.
///
/// Keyed by `session_id`. Each entry holds a single-flight `OnceCell` that
/// resolves to a `SessionHandle` so concurrent callers await the same creation.
static SESSIONS: std::sync::LazyLock<Mutex<HashMap<String, SessionCell>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Result of sending a message to a persistent session.
pub struct SessionResponse {
    pub text: String,
    /// Token counts unavailable in interactive mode.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Send a message via a persistent tmux session, creating it if needed.
///
/// This is the main entry point. It:
/// 1. Gets or creates a tmux session for the given `session_id`
/// 2. Sends the message via `tmux send-keys`
/// 3. Captures the response via pane output diffing
/// 4. Returns the extracted response text
pub async fn send_persistent(
    session_id: &str,
    agent: &str,
    model: &str,
    system_prompt_file: &str,
    message: &str,
) -> Result<SessionResponse> {
    let state = get_or_create_session(session_id, agent, model, system_prompt_file).await?;
    let mut guard = state.lock().await;

    // Check if agent/model changed — need to restart session
    if guard.agent != agent || guard.model != model {
        tracing::info!(
            session_id,
            old_agent = %guard.agent,
            new_agent = agent,
            "agent/model changed — restarting session"
        );
        let tmux = TmuxManager::new();
        let _ = tmux.kill_session(&guard.tmux_session).await;
        drop(guard);
        // Remove from registry and recurse
        {
            let mut map = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
            map.remove(session_id);
        }
        return Box::pin(send_persistent(
            session_id,
            agent,
            model,
            system_prompt_file,
            message,
        ))
        .await;
    }

    // Check idle timeout
    if guard.last_activity.elapsed() > guard.idle_timeout {
        tracing::info!(
            session_id,
            elapsed = ?guard.last_activity.elapsed(),
            "session idle — restarting"
        );
        let tmux = TmuxManager::new();
        let _ = tmux.kill_session(&guard.tmux_session).await;
        drop(guard);
        {
            let mut map = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
            map.remove(session_id);
        }
        return Box::pin(send_persistent(
            session_id,
            agent,
            model,
            system_prompt_file,
            message,
        ))
        .await;
    }

    let tmux = TmuxManager::new();

    // Verify session is still alive
    if !tmux.session_exists(&guard.tmux_session).await {
        tracing::warn!(session_id, "tmux session disappeared — recreating");
        drop(guard);
        {
            let mut map = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
            map.remove(session_id);
        }
        return Box::pin(send_persistent(
            session_id,
            agent,
            model,
            system_prompt_file,
            message,
        ))
        .await;
    }

    // Capture pane content before sending
    let before = tmux
        .capture_pane(&guard.tmux_session, 5000)
        .await
        .unwrap_or_default();
    let before_len = before.len();

    // Send message via tmux
    send_message_to_tmux(&guard.tmux_session, message).await?;

    // Wait for response and capture it
    let response = wait_and_capture_response(&tmux, &guard.tmux_session, before_len, &guard.agent)
        .await
        .context("waiting for agent response")?;

    guard.last_activity = Instant::now();

    let clean = strip_ansi_codes(&response);

    Ok(SessionResponse {
        text: clean,
        input_tokens: None,
        output_tokens: None,
    })
}

/// Kill a persistent session if it exists.
pub async fn kill_session(session_id: &str) -> Result<bool> {
    let state = {
        let mut map = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(session_id)
    };

    if let Some(state) = state {
        if let Some(state) = state.get() {
            let guard = state.lock().await;
            let tmux = TmuxManager::new();
            tmux.kill_session(&guard.tmux_session).await?;
            Ok(true)
        } else {
            // No initialized state yet; fall back to killing by convention name.
            let session_name = tmux_session_name(session_id);
            let tmux = TmuxManager::new();
            if tmux.session_exists(&session_name).await {
                tmux.kill_session(&session_name).await?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
    } else {
        // Try to kill by convention name even if not in registry
        let session_name = tmux_session_name(session_id);
        let tmux = TmuxManager::new();
        if tmux.session_exists(&session_name).await {
            tmux.kill_session(&session_name).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Check if a persistent session exists and is active.
#[allow(dead_code)]
pub async fn session_active(session_id: &str) -> bool {
    let tmux = TmuxManager::new();
    let name = tmux_session_name(session_id);
    tmux.session_exists(&name).await && tmux.is_session_active(&name).await
}

/// Get session info for display.
pub async fn session_info(session_id: &str) -> Option<String> {
    let state = {
        let map = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        map.get(session_id).cloned()
    };

    if let Some(state) = state.and_then(|cell| cell.get().cloned()) {
        let guard = state.lock().await;
        let active = {
            let tmux = TmuxManager::new();
            tmux.is_session_active(&guard.tmux_session).await
        };
        let idle = guard.last_activity.elapsed();
        Some(format!(
            "session={} agent={}:{} idle={:.0}s active={}",
            guard.tmux_session,
            guard.agent,
            guard.model,
            idle.as_secs_f64(),
            active,
        ))
    } else {
        None
    }
}

// ── Internal helpers ──────────────────────────────────────────────────

fn tmux_session_name(session_id: &str) -> String {
    format!("orch-chat-{session_id}")
}

/// Get or create a persistent session, returning a shared lock.
async fn get_or_create_session(
    session_id: &str,
    agent: &str,
    model: &str,
    system_prompt_file: &str,
) -> Result<SessionHandle> {
    let cell = {
        let mut map = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(session_id.to_string())
            .or_insert_with(|| std::sync::Arc::new(OnceCell::new()))
            .clone()
    };

    let state = cell
        .get_or_try_init(|| async move {
            // Create new session
            let tmux = TmuxManager::new();
            let session_name = tmux_session_name(session_id);

            // Kill any stale session with the same name
            if tmux.session_exists(&session_name).await {
                let _ = tmux.kill_session(&session_name).await;
            }

            let command = build_interactive_command(agent, model, system_prompt_file);
            tracing::info!(
                session_id,
                agent,
                model,
                %session_name,
                "creating persistent chat session"
            );

            // Create the tmux session directly (not via TmuxManager.create_session
            // which uses project/task_id naming)
            let output = Command::new("tmux")
                .args([
                    "new-session",
                    "-d",
                    "-s",
                    &session_name,
                    "-c",
                    "/tmp",
                    &command,
                ])
                .output()
                .await
                .context("spawning tmux session for chat")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("tmux new-session failed for chat: {stderr}");
            }

            // Wait for the agent to be ready
            wait_for_agent_ready(&tmux, &session_name, agent).await?;

            // For non-Claude agents, inject the system prompt as the first message.
            // Claude gets it via --append-system-prompt flag, but codex and opencode
            // don't have an equivalent flag for interactive mode.
            if !matches!(agent, "claude" | "kimi" | "minimax") && !system_prompt_file.is_empty() {
                if let Ok(prompt_content) = tokio::fs::read_to_string(system_prompt_file).await {
                    if !prompt_content.trim().is_empty() {
                        let injected = format!(
                            "SYSTEM INSTRUCTIONS — follow these for the entire session:\n\n{}",
                            prompt_content.trim()
                        );
                        tracing::debug!(
                            session_id,
                            agent,
                            "injecting system prompt as first message"
                        );
                        send_message_to_tmux(&session_name, &injected).await?;
                        // Wait for the agent to process and return to ready state
                        wait_for_agent_ready(&tmux, &session_name, agent).await?;
                    }
                } else {
                    tracing::warn!(
                        session_id,
                        system_prompt_file,
                        "failed to read system prompt file for injection"
                    );
                }
            }

            Ok(std::sync::Arc::new(tokio::sync::Mutex::new(SessionState {
                tmux_session: session_name,
                agent: agent.to_string(),
                model: model.to_string(),
                last_activity: Instant::now(),
                idle_timeout: DEFAULT_IDLE_TIMEOUT,
            })))
        })
        .await?;

    Ok(std::sync::Arc::clone(state))
}

/// Build the interactive command for an agent.
fn build_interactive_command(agent: &str, model: &str, system_prompt_file: &str) -> String {
    match agent {
        "claude" | "kimi" | "minimax" => {
            // Claude Code interactive mode with system prompt
            format!(
                "{agent} --model {model} --permission-mode bypassPermissions --append-system-prompt {system_prompt_file}"
            )
        }
        "codex" => {
            // Codex interactive mode
            format!("codex --model {model} --full-auto")
        }
        "opencode" => {
            // OpenCode interactive mode
            format!("opencode --model {model}")
        }
        _ => {
            // Fallback: try running the agent binary directly
            format!("{agent} --model {model}")
        }
    }
}

/// Wait for the agent to be ready (prompt appears in pane output).
async fn wait_for_agent_ready(tmux: &TmuxManager, session: &str, agent: &str) -> Result<()> {
    let start = Instant::now();
    let max_wait = Duration::from_secs(30);

    loop {
        if start.elapsed() > max_wait {
            tracing::warn!(session, agent, "agent ready timeout — proceeding anyway");
            return Ok(());
        }

        let content = tmux.capture_pane(session, 100).await.unwrap_or_default();

        if is_agent_prompt(&content, agent) {
            tracing::debug!(session, agent, "agent ready");
            return Ok(());
        }

        // Also check if the session died during startup
        if !tmux.is_session_active(session).await {
            anyhow::bail!("agent session died during startup");
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Send a message to a tmux session via load-buffer + paste-buffer.
///
/// This approach handles arbitrary content including special characters
/// and multi-line text, avoiding shell escaping issues with send-keys.
async fn send_message_to_tmux(session: &str, message: &str) -> Result<()> {
    // Write message to a unique temp file (timestamp + pid to avoid collisions
    // between concurrent sessions)
    let tmp_dir = std::env::temp_dir();
    let unique_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let msg_file = tmp_dir.join(format!(
        "orch-chat-msg-{}-{}",
        std::process::id(),
        unique_id
    ));
    tokio::fs::write(&msg_file, message).await?;

    // Use a unique named buffer to avoid races between concurrent sessions
    let buffer_name = format!("orch-{}-{}", std::process::id(), unique_id);

    // Load into a named tmux paste buffer
    let output = Command::new("tmux")
        .args([
            "load-buffer",
            "-b",
            &buffer_name,
            msg_file.to_str().unwrap_or("/tmp/orch-msg"),
        ])
        .output()
        .await
        .context("tmux load-buffer")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = tokio::fs::remove_file(&msg_file).await;
        anyhow::bail!("tmux load-buffer failed: {stderr}");
    }

    // Paste the named buffer into the session
    let output = Command::new("tmux")
        .args(["paste-buffer", "-b", &buffer_name, "-t", session])
        .output()
        .await
        .context("tmux paste-buffer")?;

    // Delete the named buffer regardless of paste result
    let _ = Command::new("tmux")
        .args(["delete-buffer", "-b", &buffer_name])
        .output()
        .await;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = tokio::fs::remove_file(&msg_file).await;
        anyhow::bail!("tmux paste-buffer failed: {stderr}");
    }

    // Press Enter to submit
    let output = Command::new("tmux")
        .args(["send-keys", "-t", session, "Enter"])
        .output()
        .await
        .context("tmux send-keys Enter")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("tmux send-keys Enter failed: {stderr}");
    }

    // Clean up temp file
    let _ = tokio::fs::remove_file(&msg_file).await;

    Ok(())
}

/// Wait for the agent response to complete by polling pane output.
///
/// Detects completion via output stabilization: polls every 500ms,
/// considers the response done when output hasn't changed for 3+ seconds
/// after initial activity.
async fn wait_and_capture_response(
    tmux: &TmuxManager,
    session: &str,
    before_len: usize,
    agent: &str,
) -> Result<String> {
    let start = Instant::now();
    let mut last_content = String::new();
    let mut last_change = Instant::now();
    let mut has_new_content = false;

    loop {
        if start.elapsed() > MAX_RESPONSE_WAIT {
            anyhow::bail!("response timeout after {:?}", MAX_RESPONSE_WAIT);
        }

        tokio::time::sleep(POLL_INTERVAL).await;

        let current = tmux.capture_pane(session, 5000).await.unwrap_or_default();

        // Check if session died
        if !tmux.is_session_active(session).await && current == last_content {
            // Session ended — return whatever we have
            return extract_response(&current, before_len, agent);
        }

        // Check for new content since the message was sent
        if current.len() > before_len {
            has_new_content = true;
        }

        // Track content changes
        if current != last_content {
            last_change = Instant::now();
            last_content = current;
            continue;
        }

        // Check stabilization: content hasn't changed for STABLE_THRESHOLD
        // and we have new content and enough time has elapsed
        if has_new_content
            && start.elapsed() > MIN_RESPONSE_TIME
            && last_change.elapsed() > STABLE_THRESHOLD
        {
            // Also verify the agent prompt has reappeared (stronger signal)
            if is_agent_prompt(&last_content, agent) || last_change.elapsed() > STABLE_THRESHOLD * 2
            {
                return extract_response(&last_content, before_len, agent);
            }
        }
    }
}

/// Extract the response text from pane content by diffing with before state.
fn extract_response(content: &str, before_len: usize, agent: &str) -> Result<String> {
    // Get the new content that appeared after the message was sent
    let new_content = if content.len() > before_len {
        &content[before_len..]
    } else {
        content
    };

    // Split into lines and clean up
    let lines: Vec<&str> = new_content.lines().collect();

    // Remove the echoed input (first non-empty lines that look like user input)
    // and the trailing prompt
    let mut start_idx = 0;
    let mut end_idx = lines.len();

    // Skip leading empty lines
    while start_idx < lines.len() && lines[start_idx].trim().is_empty() {
        start_idx += 1;
    }

    // Remove trailing prompt lines
    while end_idx > start_idx {
        let line = lines[end_idx - 1].trim();
        if line.is_empty() || is_prompt_line(line, agent) {
            end_idx -= 1;
        } else {
            break;
        }
    }

    let response = lines[start_idx..end_idx].join("\n");
    Ok(response.trim().to_string())
}

/// Detect if the pane output ends with an agent prompt (ready for input).
fn is_agent_prompt(content: &str, agent: &str) -> bool {
    let last_lines: Vec<&str> = content
        .lines()
        .rev()
        .take(5)
        .filter(|l| !l.trim().is_empty())
        .collect();

    last_lines.iter().any(|line| is_prompt_line(line, agent))
}

/// Check if a line looks like an agent prompt.
fn is_prompt_line(line: &str, agent: &str) -> bool {
    let stripped = strip_ansi_codes(line);
    let trimmed = stripped.trim();

    match agent {
        "claude" | "kimi" | "minimax" => {
            // Claude Code prompt patterns:
            // "❯ " or "> " at end, or contains the prompt character
            trimmed.ends_with('❯')
                || trimmed.ends_with("> ")
                || trimmed.contains('❯')
                || trimmed.ends_with('>')
                // Claude Code also shows a box-drawing prompt
                || trimmed.starts_with('╰')
                || trimmed.starts_with('╭')
        }
        "codex" => trimmed.ends_with('>') || trimmed.ends_with("$ ") || trimmed.contains("codex>"),
        "opencode" => {
            trimmed.ends_with('>') || trimmed.ends_with("$ ") || trimmed.contains("opencode>")
        }
        _ => trimmed.ends_with('>') || trimmed.ends_with("$ ") || trimmed.ends_with('❯'),
    }
}

/// Strip ANSI escape codes from terminal output.
pub fn strip_ansi_codes(input: &str) -> String {
    // Match ANSI escape sequences:
    // - CSI sequences: ESC [ ... final_byte
    // - OSC sequences: ESC ] ... ST (or BEL)
    // - Simple escapes: ESC followed by a single char
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // ESC character — start of escape sequence
            match chars.peek() {
                Some('[') => {
                    // CSI sequence: ESC [ params... final_byte (0x40-0x7E)
                    chars.next(); // consume '['
                    while let Some(&c) = chars.peek() {
                        if (0x40..=0x7E).contains(&(c as u32)) {
                            chars.next(); // consume final byte
                            break;
                        }
                        chars.next(); // consume parameter/intermediate bytes
                    }
                }
                Some(']') => {
                    // OSC sequence: ESC ] ... (ST or BEL)
                    chars.next(); // consume ']'
                    while let Some(&c) = chars.peek() {
                        if c == '\x07' {
                            chars.next(); // BEL terminator
                            break;
                        }
                        if c == '\x1b' {
                            chars.next(); // ESC
                            if chars.peek() == Some(&'\\') {
                                chars.next(); // ST terminator
                            }
                            break;
                        }
                        chars.next();
                    }
                }
                Some(_) => {
                    // Simple escape: ESC + one char
                    chars.next();
                }
                None => {}
            }
        } else {
            result.push(ch);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_basic_color() {
        assert_eq!(strip_ansi_codes("\x1b[31mred text\x1b[0m"), "red text");
    }

    #[test]
    fn strip_ansi_complex() {
        assert_eq!(
            strip_ansi_codes("\x1b[1;32mbold green\x1b[0m normal"),
            "bold green normal"
        );
    }

    #[test]
    fn strip_ansi_no_codes() {
        assert_eq!(strip_ansi_codes("plain text"), "plain text");
    }

    #[test]
    fn strip_ansi_osc_sequence() {
        assert_eq!(strip_ansi_codes("\x1b]0;title\x07content"), "content");
    }

    #[test]
    fn is_claude_prompt() {
        assert!(is_prompt_line("❯ ", "claude"));
        assert!(is_prompt_line("  ❯", "claude"));
        assert!(!is_prompt_line("some response text", "claude"));
    }

    #[test]
    fn is_codex_prompt() {
        assert!(is_prompt_line("codex> ", "codex"));
        assert!(is_prompt_line("> ", "codex"));
    }

    #[test]
    fn extract_response_basic() {
        let before = "previous content\n";
        let after = "previous content\nuser message\nagent response line 1\nresponse line 2\n❯ ";
        let result = extract_response(after, before.len(), "claude").unwrap();
        assert!(result.contains("agent response line 1"));
        assert!(result.contains("response line 2"));
        assert!(!result.contains("❯"));
    }

    #[test]
    fn tmux_session_name_format() {
        assert_eq!(tmux_session_name("default"), "orch-chat-default");
        assert_eq!(tmux_session_name("ops"), "orch-chat-ops");
    }

    #[test]
    fn build_claude_command() {
        let cmd = build_interactive_command("claude", "sonnet", "/tmp/sys.md");
        assert!(cmd.contains("claude"));
        assert!(cmd.contains("--model sonnet"));
        assert!(cmd.contains("--append-system-prompt"));
        assert!(cmd.contains("bypassPermissions"));
    }

    #[test]
    fn build_codex_command() {
        let cmd = build_interactive_command("codex", "gpt-4o", "/tmp/sys.md");
        assert!(cmd.contains("codex"));
        assert!(cmd.contains("--model gpt-4o"));
        assert!(cmd.contains("--full-auto"));
    }

    #[test]
    fn build_opencode_command() {
        let cmd = build_interactive_command("opencode", "deepseek-r1", "/tmp/sys.md");
        assert!(cmd.contains("opencode"));
        assert!(cmd.contains("--model deepseek-r1"));
    }
}

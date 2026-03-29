//! CLI handler for `orch events` and `orch task watch` — connects to the
//! event bus websocket server and prints task status transitions in real-time.
//!
//! If the websocket is unreachable (stale port file after service restart/crash),
//! falls back to polling the SQLite store for status changes.

use anyhow::Context;
use futures::StreamExt;
use std::collections::HashMap;
use std::net::TcpStream;
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio_tungstenite::connect_async;

/// Read the websocket port from `~/.orch/state/ws.port`.
fn read_ws_port() -> anyhow::Result<u16> {
    let state_dir = crate::home::state_dir()?;
    let port_str = std::fs::read_to_string(state_dir.join("ws.port"))
        .context("event bus not running — is the service started? (ws.port not found)")?;
    port_str
        .trim()
        .parse()
        .context("invalid port number in ws.port")
}

/// Probe whether a TCP socket is reachable on the given port.
///
/// Uses a short timeout (500ms) so the CLI doesn't hang on stale ports.
fn probe_ws_port(port: u16) -> bool {
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    TcpStream::connect_timeout(
        &std::net::SocketAddr::V4(addr),
        std::time::Duration::from_millis(500),
    )
    .is_ok()
}

/// Stream events from the event bus websocket. Falls back to store polling
/// when the websocket port is stale or unreachable.
///
/// Output format: `HH:MM:SS task_id old_status → new_status agent PR: #N`
pub async fn stream(repo: Option<&str>, task: Option<&str>) -> anyhow::Result<()> {
    // Try to read and probe the websocket port.
    match read_ws_port() {
        Ok(port) if probe_ws_port(port) => {
            // Port is alive — connect via websocket.
            stream_from_ws(port, repo, task).await
        }
        Ok(port) => {
            eprintln!("Event bus port {port} is not reachable — falling back to store polling.");
            stream_from_store(repo, task).await
        }
        Err(_) => {
            // No port file at all — service probably isn't running.
            eprintln!("Event bus not available — falling back to store polling.");
            stream_from_store(repo, task).await
        }
    }
}

/// Connect to the event bus websocket and stream events.
async fn stream_from_ws(port: u16, repo: Option<&str>, task: Option<&str>) -> anyhow::Result<()> {
    let mut url = format!("ws://127.0.0.1:{port}/events");
    let mut params = Vec::new();
    if let Some(r) = repo {
        params.push(format!("repo={}", urlencoding::encode(r)));
    }
    if let Some(t) = task {
        params.push(format!("task_id={}", urlencoding::encode(t)));
    }
    if !params.is_empty() {
        url.push('?');
        url.push_str(&params.join("&"));
    }

    let (ws, _) = connect_async(&url)
        .await
        .context("failed to connect to event bus — is the service running?")?;

    let (_write, mut read) = ws.split();

    println!("Connected to event bus (port {port}). Streaming events...\n");

    // Open the store for lag resyncs (best-effort — None if unavailable).
    let store = crate::cli::init_store().await.ok();

    while let Some(msg) = read.next().await {
        let msg = msg?;
        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            // First try to parse as a control message (lag notification).
            if let Ok(ctrl) = serde_json::from_str::<crate::engine::events::ControlMessage>(&text) {
                if ctrl.kind == "lagged" {
                    eprintln!(
                        "WARNING: event bus lagged — {} task transition(s) were dropped. \
                         Resyncing current task state from store...",
                        ctrl.missed
                    );
                    if let Some(ref s) = store {
                        resync_from_store(s, repo, task).await;
                    }
                }
                continue;
            }

            // Normal task event.
            let Ok(event) = serde_json::from_str::<crate::engine::events::TaskEvent>(&text) else {
                continue;
            };

            print_event(&event);
        }
    }

    Ok(())
}

/// Format and print a single task event.
fn print_event(event: &crate::engine::events::TaskEvent) {
    let time = if event.timestamp.len() >= 19 {
        &event.timestamp[11..19]
    } else {
        &event.timestamp
    };
    let agent_str = event.agent.as_deref().unwrap_or("");
    let pr_str = event
        .pr_number
        .as_ref()
        .map(|p| format!(" PR: #{p}"))
        .unwrap_or_default();

    println!(
        "{time} {} {} \u{2192} {} {agent_str}{pr_str}",
        event.task_id, event.old_status, event.new_status,
    );
}

/// Resync: print current status of all active tasks from the store after a lag event.
///
/// This gives the operator a point-in-time snapshot so they know where things
/// stand after missed transitions.
async fn resync_from_store(
    store: &crate::store::TaskStore,
    repo_filter: Option<&str>,
    task_filter: Option<&str>,
) {
    let tasks = match store.list_all_active_global().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("  resync failed: {e:#}");
            return;
        }
    };

    let now = chrono::Utc::now().format("%H:%M:%S");
    let mut printed = 0usize;
    for t in &tasks {
        if let Some(rf) = repo_filter {
            if !t.repo.contains(rf) {
                continue;
            }
        }
        let key = task_key(t);
        if let Some(tf) = task_filter {
            if key != tf && t.external_id.as_deref() != Some(tf) {
                continue;
            }
        }
        let agent_str = t.agent.as_deref().unwrap_or("");
        let pr_str = t
            .pr_number
            .map(|p| format!(" PR: #{p}"))
            .unwrap_or_default();
        println!(
            "{now} [resync] {} {} {agent_str}{pr_str}",
            key,
            t.status.as_str(),
        );
        printed += 1;
    }
    if printed == 0 {
        println!("{now} [resync] no active tasks");
    }
}

/// Fallback: poll the SQLite store for task status changes.
///
/// When watching a specific task (`task` is Some), polls that task's status
/// every 3 seconds and prints transitions. When watching all tasks, polls
/// all active tasks and prints any status changes.
async fn stream_from_store(
    repo_filter: Option<&str>,
    task_filter: Option<&str>,
) -> anyhow::Result<()> {
    let store = match crate::cli::init_store().await {
        Ok(s) => s,
        Err(e) => {
            anyhow::bail!(
                "Cannot stream events: store unavailable ({e:#}). Is the service running?"
            );
        }
    };

    println!("Watching store for status changes (polling every 3s)...\n");

    // Snapshot of last known statuses: task_key -> (status, agent, pr_number)
    let mut last_status: HashMap<String, (String, Option<String>, Option<i32>)> = HashMap::new();

    // Seed initial state
    let tasks = store.list_all_active_global().await.unwrap_or_default();
    for t in &tasks {
        let key = task_key(t);
        last_status.insert(
            key,
            (t.status.as_str().to_string(), t.agent.clone(), t.pr_number),
        );
    }

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
    loop {
        interval.tick().await;

        let tasks = match store.list_all_active_global().await {
            Ok(t) => t,
            Err(_) => continue,
        };

        for t in &tasks {
            let key = task_key(t);

            // Apply filters
            if let Some(ref rf) = repo_filter {
                if !t.repo.contains(rf) {
                    continue;
                }
            }
            if let Some(tf) = task_filter {
                if key != tf && t.external_id.as_deref() != Some(tf) {
                    continue;
                }
            }

            let new_status = t.status.as_str().to_string();

            match last_status.get(&key) {
                Some((old_status, _, _)) if *old_status == new_status => {
                    // No change
                }
                Some((old_status, _, _)) => {
                    // Status changed — print transition
                    let now = chrono::Utc::now().format("%H:%M:%S");
                    let agent_str = t.agent.as_deref().unwrap_or("");
                    let pr_str = t
                        .pr_number
                        .map(|p| format!(" PR: #{p}"))
                        .unwrap_or_default();

                    println!("{now} {key} {old_status} \u{2192} {new_status} {agent_str}{pr_str}",);
                    last_status.insert(key, (new_status, t.agent.clone(), t.pr_number));
                }
                None => {
                    // New task appeared
                    let now = chrono::Utc::now().format("%H:%M:%S");
                    let agent_str = t.agent.as_deref().unwrap_or("");
                    println!("{now} {key} - \u{2192} {new_status} {agent_str}");
                    last_status.insert(key, (new_status, t.agent.clone(), t.pr_number));
                }
            }
        }

        // Check for tasks that were removed (completed + cleaned up)
        let active_keys: std::collections::HashSet<String> = tasks.iter().map(task_key).collect();
        let disappeared: Vec<String> = last_status
            .keys()
            .filter(|k| !active_keys.contains(*k))
            .cloned()
            .collect();
        for key in disappeared {
            if let Some((old_status, _, _)) = last_status.remove(&key) {
                if old_status != "done" {
                    let now = chrono::Utc::now().format("%H:%M:%S");
                    println!("{now} {key} {old_status} \u{2192} done");
                }
            }
        }
    }
}

/// Derive a display key for a task: prefer external_id, fall back to internal id.
fn task_key(t: &crate::store::Task) -> String {
    t.external_id
        .clone()
        .unwrap_or_else(|| format!("internal:{}", t.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_ws_port_unreachable() {
        // Port 1 should be unreachable (reserved).
        assert!(!probe_ws_port(1));
    }

    #[test]
    fn task_key_prefers_external_id() {
        let t = crate::store::Task {
            id: 42,
            external_id: Some("123".to_string()),
            repo: "o/r".into(),
            origin: "external".into(),
            title: "t".into(),
            body: String::new(),
            status: crate::store::TaskStatus::New,
            source: String::new(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: Vec::new(),
            agent: None,
            model: None,
            complexity: String::new(),
            route_reason: String::new(),
            agent_profile: String::new(),
            selected_skills: String::new(),
            route_attempts: 0,
            attempts: 0,
            branch: String::new(),
            worktree: String::new(),
            worktree_cleaned: false,
            summary: String::new(),
            last_error: String::new(),
            parent_id: None,
            block_reason: None,
            pr_number: None,
            pr_review_context: String::new(),
            last_review_ts: String::new(),
            last_comment_review_ts: String::new(),
            merge_conflict_retries: 0,
            ci_merge_failures: 0,
            pr_create_failures: 0,
            push_failures: 0,
            review_agent_failures: 0,
            review_cycles: 0,
            review_invocations: 0,
            review_session_expected: false,
            input_tokens: 0,
            output_tokens: 0,
            input_cost_usd: 0.0,
            output_cost_usd: 0.0,
            total_cost_usd: 0.0,
            model_reroute_chain: String::new(),
            limit_reroute_chain: String::new(),
            budget_warning: String::new(),
            budget_exceeded: false,
            memory: Vec::new(),
            delegations: Vec::new(),
            auto_unblock_count: 0,
            auto_unblock_last_at: String::new(),
            ci_recovery_count: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert_eq!(task_key(&t), "123");
    }

    #[test]
    fn task_key_falls_back_to_internal() {
        let t = crate::store::Task {
            id: 7,
            external_id: None,
            repo: "o/r".into(),
            origin: "internal".into(),
            title: "t".into(),
            body: String::new(),
            status: crate::store::TaskStatus::InProgress,
            source: String::new(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: Vec::new(),
            agent: Some("claude".to_string()),
            model: None,
            complexity: String::new(),
            route_reason: String::new(),
            agent_profile: String::new(),
            selected_skills: String::new(),
            route_attempts: 0,
            attempts: 1,
            branch: String::new(),
            worktree: String::new(),
            worktree_cleaned: false,
            summary: String::new(),
            last_error: String::new(),
            parent_id: None,
            block_reason: None,
            pr_number: None,
            pr_review_context: String::new(),
            last_review_ts: String::new(),
            last_comment_review_ts: String::new(),
            merge_conflict_retries: 0,
            ci_merge_failures: 0,
            pr_create_failures: 0,
            push_failures: 0,
            review_agent_failures: 0,
            review_cycles: 0,
            review_invocations: 0,
            review_session_expected: false,
            input_tokens: 0,
            output_tokens: 0,
            input_cost_usd: 0.0,
            output_cost_usd: 0.0,
            total_cost_usd: 0.0,
            model_reroute_chain: String::new(),
            limit_reroute_chain: String::new(),
            budget_warning: String::new(),
            budget_exceeded: false,
            memory: Vec::new(),
            delegations: Vec::new(),
            auto_unblock_count: 0,
            auto_unblock_last_at: String::new(),
            ci_recovery_count: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert_eq!(task_key(&t), "internal:7");
    }
}

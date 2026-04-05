//! Channel message handlers — routes incoming channel messages to the appropriate action.
//!
//! Extracted from `engine/mod.rs` to keep the engine lifecycle code separate from
//! message dispatch logic. All handler functions here are stateless with respect to
//! the engine tick loop and operate only through the shared ref types passed in.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::backends::{ExternalId, Status};
use crate::channels::capture::CaptureService;
use crate::channels::routing::ChannelRouter;
use crate::channels::transport::conversation_key;
use crate::channels::transport::{MessageRoute, Transport};
use crate::channels::{ChannelRegistry, IncomingMessage, OutgoingMessage};
use crate::engine::commands::{execute_command, parse_command};
use crate::engine::tasks::{CreateTaskRequest, Task, TaskType};
use crate::github::http::GhHttp;
use crate::store::TaskStatus;
use crate::tmux::TmuxManager;

use super::EngineRef;

// ── Project picker ─────────────────────────────────────────────────────────

/// Timeout for pending project picks before they are discarded.
const PICK_TIMEOUT: Duration = Duration::from_secs(60);

/// A pending project selection waiting for the user to tap a button.
///
/// Stored in-memory with a 60-second TTL. If the user does not pick within
/// that window the entry is silently discarded on next access.
pub struct PendingPick {
    /// Original free-text body that will become the task title/body.
    pub original_body: String,
    /// When this pick expires and should be discarded.
    pub expires_at: Instant,
}

impl PendingPick {
    pub fn new(original_body: String) -> Self {
        Self {
            original_body,
            expires_at: Instant::now() + PICK_TIMEOUT,
        }
    }

    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// In-memory map of pending project picks, keyed by `"<channel>:<original_msg_id>"`.
///
/// The key uses the original user message ID so that the callback_data embedded in
/// button payloads (`pick:<original_msg_id>:<repo>`) maps back unambiguously.
pub type PendingPicks = Arc<tokio::sync::Mutex<HashMap<String, PendingPick>>>;

/// Parse a pick callback body of the form `"pick:<original_msg_id>:<repo>"`.
///
/// Returns `(original_msg_id, repo)` on success, `None` otherwise.
pub fn parse_pick_callback(body: &str) -> Option<(String, String)> {
    let rest = body.strip_prefix("pick:")?;
    let colon = rest.find(':')?;
    let orig_id = &rest[..colon];
    let repo = &rest[colon + 1..];
    if orig_id.is_empty() || repo.is_empty() {
        return None;
    }
    Some((orig_id.to_string(), repo.to_string()))
}

/// Build the list of `(button_label, callback_data)` pairs for a project picker.
///
/// `original_msg_id` is embedded in every `callback_data` so the handler can
/// look up the original message body in `pending_picks`.
///
/// Button label: the last path component of the repo slug (e.g. `"orch"` from
/// `"gabrielkoerich/orch"`), falling back to the full slug when no `/` is present.
pub fn buttons_for_picker(repos: &[&str], original_msg_id: &str) -> Vec<(String, String)> {
    repos
        .iter()
        .map(|repo| {
            let label = repo.rsplit('/').next().unwrap_or(repo).to_string();
            let callback_data = format!("pick:{original_msg_id}:{repo}");
            (label, callback_data)
        })
        .collect()
}

fn control_session_id(channel: &str, thread_id: &str, topic_id: Option<&str>) -> String {
    format!("control:{}", conversation_key(channel, thread_id, topic_id))
}

fn normalize_channel_route(
    msg: &IncomingMessage,
    route: MessageRoute,
    is_control: bool,
) -> MessageRoute {
    if is_control {
        return MessageRoute::ControlSession;
    }

    match route {
        // Telegram should behave like `orch chat` by default. Keep explicit
        // commands, bound task threads, and existing picker callbacks on their
        // normal paths.
        MessageRoute::NewTask
            if msg.channel == "telegram" && parse_pick_callback(msg.body.trim()).is_none() =>
        {
            MessageRoute::ControlSession
        }
        other => other,
    }
}

/// Send a reply message to a specific channel thread.
///
/// Iterates the channel registry and finds the channel by name,
/// then sends the message to the given thread ID.
/// Optionally sends to a specific topic (e.g. Telegram forum topic).
pub(super) async fn send_channel_reply(
    channels: &Arc<ChannelRegistry>,
    channel_name: &str,
    thread_id: &str,
    body: String,
    topic_id: Option<&str>,
) {
    for ch in channels.iter() {
        if ch.name() == channel_name {
            let msg = OutgoingMessage {
                thread_id: thread_id.to_string(),
                body,
                reply_to: None,
                metadata: serde_json::json!({}),
                topic_id: topic_id.map(String::from),
            };
            if let Err(e) = ch.send(&msg).await {
                tracing::warn!(
                    channel = channel_name,
                    thread_id,
                    ?e,
                    "failed to send channel reply"
                );
            }
            return;
        }
    }
    tracing::debug!(
        channel = channel_name,
        "channel not found in registry for reply"
    );
}

/// Forward a text message to an agent's tmux session via send-keys.
pub(super) async fn forward_to_tmux(
    transport: &Arc<Transport>,
    repo: &str,
    task_id: &str,
    text: &str,
) {
    if let Some(binding) = transport.get_binding(repo, task_id).await {
        if let Err(e) = crate::channels::tmux::send_keys(&binding.tmux_session, text).await {
            tracing::warn!(
                task_id,
                session = %binding.tmux_session,
                ?e,
                "failed to forward message to tmux session"
            );
        } else {
            tracing::debug!(
                task_id,
                session = %binding.tmux_session,
                "forwarded message to tmux"
            );
        }
    } else {
        tracing::warn!(task_id, "no tmux binding found, cannot forward message");
    }
}

/// Find the index in `repos` whose computed tmux session name matches `tmux_session`.
///
/// This is used to route `TaskSession` slash commands to the correct project backend
/// in a multi-project setup. Returns `0` (first project) when no match is found so
/// that single-project deployments continue to work without change.
fn engine_ref_idx_for_session(
    repos: &[&str],
    tmux: &TmuxManager,
    task_id: &str,
    tmux_session: &str,
) -> usize {
    repos
        .iter()
        .position(|repo| tmux.session_name(repo, task_id) == tmux_session)
        .unwrap_or(0)
}

/// Acknowledge an interactive callback for the appropriate channel.
///
/// Telegram requires `answerCallbackQuery` to dismiss the button spinner.
/// Discord buttons are already acknowledged (type 6) in the websocket handler.
/// Other channels are no-ops.
async fn ack_channel_interaction(
    channels: &Arc<ChannelRegistry>,
    channel_name: &str,
    metadata: &serde_json::Value,
) {
    if channel_name != "telegram" {
        return;
    }
    let cb_id = match metadata["callback_query_id"].as_str() {
        Some(id) => id,
        None => return,
    };
    for ch in channels.iter() {
        if ch.name() == channel_name {
            if let Err(e) = ch.ack_interaction(cb_id).await {
                tracing::warn!(channel = channel_name, ?e, "failed to ack callback query");
            }
            return;
        }
    }
}

/// Create an internal task in the named project and send a confirmation reply.
///
/// Used both by the direct `NewTask` path and by the project-picker callback path.
///
/// `forced_repo` overrides project resolution and ALWAYS shows the `in [repo]` label.
#[allow(clippy::too_many_arguments)]
async fn create_and_announce_task(
    body: &str,
    forced_repo: Option<&str>,
    channel: &str,
    thread_id: &str,
    topic_id: Option<&str>,
    engine_refs: &[EngineRef],
    transport: &Arc<Transport>,
    tmux: &Arc<TmuxManager>,
    capture: &Arc<CaptureService>,
    channels: &Arc<ChannelRegistry>,
) {
    use crate::channels::stream::fanout_output;

    let target = if let Some(repo) = forced_repo {
        engine_refs.iter().find(|(r, _, _, _)| r == repo)
    } else {
        engine_refs.first()
    };

    let Some((repo, _, task_manager, _)) = target else {
        tracing::warn!("no project configured, cannot create task from channel message");
        send_channel_reply(
            channels,
            channel,
            thread_id,
            "No project configured. Cannot create task.".to_string(),
            topic_id,
        )
        .await;
        return;
    };

    let title = if body.chars().count() > 80 {
        let truncated: String = body.chars().take(80).collect();
        format!("{}…", truncated)
    } else {
        body.to_string()
    };
    let req = CreateTaskRequest {
        title,
        body: body.to_string(),
        task_type: TaskType::Internal,
        labels: vec!["channel-created".to_string()],
        source: channel.to_string(),
        source_id: thread_id.to_string(),
    };
    match task_manager.create_task(req).await {
        Ok(task) => {
            let task_id = match &task {
                Task::Internal(t) => format!("internal:{}", t.id),
                Task::External(t) => t.id.0.clone(),
            };
            let session_name = tmux.session_name(repo, &task_id);
            transport
                .bind(repo, &task_id, &session_name, channel, thread_id, topic_id)
                .await;
            capture
                .register_session(repo, &task_id, &session_name)
                .await;
            let transport_clone = transport.clone();
            let channels_clone = channels.clone();
            let task_id_clone = task_id.clone();
            let repo_clone = repo.to_string();
            tokio::spawn(async move {
                fanout_output(repo_clone, task_id_clone, transport_clone, channels_clone).await;
            });
            // Always show project label (caller sets forced_repo when project isn't obvious)
            let project_label = format!(" in [{repo}]");
            let reply =
                format!("Task created: `{task_id}`{project_label} — I'll start working on it now.");
            send_channel_reply(channels, channel, thread_id, reply, topic_id).await;
        }
        Err(e) => {
            tracing::warn!(repo, err = %e, "failed to create task from channel message");
            send_channel_reply(
                channels,
                channel,
                thread_id,
                format!("Failed to create task: {e}"),
                topic_id,
            )
            .await;
        }
    }
}

/// Handle an incoming channel message by routing it to the appropriate action.
///
/// - `TaskSession`: slash command → execute on task; otherwise → forward to tmux
/// - `Command`: global commands like `/status`, `/stats`, `/subscribe`, `/unsubscribe`, `/stream`
/// - `NewTask`: create an internal task, bind thread, start output fanout
///
/// The `channel_router` resolves which project a message targets based on
/// its topic/channel ID. This enables per-project task creation and status views.
///
/// When a free-text message arrives on the General channel with multiple projects
/// configured, a project picker is shown instead of silently using the first project.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_channel_message(
    msg: IncomingMessage,
    transport: &Arc<Transport>,
    tmux: &Arc<TmuxManager>,
    capture: &Arc<CaptureService>,
    channels: &Arc<ChannelRegistry>,
    engine_refs: &[EngineRef],
    channel_router: &Arc<ChannelRouter>,
    pending_picks: &PendingPicks,
) {
    // Resolve project from incoming message topic/channel
    let topic_id = msg.topic_id.as_deref().unwrap_or(&msg.thread_id);
    let resolved_repo = channel_router.resolve_project(&msg.channel, topic_id);
    let is_general = channel_router.is_general(&msg.channel, topic_id);
    let is_control = channel_router.is_control_channel(&msg.channel, topic_id);
    let msg_topic_id = msg.topic_id.clone();

    // Check control-channel overrides before dispatch so configured control
    // topics and Telegram default chat sessions both land on the chat path.
    let route = normalize_channel_route(&msg, transport.route(&msg).await, is_control);

    match route {
        MessageRoute::TaskSession { session_key } => {
            let body = msg.body.trim().to_string();
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();

            // Derive repo and task_id from the session key.
            // For external tasks the key is "repo:task_id"; for internal tasks
            // it is "internal:<id>" and we fall back to resolved_repo.
            let (repo, task_id): (&str, &str) =
                if let Some((r, t)) = crate::channels::transport::parse_session_key(&session_key) {
                    (r, t)
                } else {
                    // Internal task or unparseable key — use resolved_repo
                    (resolved_repo.unwrap_or(""), &session_key)
                };

            if body.starts_with('/') {
                // Parse slash command and execute it on the bound task
                if let Some(cmd) = parse_command(&body) {
                    // Find the engine ref that owns this task by matching the tmux session
                    // name stored in the transport binding.  In a multi-project setup this
                    // prevents routing the command to the wrong project's GitHub backend.
                    // Falls back to the first project when no binding / no match is found.
                    let repos: Vec<&str> =
                        engine_refs.iter().map(|(r, _, _, _)| r.as_str()).collect();
                    let idx = transport
                        .get_binding(repo, task_id)
                        .await
                        .map(|b| engine_ref_idx_for_session(&repos, tmux, task_id, &b.tmux_session))
                        .unwrap_or(0);
                    if let Some((repo, backend, task_manager, store)) = engine_refs.get(idx) {
                        let gh = match GhHttp::new() {
                            Ok(gh) => gh,
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to build HTTP client for command execution");
                                send_channel_reply(
                                    channels,
                                    &channel,
                                    &thread_id,
                                    format!("Command `{cmd}` failed: {e}"),
                                    msg_topic_id.as_deref(),
                                )
                                .await;
                                return;
                            }
                        };
                        let ext_id = ExternalId(task_id.to_string());
                        let result =
                            execute_command(backend, &gh, repo, &ext_id, &cmd, store, task_manager)
                                .await;
                        let reply = match result {
                            Ok(r) => r,
                            Err(e) => format!("Command `{cmd}` failed: {e}"),
                        };
                        send_channel_reply(
                            channels,
                            &channel,
                            &thread_id,
                            reply,
                            msg_topic_id.as_deref(),
                        )
                        .await;
                    }
                } else {
                    // Unknown command — forward to agent as-is
                    forward_to_tmux(transport, repo, task_id, &body).await;
                }
            } else {
                // Regular message — forward to the agent's tmux session
                forward_to_tmux(transport, repo, task_id, &body).await;
            }
        }

        MessageRoute::Command { raw } => {
            let cmd_str = raw.trim().to_string();
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();

            if cmd_str == "/status" || cmd_str.starts_with("/status ") {
                // /status — project-aware: show tasks for resolved project or all
                let reply = handle_status_command(engine_refs, resolved_repo).await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if cmd_str == "/stats" || cmd_str.starts_with("/stats ") {
                let reply = handle_stats_command(engine_refs, resolved_repo, is_general).await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if cmd_str.starts_with("/subscribe") {
                let reply = handle_subscribe_command(
                    &cmd_str,
                    &channel,
                    &thread_id,
                    msg_topic_id.as_deref(),
                    engine_refs,
                    channel_router,
                )
                .await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if cmd_str.starts_with("/unsubscribe") {
                let reply = handle_unsubscribe_command(
                    &cmd_str,
                    &channel,
                    &thread_id,
                    engine_refs,
                    channel_router,
                )
                .await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if cmd_str.starts_with("/stream") {
                let reply = handle_stream_command(
                    &cmd_str,
                    &channel,
                    &thread_id,
                    msg_topic_id.as_deref(),
                    transport,
                    engine_refs,
                )
                .await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if let Some(cmd) = parse_command(&cmd_str) {
                let reply = format!(
                    "Command `{cmd}` requires a task context. \
                     Send it in a thread that is bound to a running task."
                );
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else {
                let reply = "Available commands: /status, /stats, /subscribe, /unsubscribe, \
                             /stream, /retry, /close, /block, /unblock, /review — \
                             send task-specific commands in a task thread."
                    .to_string();
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            }
        }

        MessageRoute::ControlSession => {
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();
            let body = msg.body.clone();

            // Session ID is per channel+topic so Telegram and Discord are isolated.
            let session_id = control_session_id(&channel, &thread_id, msg_topic_id.as_deref());
            let channel_thread = conversation_key(&channel, &thread_id, msg_topic_id.as_deref());

            // Find a store from the first available engine reference.
            let store = engine_refs
                .iter()
                .find_map(|(_, _, _, s)| s.as_ref())
                .cloned();

            if let Some(store) = store {
                match crate::control::send_message(
                    &store,
                    &session_id,
                    &channel,
                    Some(&channel_thread),
                    &body,
                )
                .await
                {
                    Ok(reply) => {
                        send_channel_reply(
                            channels,
                            &channel,
                            &thread_id,
                            reply,
                            msg_topic_id.as_deref(),
                        )
                        .await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "control session agent invocation failed");
                        send_channel_reply(
                            channels,
                            &channel,
                            &thread_id,
                            format!("Control session error: {e}"),
                            msg_topic_id.as_deref(),
                        )
                        .await;
                    }
                }
            } else {
                tracing::warn!("no store available, cannot handle control session message");
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    "No projects configured. Please add a project with `orch project add` before using the control channel.".to_string(),
                    msg_topic_id.as_deref(),
                )
                .await;
            }
        }

        MessageRoute::NewTask => {
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();

            // Handle pick callback (user tapped a project button).
            if let Some((orig_id, repo)) = parse_pick_callback(msg.body.trim()) {
                let pick_key = format!("{channel}:{orig_id}");
                let original_body = {
                    let mut picks = pending_picks.lock().await;
                    picks
                        .remove(&pick_key)
                        .filter(|p| !p.is_expired())
                        .map(|p| p.original_body)
                };
                if let Some(body) = original_body {
                    // Acknowledge the interaction (Telegram spinner, Discord already ack'd)
                    ack_channel_interaction(channels, &channel, &msg.metadata).await;

                    create_and_announce_task(
                        &body,
                        Some(repo.as_str()),
                        &channel,
                        &thread_id,
                        msg_topic_id.as_deref(),
                        engine_refs,
                        transport,
                        tmux,
                        capture,
                        channels,
                    )
                    .await;
                } else {
                    tracing::debug!(
                        pick_key,
                        "pick callback received but no matching pending pick (expired or unknown)"
                    );
                    send_channel_reply(
                        channels,
                        &channel,
                        &thread_id,
                        "Sorry, that selection has expired. Please send your message again."
                            .to_string(),
                        msg_topic_id.as_deref(),
                    )
                    .await;
                }
                return;
            }

            // Multi-project General channel: show a picker instead of silently using first.
            if resolved_repo.is_none() && engine_refs.len() > 1 {
                let repos: Vec<&str> = engine_refs.iter().map(|(r, _, _, _)| r.as_str()).collect();
                let buttons = buttons_for_picker(&repos, &msg.id);
                let buttons_json: Vec<serde_json::Value> = buttons
                    .iter()
                    .map(|(text, cb)| serde_json::json!({ "text": text, "callback_data": cb }))
                    .collect();

                // Store the original body so the pick callback can create the task.
                {
                    let key = format!("{channel}:{}", msg.id);
                    let mut picks = pending_picks.lock().await;
                    // Evict expired entries to prevent unbounded growth.
                    picks.retain(|_, v| !v.is_expired());
                    picks.insert(key, PendingPick::new(msg.body.clone()));
                }

                let picker_msg = OutgoingMessage {
                    thread_id: thread_id.clone(),
                    body: "Which project should I create this task in?".to_string(),
                    reply_to: None,
                    metadata: serde_json::json!({ "buttons": buttons_json }),
                    topic_id: msg_topic_id.clone(),
                };
                for ch in channels.iter() {
                    if ch.name() == channel {
                        if let Err(e) = ch.send(&picker_msg).await {
                            tracing::warn!(channel, ?e, "failed to send project picker");
                        }
                        break;
                    }
                }
                return;
            }

            // Single project (or specific project resolved): create task directly.
            create_and_announce_task(
                &msg.body,
                resolved_repo,
                &channel,
                &thread_id,
                msg_topic_id.as_deref(),
                engine_refs,
                transport,
                tmux,
                capture,
                channels,
            )
            .await;
        }
    }
}

/// Handle `/status` command with project-aware filtering.
pub(super) async fn handle_status_command(
    engine_refs: &[EngineRef],
    resolved_repo: Option<&str>,
) -> String {
    let mut lines = vec!["**Active tasks:**".to_string()];
    for (repo, _, task_manager, _) in engine_refs {
        // If a specific project was resolved, only show that project
        if let Some(target) = resolved_repo {
            if repo != target {
                continue;
            }
        }
        match task_manager
            .list_external_by_status(Status::InProgress)
            .await
        {
            Ok(tasks) => {
                for t in &tasks {
                    lines.push(format!("- #{} [{}]: {}", t.id.0, repo, t.title));
                }
            }
            Err(e) => {
                tracing::warn!(repo, err = %e, "failed to list in-progress tasks");
            }
        }
        match task_manager
            .list_internal_by_status(TaskStatus::InProgress)
            .await
        {
            Ok(tasks) => {
                for t in &tasks {
                    lines.push(format!("- #{} [{}]: {}", t.id.0, repo, t.title));
                }
            }
            Err(e) => {
                tracing::warn!(repo, err = %e, "failed to list in-progress internal tasks");
            }
        }
    }
    if lines.len() == 1 {
        lines.push("No tasks currently in progress.".to_string());
    }
    lines.join("\n")
}

/// Handle `/stats` command — show 24h metrics, optionally per-project.
pub(super) async fn handle_stats_command(
    engine_refs: &[EngineRef],
    resolved_repo: Option<&str>,
    is_general: bool,
) -> String {
    let mut lines = vec!["**Stats (24h)**".to_string(), String::new()];

    // Determine which repos to query
    let repos_to_query: Vec<&str> = if let Some(repo) = resolved_repo {
        vec![repo]
    } else {
        engine_refs.iter().map(|(r, _, _, _)| r.as_str()).collect()
    };

    for repo in &repos_to_query {
        // Find the store for this repo
        let store = engine_refs
            .iter()
            .find(|(r, _, _, _)| r == repo)
            .and_then(|(_, _, _, s)| s.as_ref());

        let store = match store {
            Some(s) => s,
            None => continue,
        };

        match store.get_metrics_summary_by_repo(repo, 24).await {
            Ok(summary) => {
                let total = summary.tasks_completed_24h + summary.tasks_failed_24h;
                let rate = if total > 0 {
                    (summary.tasks_completed_24h as f64 / total as f64) * 100.0
                } else {
                    0.0
                };
                // Show repo name only if showing multiple (general or no resolution)
                if is_general || resolved_repo.is_none() {
                    lines.push(format!(
                        "**{}**: {} done, {} failed ({:.1}%)",
                        repo, summary.tasks_completed_24h, summary.tasks_failed_24h, rate
                    ));
                } else {
                    lines.push(format!(
                        "{} done, {} failed ({:.1}%)",
                        summary.tasks_completed_24h, summary.tasks_failed_24h, rate
                    ));
                }
                // Per-agent breakdown
                for agent in &summary.agent_stats {
                    lines.push(format!(
                        "  {}: {} runs ({:.0}%)",
                        agent.agent, agent.total_runs, agent.success_rate
                    ));
                }
                if !summary.agent_stats.is_empty() {
                    lines.push(String::new());
                }
            }
            Err(e) => {
                tracing::warn!(repo, err = %e, "failed to query metrics");
                lines.push(format!("**{}**: error fetching metrics", repo));
            }
        }
    }

    if lines.len() <= 2 {
        lines.push("No metrics data available.".to_string());
    }
    lines.join("\n")
}

/// Handle `/subscribe <project>` command.
pub(super) async fn handle_subscribe_command(
    cmd_str: &str,
    channel: &str,
    thread_id: &str,
    topic_id: Option<&str>,
    engine_refs: &[EngineRef],
    channel_router: &Arc<ChannelRouter>,
) -> String {
    let parts: Vec<&str> = cmd_str.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[1].trim().is_empty() {
        let projects: Vec<&str> = channel_router
            .projects()
            .iter()
            .map(|s| s.as_str())
            .collect();
        return format!(
            "Usage: `/subscribe <project>`\nAvailable projects: {}",
            projects.join(", ")
        );
    }
    let project = parts[1].trim();

    // Validate project exists
    if !channel_router.projects().iter().any(|p| p == project) {
        return format!(
            "Unknown project `{project}`. Available: {}",
            channel_router
                .projects()
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Find a store to use for subscription
    if let Some((_, _, _, Some(store))) = engine_refs.first() {
        match store
            .subscribe_channel(channel, thread_id, project, topic_id)
            .await
        {
            Ok(()) => format!("Subscribed to notifications from `{project}`."),
            Err(e) => format!("Failed to subscribe: {e}"),
        }
    } else {
        "No store available for subscription management.".to_string()
    }
}

/// Handle `/unsubscribe <project>` command.
pub(super) async fn handle_unsubscribe_command(
    cmd_str: &str,
    channel: &str,
    thread_id: &str,
    engine_refs: &[EngineRef],
    channel_router: &Arc<ChannelRouter>,
) -> String {
    let parts: Vec<&str> = cmd_str.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[1].trim().is_empty() {
        return "Usage: `/unsubscribe <project>`".to_string();
    }
    let project = parts[1].trim();

    // Validate project exists
    if !channel_router.projects().iter().any(|p| p == project) {
        return format!("Unknown project `{project}`.");
    }

    if let Some((_, _, _, Some(store))) = engine_refs.first() {
        match store.unsubscribe_channel(channel, thread_id, project).await {
            Ok(()) => format!("Unsubscribed from `{project}` notifications."),
            Err(e) => format!("Failed to unsubscribe: {e}"),
        }
    } else {
        "No store available for subscription management.".to_string()
    }
}

/// Handle `/stream <task_id>` command — bind channel to task output stream.
pub(super) async fn handle_stream_command(
    cmd_str: &str,
    channel: &str,
    thread_id: &str,
    topic_id: Option<&str>,
    transport: &Arc<Transport>,
    engine_refs: &[EngineRef],
) -> String {
    let parts: Vec<&str> = cmd_str.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[1].trim().is_empty() {
        return "Usage: `/stream <task_id>`".to_string();
    }
    let task_id = parts[1].trim();

    // Check if the task has an active binding (i.e. is running)
    // Try all known repos to find the binding
    let repos: Vec<&str> = engine_refs.iter().map(|(r, _, _, _)| r.as_str()).collect();
    let mut found = None;
    for repo in &repos {
        if let Some(binding) = transport.get_binding(repo, task_id).await {
            found = Some((repo.to_string(), binding));
            break;
        }
    }

    if let Some((repo, binding)) = found {
        // Bind this channel/thread as an additional output target
        // The session name is retrieved from the existing binding
        transport
            .bind(
                &repo,
                task_id,
                &binding.tmux_session,
                channel,
                thread_id,
                topic_id,
            )
            .await;
        format!("Streaming output from task `{task_id}` to this channel.")
    } else {
        format!("Task `{task_id}` is not currently running or has no active session.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn parse_pick_callback_parses_valid_input() {
        let (orig_id, repo) = parse_pick_callback("pick:msg123:owner/repo").unwrap();
        assert_eq!(orig_id, "msg123");
        assert_eq!(repo, "owner/repo");
    }

    #[test]
    fn parse_pick_callback_returns_none_for_non_pick_prefix() {
        assert!(parse_pick_callback("not-a-pick").is_none());
        assert!(parse_pick_callback("/command").is_none());
        assert!(parse_pick_callback("").is_none());
    }

    #[test]
    fn parse_pick_callback_returns_none_when_too_few_parts() {
        // "pick:" with no id/repo
        assert!(parse_pick_callback("pick:").is_none());
        // "pick:only-one-part" — no repo after second colon
        assert!(parse_pick_callback("pick:only-one-part").is_none());
    }

    #[test]
    fn parse_pick_callback_allows_slash_in_repo() {
        let (orig_id, repo) = parse_pick_callback("pick:abc:owner/repo-name").unwrap();
        assert_eq!(orig_id, "abc");
        assert_eq!(repo, "owner/repo-name");
    }

    #[test]
    fn pending_pick_is_expired_when_deadline_in_past() {
        let pick = PendingPick {
            original_body: "fix the login button".to_string(),
            expires_at: Instant::now() - Duration::from_secs(1),
        };
        assert!(pick.is_expired());
    }

    #[test]
    fn pending_pick_is_not_expired_before_deadline() {
        let pick = PendingPick {
            original_body: "fix the login button".to_string(),
            expires_at: Instant::now() + Duration::from_secs(60),
        };
        assert!(!pick.is_expired());
    }

    #[test]
    fn buttons_for_picker_generates_pick_callback_data() {
        let repos = ["owner/project-a", "owner/project-b"];
        let buttons = buttons_for_picker(&repos, "msg_123");
        assert_eq!(buttons.len(), 2);
        assert_eq!(
            buttons[0],
            (
                "project-a".to_string(),
                "pick:msg_123:owner/project-a".to_string()
            )
        );
        assert_eq!(
            buttons[1],
            (
                "project-b".to_string(),
                "pick:msg_123:owner/project-b".to_string()
            )
        );
    }

    #[test]
    fn buttons_for_picker_uses_full_name_when_no_slash() {
        let repos = ["myrepo"];
        let buttons = buttons_for_picker(&repos, "id1");
        assert_eq!(buttons[0].0, "myrepo");
        assert_eq!(buttons[0].1, "pick:id1:myrepo");
    }

    #[test]
    fn control_session_id_uses_conversation_key() {
        assert_eq!(
            control_session_id("telegram", "123", Some("456")),
            "control:telegram:123|456"
        );
        assert_eq!(
            control_session_id("telegram", "123", None),
            "control:telegram:123"
        );
    }

    #[test]
    fn normalize_channel_route_promotes_telegram_new_task_to_control_session() {
        let msg = IncomingMessage {
            channel: "telegram".to_string(),
            id: "m1".to_string(),
            thread_id: "42".to_string(),
            author: "user".to_string(),
            body: "what's running?".to_string(),
            timestamp: chrono::Utc::now(),
            metadata: serde_json::json!({}),
            topic_id: None,
        };

        assert!(matches!(
            normalize_channel_route(&msg, MessageRoute::NewTask, false),
            MessageRoute::ControlSession
        ));
    }

    #[test]
    fn normalize_channel_route_preserves_picker_callbacks() {
        let msg = IncomingMessage {
            channel: "telegram".to_string(),
            id: "m1".to_string(),
            thread_id: "42".to_string(),
            author: "user".to_string(),
            body: "pick:m1:owner/repo".to_string(),
            timestamp: chrono::Utc::now(),
            metadata: serde_json::json!({}),
            topic_id: None,
        };

        assert!(matches!(
            normalize_channel_route(&msg, MessageRoute::NewTask, false),
            MessageRoute::NewTask
        ));
    }

    #[test]
    fn normalize_channel_route_preserves_non_telegram_new_task() {
        let msg = IncomingMessage {
            channel: "discord".to_string(),
            id: "m1".to_string(),
            thread_id: "42".to_string(),
            author: "user".to_string(),
            body: "create a task".to_string(),
            timestamp: chrono::Utc::now(),
            metadata: serde_json::json!({}),
            topic_id: None,
        };

        assert!(matches!(
            normalize_channel_route(&msg, MessageRoute::NewTask, false),
            MessageRoute::NewTask
        ));
    }

    /// Regression test for issue #780: TaskSession slash commands must use the engine_ref
    /// that owns the task, not always the first one.
    ///
    /// In a two-project setup where project-b owns task "42" (its tmux session is
    /// "orch-project-b-42"), `engine_ref_idx_for_session` must return 1 (project-b),
    /// not 0 (project-a).
    #[test]
    fn engine_ref_idx_for_session_returns_owning_project() {
        let tmux = TmuxManager::new();
        let repos: Vec<&str> = vec!["owner/project-a", "owner/project-b"];
        let task_id = "42";
        let session_b = tmux.session_name("owner/project-b", task_id); // "orch-project-b-42"

        let idx = engine_ref_idx_for_session(&repos, &tmux, task_id, &session_b);
        assert_eq!(
            idx, 1,
            "should resolve to project-b (index 1), not project-a"
        );
    }

    /// Regression test for issue #780: when no session name matches any project,
    /// fall back to the first engine_ref (index 0) to preserve existing behaviour.
    #[test]
    fn engine_ref_idx_for_session_falls_back_to_zero_on_no_match() {
        let tmux = TmuxManager::new();
        let repos: Vec<&str> = vec!["owner/project-a", "owner/project-b"];
        let task_id = "42";

        let idx = engine_ref_idx_for_session(&repos, &tmux, task_id, "orch-unknown-42");
        assert_eq!(
            idx, 0,
            "unknown session should fall back to first project (index 0)"
        );
    }

    /// Regression test for issue #773: the session name registered in the transport
    /// binding must match the name that TmuxManager uses for the actual tmux session.
    ///
    /// For internal tasks the raw task ID is "internal:42" (colon), but tmux sanitizes
    /// it to "internal-42" (hyphen).  The old code used `format!("orch-{repo}-{task_id}")`
    /// which embedded the unsanitized colon, so `bind()` / `register_session()` stored
    /// "orch-repo-internal:42" while the real session was "orch-repo-internal-42".
    ///
    /// After the fix both call sites use `tmux.session_name(repo, &task_id)` which
    /// applies the same sanitization, so the names match.
    #[test]
    fn session_name_used_for_internal_task_matches_tmux_manager() {
        let tmux = TmuxManager::new();
        let repo = "owner/repo";
        let task_id = "internal:42";

        // This is the name the runner/dispatch tick uses for the actual tmux session.
        let actual_session = tmux.session_name(repo, task_id);

        // Before the fix, channel_handler computed the session name as:
        //   format!("orch-{repo}-{task_id}") = "orch-owner/repo-internal:42"
        // which does NOT equal the sanitized name.
        let old_buggy_name = format!("orch-{repo}-{task_id}");
        assert_ne!(
            old_buggy_name, actual_session,
            "sanity check: old format should differ from sanitized name"
        );

        // After the fix, channel_handler uses tmux.session_name() — same as the runner.
        assert_eq!(actual_session, "orch-repo-internal-42");
    }
}

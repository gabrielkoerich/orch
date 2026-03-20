//! Channel message handlers — routes incoming channel messages to the appropriate action.
//!
//! Extracted from `engine/mod.rs` to keep the engine lifecycle code separate from
//! message dispatch logic. All handler functions here are stateless with respect to
//! the engine tick loop and operate only through the shared ref types passed in.

use std::sync::Arc;

use crate::backends::{ExternalId, Status};
use crate::channels::capture::CaptureService;
use crate::channels::routing::ChannelRouter;
use crate::channels::transport::{MessageRoute, Transport};
use crate::channels::{ChannelRegistry, IncomingMessage, OutgoingMessage};
use crate::engine::commands::{execute_command, parse_command};
use crate::engine::tasks::{CreateTaskRequest, Task, TaskType};
use crate::github::http::GhHttp;
use crate::tmux::TmuxManager;

use super::{EngineRef, PendingPick, PendingPicks};

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

/// Send an inline keyboard to a specific channel.
/// Returns the message ID of the keyboard message, or an empty string on failure.
pub(super) async fn send_channel_keyboard(
    channels: &Arc<ChannelRegistry>,
    channel_name: &str,
    thread_id: &str,
    topic_id: Option<&str>,
    text: &str,
    buttons: &[(String, String)],
) -> String {
    for ch in channels.iter() {
        if ch.name() == channel_name {
            match ch.send_keyboard(thread_id, topic_id, text, buttons).await {
                Ok(msg_id) => return msg_id,
                Err(e) => {
                    tracing::warn!(
                        channel = channel_name,
                        ?e,
                        "failed to send project picker keyboard"
                    );
                    return String::new();
                }
            }
        }
    }
    tracing::debug!(
        channel = channel_name,
        "channel not found for keyboard send"
    );
    String::new()
}

/// Forward a text message to an agent's tmux session via send-keys.
pub(super) async fn forward_to_tmux(transport: &Arc<Transport>, task_id: &str, text: &str) {
    if let Some(binding) = transport.get_binding(task_id).await {
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

/// Handle an incoming channel message by routing it to the appropriate action.
///
/// - `TaskSession`: slash command → execute on task; otherwise → forward to tmux
/// - `Command`: global commands like `/status`, `/stats`, `/subscribe`, `/unsubscribe`, `/stream`
/// - `NewTask`: create an internal task, bind thread, start output fanout
///
/// The `channel_router` resolves which project a message targets based on
/// its topic/channel ID. This enables per-project task creation and status views.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_channel_message(
    msg: IncomingMessage,
    transport: &Arc<Transport>,
    _tmux: &Arc<TmuxManager>,
    capture: &Arc<CaptureService>,
    channels: &Arc<ChannelRegistry>,
    engine_refs: &[EngineRef],
    channel_router: &Arc<ChannelRouter>,
    pending_picks: &PendingPicks,
) {
    use crate::channels::stream::fanout_output;

    // Resolve project from incoming message topic/channel
    let topic_id = msg.topic_id.as_deref().unwrap_or(&msg.thread_id);
    let resolved_repo = channel_router.resolve_project(&msg.channel, topic_id);
    let is_general = channel_router.is_general(&msg.channel, topic_id);
    let is_control = channel_router.is_control_channel(&msg.channel, topic_id);
    let msg_topic_id = msg.topic_id.clone();

    // ── Picker response: user tapped a project button ────────────────────────
    let is_picker_response = msg.body.starts_with("pick:")
        && (msg.metadata.get("callback_query_id").is_some()
            || msg.metadata.get("interaction_id").is_some());

    if is_picker_response {
        let repo = msg.body["pick:".len()..].to_string();
        let pick_key = format!("{}:{}", msg.channel, msg.thread_id);
        let pending = {
            let mut map = pending_picks.lock().unwrap_or_else(|e| e.into_inner());
            map.remove(&pick_key)
        };

        match pending {
            None => {
                send_channel_reply(
                    channels,
                    &msg.channel,
                    &msg.thread_id,
                    "No pending project selection (may have timed out).".to_string(),
                    msg_topic_id.as_deref(),
                )
                .await;
            }
            Some(pick) if pick.created_at.elapsed().as_secs() > 60 => {
                send_channel_reply(
                    channels,
                    &msg.channel,
                    &msg.thread_id,
                    "Project selection timed out (60s). Please send your message again."
                        .to_string(),
                    pick.msg_topic_id.as_deref(),
                )
                .await;
            }
            Some(pick) => {
                // Find the target engine ref for the selected repo
                if let Some((_, _, task_manager, _)) =
                    engine_refs.iter().find(|(r, _, _, _)| r == &repo)
                {
                    let title = if pick.original_body.chars().count() > 80 {
                        let truncated: String = pick.original_body.chars().take(80).collect();
                        format!("{}…", truncated)
                    } else {
                        pick.original_body.clone()
                    };
                    let req = crate::engine::tasks::CreateTaskRequest {
                        title,
                        body: pick.original_body.clone(),
                        task_type: crate::engine::tasks::TaskType::Internal,
                        labels: vec!["channel-created".to_string()],
                        source: msg.channel.clone(),
                        source_id: msg.thread_id.clone(),
                    };
                    match task_manager.create_task(req).await {
                        Ok(task) => {
                            let task_id = match &task {
                                Task::Internal(t) => format!("internal:{}", t.id),
                                Task::External(t) => t.id.0.clone(),
                            };
                            transport
                                .bind(
                                    &task_id,
                                    &format!("orch-{repo}-{task_id}"),
                                    &msg.channel,
                                    &msg.thread_id,
                                )
                                .await;
                            capture
                                .register_session(&task_id, &format!("orch-{repo}-{task_id}"))
                                .await;
                            let transport_clone = transport.clone();
                            let channels_clone = channels.clone();
                            let task_id_clone = task_id.clone();
                            tokio::spawn(async move {
                                fanout_output(task_id_clone, transport_clone, channels_clone).await;
                            });
                            let reply = format!(
                                "Task created: `{task_id}` in [{repo}] — I'll start working on it now."
                            );
                            send_channel_reply(
                                channels,
                                &msg.channel,
                                &msg.thread_id,
                                reply,
                                pick.msg_topic_id.as_deref(),
                            )
                            .await;
                        }
                        Err(e) => {
                            tracing::warn!(repo, err = %e, "failed to create task from picker selection");
                            send_channel_reply(
                                channels,
                                &msg.channel,
                                &msg.thread_id,
                                format!("Failed to create task: {e}"),
                                pick.msg_topic_id.as_deref(),
                            )
                            .await;
                        }
                    }
                } else {
                    send_channel_reply(
                        channels,
                        &msg.channel,
                        &msg.thread_id,
                        format!("Project `{repo}` not found. Please try again."),
                        pick.msg_topic_id.as_deref(),
                    )
                    .await;
                }
            }
        }
        return; // Picker response fully handled — skip normal routing
    }

    // Check control channel BEFORE task bindings so a control channel message
    // never accidentally falls through to task session routing.
    let route = if is_control {
        MessageRoute::ControlSession
    } else {
        transport.route(&msg).await
    };

    match route {
        MessageRoute::TaskSession { task_id } => {
            let body = msg.body.trim().to_string();
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();

            if body.starts_with('/') {
                // Parse slash command and execute it on the bound task
                if let Some(cmd) = parse_command(&body) {
                    if let Some((repo, backend, task_manager, store)) = engine_refs.first() {
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
                        let ext_id = ExternalId(task_id.clone());
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
                    forward_to_tmux(transport, &task_id, &body).await;
                }
            } else {
                // Regular message — forward to the agent's tmux session
                forward_to_tmux(transport, &task_id, &body).await;
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
                let reply = handle_stream_command(&cmd_str, &channel, &thread_id, transport).await;
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
            let session_id = format!("{}:{}", channel, topic_id);

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
                    Some(&thread_id),
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
            }
        }

        MessageRoute::NewTask => {
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();

            // Show interactive picker when in General with multiple projects configured
            if is_general && resolved_repo.is_none() && engine_refs.len() > 1 {
                let buttons: Vec<(String, String)> = engine_refs
                    .iter()
                    .map(|(repo, _, _, _)| {
                        // Use the short repo name (after the slash) as button label
                        let label = repo.rsplit('/').next().unwrap_or(repo.as_str()).to_string();
                        (label, format!("pick:{repo}"))
                    })
                    .collect();

                send_channel_keyboard(
                    channels,
                    &channel,
                    &thread_id,
                    msg_topic_id.as_deref(),
                    "Which project should I create this task in?",
                    &buttons,
                )
                .await;

                // Store the pending pick
                let pick_key = format!("{channel}:{thread_id}");
                let pick = PendingPick {
                    original_body: msg.body.clone(),
                    msg_topic_id: msg_topic_id.clone(),
                    created_at: std::time::Instant::now(),
                };
                {
                    let mut map = pending_picks.lock().unwrap_or_else(|e| e.into_inner());
                    map.insert(pick_key, pick);
                }
                return; // Wait for the user's button tap
            }

            // Single project or already resolved → create immediately
            let target_engine_ref = if let Some(repo) = resolved_repo {
                engine_refs.iter().find(|(r, _, _, _)| r == repo)
            } else {
                engine_refs.first()
            };

            if let Some((repo, _, task_manager, _)) = target_engine_ref {
                let title = if msg.body.chars().count() > 80 {
                    let truncated: String = msg.body.chars().take(80).collect();
                    format!("{}…", truncated)
                } else {
                    msg.body.clone()
                };
                let req = CreateTaskRequest {
                    title,
                    body: msg.body.clone(),
                    task_type: TaskType::Internal,
                    labels: vec!["channel-created".to_string()],
                    source: channel.clone(),
                    source_id: thread_id.clone(),
                };
                match task_manager.create_task(req).await {
                    Ok(task) => {
                        let task_id = match &task {
                            Task::Internal(t) => format!("internal:{}", t.id),
                            Task::External(t) => t.id.0.clone(),
                        };
                        // Bind the thread to the new task
                        transport
                            .bind(
                                &task_id,
                                &format!("orch-{repo}-{task_id}"),
                                &channel,
                                &thread_id,
                            )
                            .await;
                        // Register the session with CaptureService (graceful if no session yet)
                        capture
                            .register_session(&task_id, &format!("orch-{repo}-{task_id}"))
                            .await;
                        // Spawn output fanout for this task
                        let transport_clone = transport.clone();
                        let channels_clone = channels.clone();
                        let task_id_clone = task_id.clone();
                        tokio::spawn(async move {
                            fanout_output(task_id_clone, transport_clone, channels_clone).await;
                        });
                        let project_label = if resolved_repo.is_some() {
                            String::new()
                        } else {
                            format!(" in [{repo}]")
                        };
                        let reply = format!(
                            "Task created: `{task_id}`{project_label} — I'll start working on it now."
                        );
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
                        tracing::warn!(repo, err = %e, "failed to create task from channel message");
                        let reply = format!("Failed to create task: {e}");
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
            } else {
                tracing::warn!("no project configured, cannot create task from channel message");
            }
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

        match store.get_metrics_summary_24h_by_repo(repo).await {
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
        match store.subscribe_channel(channel, thread_id, project).await {
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
    transport: &Arc<Transport>,
) -> String {
    let parts: Vec<&str> = cmd_str.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[1].trim().is_empty() {
        return "Usage: `/stream <task_id>`".to_string();
    }
    let task_id = parts[1].trim();

    // Check if the task has an active binding (i.e. is running)
    if let Some(binding) = transport.get_binding(task_id).await {
        // Bind this channel/thread as an additional output target
        // The session name is retrieved from the existing binding
        transport
            .bind(task_id, &binding.tmux_session, channel, thread_id)
            .await;
        format!("Streaming output from task `{task_id}` to this channel.")
    } else {
        format!("Task `{task_id}` is not currently running or has no active session.")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pending_pick_key_format() {
        let channel = "telegram";
        let thread_id = "-100123456";
        let key = format!("{channel}:{thread_id}");
        assert_eq!(key, "telegram:-100123456");
    }

    #[test]
    fn pick_body_prefix() {
        let body = "pick:owner/orch";
        assert!(body.starts_with("pick:"));
        assert_eq!(&body[5..], "owner/orch");
    }

    #[test]
    fn picker_triggered_when_general_and_multiple_projects() {
        let is_general = true;
        let resolved_repo: Option<&str> = None;
        let project_count = 2usize;

        let should_show_picker = is_general && resolved_repo.is_none() && project_count > 1;
        assert!(should_show_picker);
    }

    #[test]
    fn picker_not_triggered_when_single_project() {
        let is_general = true;
        let resolved_repo: Option<&str> = None;
        let project_count = 1usize;

        let should_show_picker = is_general && resolved_repo.is_none() && project_count > 1;
        assert!(!should_show_picker);
    }

    #[test]
    fn picker_not_triggered_when_repo_resolved() {
        let is_general = true;
        let resolved_repo: Option<&str> = Some("owner/orch");
        let project_count = 2usize;

        let should_show_picker = is_general && resolved_repo.is_none() && project_count > 1;
        assert!(!should_show_picker);
    }
}

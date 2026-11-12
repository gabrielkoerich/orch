//! Project chat: an agent that knows one project, runs in its folder and acts on it alone. The
//! global `orch chat` keeps the control session, and plan mode has its own prompt in `plan.rs`

use std::fmt::Write;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::{App, ChatMessage, RepoQuery, authorized, internal, project_dir, unauthorized};
use crate::control;
use crate::security;
use crate::store::{Task, TaskStatus, TaskStore};

const HISTORY_TURNS: i64 = 20;
const MERGED_SHOWN: usize = 15;
const PROMPT: &str = include_str!("../../prompts/project_chat.md");

#[derive(Deserialize)]
pub(super) struct Say {
    repo: String,
    message: String,
}

#[derive(Serialize)]
struct Answer {
    role: &'static str,
    content: String,
    agent: String,
    model: String,
}

fn session(repo: &str) -> String {
    format!("project:{repo}")
}

fn task_line(t: &Task) -> String {
    let id = t
        .external_id
        .as_deref()
        .map_or_else(|| format!("T-{}", t.id), |n| format!("#{n}"));
    let mut line = format!("- {id} [{}] {}", t.status.as_str(), t.title);
    if let Some(agent) = &t.agent {
        let _ = write!(line, " ({agent})");
    }

    if t.status == TaskStatus::Blocked {
        let why = t.block_reason.as_deref().unwrap_or(&t.last_error);
        let why: String = why.chars().take(200).collect();
        let _ = write!(line, ". Blocked: {why}");
    }

    line
}

fn or_none(text: String) -> String {
    if text.trim().is_empty() {
        "(none)".to_string()
    } else {
        text
    }
}

/// The prompt with this project's state filled in, and nothing about any other project
async fn context(store: &TaskStore, repo: &str, dir: &Path) -> anyhow::Result<String> {
    let open = store.list_open(Some(repo)).await?;
    let now = chrono::Utc::now();
    let week = (now - chrono::Duration::days(7))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let until = (now + chrono::Duration::hours(1))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let merged = store
        .entered_between(TaskStatus::Done, &week, &until, Some(repo))
        .await?;
    let config = fs::read_to_string(dir.join(".orch.yml")).unwrap_or_default();
    let past = store
        .list_control_messages(&session(repo), None, HISTORY_TURNS)
        .await?;
    let history = past
        .iter()
        .map(|m| format!("{}: {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n\n");

    Ok(PROMPT
        .replace("{{REPO}}", repo)
        .replace("{{PATH}}", &dir.display().to_string())
        .replace(
            "{{TASKS}}",
            &or_none(open.iter().map(task_line).collect::<Vec<_>>().join("\n")),
        )
        .replace(
            "{{MERGED}}",
            &or_none(
                merged
                    .iter()
                    .take(MERGED_SHOWN)
                    .map(|(_, t)| task_line(t))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        )
        .replace("{{CONFIG}}", &or_none(security::redact(&config)))
        .replace("{{HISTORY}}", &or_none(history)))
}

pub(super) async fn history(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<RepoQuery>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    match app
        .store
        .list_control_messages(&session(&q.repo), None, 100)
        .await
    {
        Ok(messages) => Json(
            messages
                .into_iter()
                .map(|m| ChatMessage {
                    role: m.role,
                    content: security::redact(&m.content),
                    agent: m.agent,
                    model: m.model,
                    created_at: m.created_at,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(&e),
    }
}

pub(super) async fn send(
    State(app): State<App>,
    headers: HeaderMap,
    Json(req): Json<Say>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let Some(dir) = project_dir(&req.repo) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let store = Arc::clone(&app.store);
    // The turn finishes and is saved even if the page closes while the agent works
    let work = tokio::spawn(async move { turn(&store, &req.repo, &dir, &req.message).await });
    match work.await.unwrap_or_else(|e| Err(anyhow::anyhow!(e))) {
        Ok(answer) => Json(answer).into_response(),
        Err(e) => internal(&e),
    }
}

async fn turn(store: &TaskStore, repo: &str, dir: &Path, message: &str) -> anyhow::Result<Answer> {
    let prompt = context(store, repo, dir).await?;
    let id = session(repo);
    store
        .insert_control_message(
            &id, "user", "ui", None, message, None, None, None, None, None, None, None,
        )
        .await?;
    let agent = control::get_agent(store).await?;
    let model = control::get_model(store).await?;
    let result = control::invoke_agent_in(&agent, &model, &prompt, message, dir).await?;
    let reply = result.text.trim().to_string();
    store
        .insert_control_message(
            &id,
            "assistant",
            "ui",
            None,
            &reply,
            None,
            Some(&model),
            Some(&agent),
            None,
            None,
            None,
            None,
        )
        .await?;
    Ok(Answer {
        role: "assistant",
        content: security::redact(&reply),
        agent,
        model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_context_holds_only_this_project() {
        let store = TaskStore::open_memory().await.unwrap();
        store
            .create_internal("o/mine", "fix the login page", "", "test", "a", None)
            .await
            .unwrap();
        store
            .create_internal("o/other", "a secret roadmap item", "", "test", "b", None)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prompt = context(&store, "o/mine", dir.path()).await.unwrap();
        assert!(
            prompt.contains("fix the login page"),
            "this project's task is there"
        );
        assert!(
            !prompt.contains("a secret roadmap item"),
            "another project's task is not"
        );
        assert!(prompt.contains("o/mine"), "the prompt names the project");
    }
}

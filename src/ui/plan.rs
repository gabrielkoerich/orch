//! Plan mode: a chat with an agent that writes a project plan, then imports it as tasks the way
//! `orch plan <file>` does, with the plan committed to the repo so each issue links to it

use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::{App, RepoQuery, authorized, internal, project_dir, unauthorized};
use crate::cli::plan as cli_plan;
use crate::control;
use crate::github::http::GhHttp;
use crate::plan;
use crate::security;
use crate::store::TaskStore;

const HISTORY_TURNS: i64 = 20;
const PROMPT: &str = include_str!("../../prompts/plan_chat.md");

#[derive(Deserialize)]
pub(super) struct PlanMessage {
    repo: String,
    message: String,
}

#[derive(Deserialize)]
pub(super) struct PlanName {
    repo: String,
    #[serde(default)]
    name: String,
}

#[derive(Serialize)]
struct Turn {
    role: String,
    content: String,
    created_at: String,
}

#[derive(Serialize)]
struct PlanState {
    messages: Vec<Turn>,
    draft: String,
    name: String,
}

#[derive(Serialize)]
struct Reply {
    reply: String,
    draft: String,
}

#[derive(Serialize)]
struct Preview {
    path: String,
    outline: String,
    report: String,
}

fn session(repo: &str) -> String {
    format!("plan:{repo}")
}

fn draft_key(repo: &str) -> String {
    format!("plan:draft:{repo}")
}

fn roadmap_key(repo: &str) -> String {
    format!("plan:roadmap:{repo}")
}

/// The draft between `<plan>` tags, and the reply with the draft taken out
fn split_reply(text: &str) -> (String, Option<String>) {
    let Some(start) = text.find("<plan>") else {
        return (text.trim().to_string(), None);
    };
    let after = &text[start + "<plan>".len()..];
    let Some(end) = after.find("</plan>") else {
        return (text.trim().to_string(), None);
    };
    let draft = after[..end].trim().to_string();
    let rest = format!("{}{}", &text[..start], &after[end + "</plan>".len()..]);
    (rest.trim().to_string(), Some(draft))
}

/// A file name from the plan's title, `plan` when it has none
fn plan_name(draft: &str, asked: &str) -> String {
    let source = if asked.trim().is_empty() {
        draft
            .lines()
            .find_map(|l| l.strip_prefix("# "))
            .unwrap_or("plan")
    } else {
        asked
    };
    let slug: String = source
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "plan".to_string()
    } else {
        slug.chars().take(60).collect()
    }
}

fn plan_path(name: &str) -> String {
    format!("docs/plans/{name}.md")
}

pub(super) async fn state(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<RepoQuery>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let messages = app
        .store
        .list_control_messages(&session(&q.repo), None, 200)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|m| Turn {
            role: m.role,
            content: security::redact(&m.content),
            created_at: m.created_at,
        })
        .collect();
    let draft = app
        .store
        .kv_get(&draft_key(&q.repo))
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let name = plan_name(&draft, "");
    Json(PlanState {
        messages,
        draft,
        name,
    })
    .into_response()
}

/// One turn with the planning agent. It gets the draft and the recent turns each time, so any agent
/// works, with or without its own session memory
pub(super) async fn message(
    State(app): State<App>,
    headers: HeaderMap,
    Json(req): Json<PlanMessage>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    if project_dir(&req.repo).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let store = Arc::clone(&app.store);
    let work = tokio::spawn(async move { turn(&store, &req.repo, &req.message).await });
    match work.await.unwrap_or_else(|e| Err(anyhow::anyhow!(e))) {
        Ok(reply) => Json(reply).into_response(),
        Err(e) => internal(&e),
    }
}

async fn turn(store: &TaskStore, repo: &str, message: &str) -> anyhow::Result<Reply> {
    let id = session(repo);
    let draft = store.kv_get(&draft_key(repo)).await?.unwrap_or_default();
    let past = store
        .list_control_messages(&id, None, HISTORY_TURNS)
        .await?;
    let history = past
        .iter()
        .map(|m| format!("{}: {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n\n");
    store
        .insert_control_message(
            &id, "user", "ui", None, message, None, None, None, None, None, None, None,
        )
        .await?;

    let context = PROMPT
        .replace("{{REPO}}", repo)
        .replace(
            "{{DRAFT}}",
            if draft.trim().is_empty() {
                "(none yet)"
            } else {
                &draft
            },
        )
        .replace(
            "{{HISTORY}}",
            if history.is_empty() {
                "(none yet)"
            } else {
                &history
            },
        );
    let agent = control::get_agent(store).await?;
    let model = control::get_model(store).await?;
    let result = control::invoke_agent(&agent, &model, &context, message).await?;

    let (reply, new_draft) = split_reply(&result.text);
    let draft = match new_draft {
        Some(d) => {
            store.kv_set(&draft_key(repo), &d).await?;
            // A new draft makes the last roadmap stale
            store.kv_delete(&roadmap_key(repo)).await?;
            d
        }
        None => draft,
    };
    let reply = if reply.is_empty() {
        "Updated the plan.".to_string()
    } else {
        reply
    };
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
    Ok(Reply {
        reply: security::redact(&reply),
        draft,
    })
}

/// Turn the draft into a roadmap and show what an import would create, without creating anything
pub(super) async fn preview(
    State(app): State<App>,
    headers: HeaderMap,
    Json(req): Json<PlanName>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let store = Arc::clone(&app.store);
    let work = tokio::spawn(async move { build_preview(&store, &req).await });
    match work.await.unwrap_or_else(|e| Err(anyhow::anyhow!(e))) {
        Ok(p) => Json(p).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")).into_response(),
    }
}

async fn roadmap_for(store: &TaskStore, repo: &str, draft: &str) -> anyhow::Result<String> {
    if let Some(saved) = store.kv_get(&roadmap_key(repo)).await? {
        return Ok(saved);
    }

    let (markdown, _) = cli_plan::draft_roadmap_with(store, draft, None).await?;
    store.kv_set(&roadmap_key(repo), &markdown).await?;
    Ok(markdown)
}

async fn build_preview(store: &TaskStore, req: &PlanName) -> anyhow::Result<Preview> {
    let draft = store
        .kv_get(&draft_key(&req.repo))
        .await?
        .unwrap_or_default();
    if draft.trim().is_empty() {
        anyhow::bail!("there is no plan yet, ask the agent to write one");
    }

    let name = plan_name(&draft, &req.name);
    let roadmap_md = roadmap_for(store, &req.repo, &draft).await?;
    let roadmap_path = cli_plan::roadmap_path(Path::new(&plan_path(&name)));
    let rel = roadmap_path.to_string_lossy().to_string();
    let (roadmap, _) = plan::parse_roadmap_for(&roadmap_md, None)?;
    let outcome = cli_plan::import_roadmap(&req.repo, &roadmap_md, &rel, None, false, true).await?;
    Ok(Preview {
        path: plan_path(&name),
        outline: plan::render_outline(&roadmap),
        report: outcome.report,
    })
}

/// Commit the plan and its roadmap to the default branch, then create the issues
pub(super) async fn import(
    State(app): State<App>,
    headers: HeaderMap,
    Json(req): Json<PlanName>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let store = Arc::clone(&app.store);
    let work = tokio::spawn(async move { run_import(&store, &req).await });
    match work.await.unwrap_or_else(|e| Err(anyhow::anyhow!(e))) {
        Ok(report) => Json(serde_json::json!({ "report": report })).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")).into_response(),
    }
}

async fn run_import(store: &TaskStore, req: &PlanName) -> anyhow::Result<String> {
    let draft = store
        .kv_get(&draft_key(&req.repo))
        .await?
        .unwrap_or_default();
    if draft.trim().is_empty() {
        anyhow::bail!("there is no plan to import");
    }

    let name = plan_name(&draft, &req.name);
    let path = plan_path(&name);
    let roadmap_md = roadmap_for(store, &req.repo, &draft).await?;
    let roadmap_rel = cli_plan::roadmap_path(Path::new(&path))
        .to_string_lossy()
        .to_string();

    let gh = GhHttp::new()?;
    let branch = gh.get_default_branch(&req.repo).await?;
    gh.put_file(
        &req.repo,
        &path,
        &branch,
        &format!("Add the {name} plan"),
        &with_newline(&draft),
    )
    .await?;
    gh.put_file(
        &req.repo,
        &roadmap_rel,
        &branch,
        &format!("Add the {name} roadmap"),
        &with_newline(&roadmap_md),
    )
    .await?;

    let outcome =
        cli_plan::import_roadmap(&req.repo, &roadmap_md, &roadmap_rel, None, false, false).await?;
    store
        .insert_control_message(
            &session(&req.repo),
            "assistant",
            "ui",
            None,
            &format!(
                "Imported. The plan is in {path} on {branch}.\n\n{}",
                outcome.report
            ),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await?;
    Ok(outcome.report)
}

fn with_newline(text: &str) -> String {
    if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    }
}

/// Start over: the conversation and the draft go, imported issues stay
pub(super) async fn clear(
    State(app): State<App>,
    headers: HeaderMap,
    Json(req): Json<PlanName>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let id = session(&req.repo);
    let done = async {
        app.store.delete_control_session(&id).await?;
        app.store.kv_delete(&draft_key(&req.repo)).await?;
        app.store.kv_delete(&roadmap_key(&req.repo)).await
    };
    match done.await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_is_taken_out_of_the_reply() {
        let (reply, draft) =
            split_reply("Here it is.\n<plan>\n# Ship it\n\nGoal.\n</plan>\nWhat about auth?");
        assert_eq!(
            draft.as_deref(),
            Some("# Ship it\n\nGoal."),
            "the draft between the tags"
        );
        assert_eq!(
            reply, "Here it is.\n\nWhat about auth?",
            "the reply without the draft"
        );
    }

    #[test]
    fn a_reply_without_a_plan_keeps_the_draft() {
        let (reply, draft) = split_reply("Who is it for?");
        assert!(draft.is_none(), "no new draft");
        assert_eq!(reply, "Who is it for?", "the question stays");
    }

    #[test]
    fn the_file_name_comes_from_the_title() {
        assert_eq!(
            plan_name("# Billing v2: Stripe!\n\ntext", ""),
            "billing-v2-stripe",
            "slug of the title"
        );
        assert_eq!(plan_name("no title", ""), "plan", "a plan with no title");
        assert_eq!(
            plan_name("# A", "My Launch"),
            "my-launch",
            "the name asked for wins"
        );
    }
}

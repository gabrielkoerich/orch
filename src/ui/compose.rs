//! New tasks from the web UI. With GitHub the task is an issue, created held with `no-agent`
//! until its dependencies are linked, so the engine never routes it ahead of its blockers

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::{App, authorized, project_dir, unauthorized};
use crate::engine;
use crate::github::http::GhHttp;
use crate::github::token;
use crate::plan::NO_AGENT_LABEL;
use crate::store::{DepSource, NewDependency, Task, TaskStore, UpsertExternal};

#[derive(Deserialize)]
pub(super) struct NewTask {
    repo: String,
    title: String,
    #[serde(default)]
    body: String,
    /// None lets the router pick, "none" keeps the task for a person, a name skips the router
    agent: Option<String>,
    #[serde(default)]
    depends_on: Vec<i64>,
}

#[derive(Serialize)]
struct Created {
    id: i64,
    number: Option<String>,
}

pub(super) async fn agents(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    Json(engine::configured_agents()).into_response()
}

pub(super) async fn create(
    State(app): State<App>,
    headers: HeaderMap,
    Json(req): Json<NewTask>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    if req.title.trim().is_empty() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "a task needs a title").into_response();
    }

    if project_dir(&req.repo).is_none() {
        return (StatusCode::NOT_FOUND, "orch does not run that project").into_response();
    }

    // A half-made task must still be released or reported, so a closed page does not stop it
    let store = Arc::clone(&app.store);
    let work = tokio::spawn(async move { create_task(&store, req).await });
    match work.await.unwrap_or_else(|e| Err(anyhow::anyhow!(e))) {
        Ok(created) => Json(created).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")).into_response(),
    }
}

/// The tasks a new one waits on, all in its project
async fn blockers(store: &TaskStore, repo: &str, ids: &[i64]) -> anyhow::Result<Vec<Task>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let task = store.get(*id).await?;
        if task.repo != repo {
            anyhow::bail!(
                "task {id} is in {}, a dependency must be in the same project",
                task.repo
            );
        }
        out.push(task);
    }
    Ok(out)
}

fn issue_number(task: &Task) -> Option<i64> {
    task.external_id.as_deref()?.parse().ok()
}

fn labels_for(agent: Option<&str>, hold: bool) -> Vec<String> {
    let mut labels = Vec::new();
    match agent {
        Some("none") => labels.push(NO_AGENT_LABEL.to_string()),
        Some(name) => {
            labels.push(format!("agent:{name}"));
            if hold {
                labels.push(NO_AGENT_LABEL.to_string());
            }
        }
        None if hold => labels.push(NO_AGENT_LABEL.to_string()),
        None => {}
    }
    labels
}

async fn create_task(store: &TaskStore, req: NewTask) -> anyhow::Result<Created> {
    let deps = blockers(store, &req.repo, &req.depends_on).await?;
    let agent = req
        .agent
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty());
    let github = token::shared().get_token().await.ok().flatten().is_some();
    if !github {
        return create_internal(store, &req, agent, &deps).await;
    }

    // GitHub links issues, so a dependency on an internal task is refused before anything exists
    if let Some(internal) = deps.iter().find(|d| issue_number(d).is_none()) {
        anyhow::bail!(
            "task {} \"{}\" has no GitHub issue to depend on",
            internal.id,
            internal.title
        );
    }

    // Held while its blockers are linked, a person's own task keeps the label for good
    let hold = !deps.is_empty();
    let labels = labels_for(agent, hold);
    let gh = GhHttp::new()?;
    let issue = gh
        .create_issue(&req.repo, req.title.trim(), &req.body, &labels)
        .await?;
    let number = issue.number.to_string();
    let id = store
        .upsert_external(&UpsertExternal {
            repo: &req.repo,
            ext_id: &number,
            title: req.title.trim(),
            body: &req.body,
            author: &issue.user.login,
            url: &issue.html_url,
            labels: &labels,
            origin: "github",
        })
        .await?;

    let rows: Vec<NewDependency> = deps
        .iter()
        .filter_map(|d| {
            Some(NewDependency {
                repo: req.repo.clone(),
                number: issue_number(d)?,
                source: DepSource::Cli,
            })
        })
        .collect();
    store.replace_dependencies(id, &rows).await?;

    for dep in &deps {
        let Some(dep_number) = dep.external_id.as_deref() else {
            continue;
        };
        let blocker = gh.get_issue(&req.repo, dep_number).await?;
        let blocker_id = i64::try_from(blocker.id)?;
        gh.add_blocked_by(&req.repo, issue.number, blocker_id)
            .await
            .map_err(|e| {
                e.context(format!(
                    "created #{number}, but linking #{dep_number} failed, so it stays held with no-agent"
                ))
            })?;
    }

    if hold && agent != Some("none") {
        gh.remove_label(&req.repo, &number, NO_AGENT_LABEL).await?;
        // The engine reads labels from the store, so the release there is what lets it route now
        let released: Vec<String> = labels.into_iter().filter(|l| l != NO_AGENT_LABEL).collect();
        store
            .set_fields(id, &[("labels", serde_json::json!(released))])
            .await?;
    }

    Ok(Created {
        id,
        number: Some(number),
    })
}

/// No GitHub sign-in: an orch task in the store alone, its dependencies written with it
async fn create_internal(
    store: &TaskStore,
    req: &NewTask,
    agent: Option<&str>,
    deps: &[Task],
) -> anyhow::Result<Created> {
    let rows: Vec<NewDependency> = deps
        .iter()
        .map(|d| {
            issue_number(d)
                .map(|number| NewDependency {
                    repo: req.repo.clone(),
                    number,
                    source: DepSource::Cli,
                })
                .ok_or_else(|| anyhow::anyhow!("task {} has no issue to depend on", d.id))
        })
        .collect::<anyhow::Result<_>>()?;
    // Born held until the dependency rows exist, the same order as with GitHub
    let labels = labels_for(agent, !rows.is_empty());
    let id = store
        .create_internal_labeled(&req.repo, req.title.trim(), &req.body, "ui", labels.clone())
        .await?;
    store.replace_dependencies(id, &rows).await?;
    if !rows.is_empty() && agent != Some("none") {
        let released: Vec<String> = labels.into_iter().filter(|l| l != NO_AGENT_LABEL).collect();
        store
            .set_fields(id, &[("labels", serde_json::json!(released))])
            .await?;
    }

    Ok(Created { id, number: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_hold_a_task_with_dependencies() {
        assert_eq!(
            labels_for(None, true),
            vec!["no-agent"],
            "held for the links"
        );
        assert!(labels_for(None, false).is_empty(), "nothing to hold");
        assert_eq!(
            labels_for(Some("codex"), true),
            vec!["agent:codex", "no-agent"],
            "the chosen agent and the hold"
        );
        assert_eq!(
            labels_for(Some("none"), false),
            vec!["no-agent"],
            "a person's task is held for good"
        );
    }

    #[tokio::test]
    async fn a_task_without_an_issue_has_no_number_to_depend_on() {
        let store = TaskStore::open_memory().await.unwrap();
        let id = store
            .create_internal("o/r", "local only", "", "test", "t", None)
            .await
            .unwrap();
        let task = store.get(id).await.unwrap();
        assert_eq!(issue_number(&task), None, "an internal task has no issue");
        store.update_external_id(id, "12").await.unwrap();
        let task = store.get(id).await.unwrap();
        assert_eq!(issue_number(&task), Some(12), "an issue number");
    }

    #[tokio::test]
    async fn an_internal_task_waits_on_its_dependency() {
        let store = TaskStore::open_memory().await.unwrap();
        let blocker = store
            .create_internal("o/r", "first", "", "test", "t", None)
            .await
            .unwrap();
        store.update_external_id(blocker, "7").await.unwrap();
        let req = NewTask {
            repo: "o/r".into(),
            title: "second".into(),
            body: String::new(),
            agent: None,
            depends_on: vec![blocker],
        };
        let deps = blockers(&store, "o/r", &req.depends_on).await.unwrap();
        let made = create_internal(&store, &req, None, &deps).await.unwrap();
        let rows = store.list_dependencies(made.id).await.unwrap();
        assert_eq!(rows.len(), 1, "the dependency is written");
        let task = store.get(made.id).await.unwrap();
        assert!(
            !task.labels.iter().any(|l| l == NO_AGENT_LABEL),
            "released once its dependency exists"
        );
    }
}

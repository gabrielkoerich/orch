//! The one way a CLI command turns a task id into a task, and into the state directory that
//! holds its output. Every id form `orch task inspect` accepts works everywhere

use crate::config;
use crate::store::{self, CliTaskRef, TaskStore};
use std::path::{MAIN_SEPARATOR_STR, Path, PathBuf};

/// Resolve a task id against the store, using the project of the working directory
pub async fn resolve(store: &TaskStore, id: &str) -> anyhow::Result<CliTaskRef> {
    resolve_in(store, id, current_repo().as_deref()).await
}

/// Resolve a task id to its store row id
pub async fn resolve_store_id(store: &TaskStore, id: &str) -> anyhow::Result<i64> {
    resolve(store, id)
        .await?
        .store_id
        .ok_or_else(|| anyhow::anyhow!("task not found in store: {id}"))
}

/// `resolve` against an explicit project, so tests do not read the operator's config
pub async fn resolve_in(
    store: &TaskStore,
    id: &str,
    current_repo: Option<&str>,
) -> anyhow::Result<CliTaskRef> {
    let spec = expand_repo(store, &normalize(id), current_repo).await?;
    if let Some(task) = store.resolve_cli_id(&spec, current_repo).await? {
        return Ok(task);
    }
    match store::parse_cli_task_spec(&spec)? {
        store::CliTaskSpec::Issue { repo, number } => {
            Ok(store::CliTaskRef::unsynced_issue(repo, number))
        }
        store::CliTaskSpec::Bare(number) => {
            let repo = current_repo
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("task not found: {id}"))?;
            Ok(store::CliTaskRef::unsynced_issue(repo, number))
        }
        store::CliTaskSpec::Internal(_) => anyhow::bail!("internal task '{id}' not found"),
    }
}

/// `~/.orch/state/<owner>/<repo>/tasks/<id>` for a resolved task. Does not create it
pub fn state_dir(task: &CliTaskRef) -> anyhow::Result<PathBuf> {
    Ok(dirs_under(&crate::home::state_dir()?, task).1)
}

/// A task's own state dir and its review agent's
pub fn state_dirs(task: &CliTaskRef) -> anyhow::Result<Vec<PathBuf>> {
    let (review, own) = dirs_under(&crate::home::state_dir()?, task);
    Ok(vec![review, own])
}

/// The review dir and the task's own dir under a state root. The runner names a task dir after
/// its external id, which `CliTaskRef` fills with `internal:<store id>` for an internal task
fn dirs_under(state: &Path, task: &CliTaskRef) -> (PathBuf, PathBuf) {
    let tasks = state
        .join(task.repo.replace('/', MAIN_SEPARATOR_STR))
        .join("tasks");
    let name = &task.external_id;
    (tasks.join(format!("{name}-review")), tasks.join(name))
}

/// `repo#N` names a project without its owner. Expand it against the current project, then the
/// repos the store knows, so the short form works wherever `owner/repo#N` does
async fn expand_repo(
    store: &TaskStore,
    spec: &str,
    current_repo: Option<&str>,
) -> anyhow::Result<String> {
    let Some((name, number)) = spec.rsplit_once('#') else {
        return Ok(spec.to_string());
    };
    if name.is_empty() || name.contains('/') || number.parse::<i64>().is_err() {
        return Ok(spec.to_string());
    }

    if let Some(repo) = current_repo
        && repo.rsplit_once('/').is_some_and(|(_, n)| n == name)
    {
        return Ok(format!("{repo}#{number}"));
    }

    let owners: Vec<String> = store
        .distinct_repos()
        .await?
        .into_iter()
        .filter(|repo| repo.rsplit_once('/').is_some_and(|(_, n)| n == name))
        .collect();
    match owners.as_slice() {
        [repo] => Ok(format!("{repo}#{number}")),
        // Unknown name: leave it for `parse_cli_task_spec` to reject with the forms it accepts
        [] => Ok(spec.to_string()),
        many => anyhow::bail!(
            "repo '{name}' matches {}, use owner/repo#{number} to pick one",
            many.join(" and ")
        ),
    }
}

/// Session names cannot hold a `:`, so `internal-8` names the same task as `internal:8`
fn normalize(id: &str) -> String {
    let id = id.trim();
    match id.strip_prefix("internal-") {
        Some(rest) if rest.chars().all(|c| c.is_ascii_digit()) => format!("internal:{rest}"),
        _ => id.to_string(),
    }
}

fn current_repo() -> Option<String> {
    config::get_current_repo()
        .ok()
        .filter(|repo| !repo.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewTask, TaskStore};

    async fn store_with_tasks() -> (TaskStore, i64) {
        let store = TaskStore::open_memory().await.unwrap();
        store
            .create(&NewTask {
                external_id: Some("5".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "GitHub task 5".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let internal = store
            .create_internal("owner/other", "Internal task", "body", "job", "j:1", None)
            .await
            .unwrap();
        (store, internal)
    }

    #[tokio::test]
    async fn resolve_accepts_every_id_form() {
        let (store, internal) = store_with_tasks().await;
        let forms = [
            ("owner/repo#5", Some("owner/other")),
            ("repo#5", Some("owner/other")),
            ("5", Some("owner/repo")),
            ("#5", Some("owner/repo")),
        ];
        for (id, repo) in forms {
            let task = resolve_in(&store, id, repo).await.unwrap();
            assert_eq!(task.display_spec(), "owner/repo#5", "{id}");
        }

        for id in [
            format!("internal:{internal}"),
            format!("internal-{internal}"),
        ] {
            let task = resolve_in(&store, &id, None).await.unwrap();
            assert_eq!(task.display_spec(), format!("internal:{internal}"), "{id}");
        }
    }

    #[tokio::test]
    async fn bare_id_in_a_project_does_not_clash_with_another_project() {
        let store = TaskStore::open_memory().await.unwrap();
        store
            .create(&NewTask {
                external_id: Some("5".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "GitHub task 5".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        // Internal task 5 in another project
        for _ in 0..5 {
            store
                .create_internal("owner/other", "Internal", "body", "job", "j:1", None)
                .await
                .unwrap();
        }

        let task = resolve_in(&store, "5", Some("owner/repo")).await.unwrap();
        assert_eq!(task.display_spec(), "owner/repo#5");

        // Outside a project the clash names both forms
        let err = resolve_in(&store, "5", None).await.unwrap_err().to_string();
        assert!(
            err.contains("Use owner/repo#5 for the GitHub task"),
            "{err}"
        );
        assert!(err.contains("or internal:5 for the internal one"), "{err}");
    }

    #[tokio::test]
    async fn state_dirs_follow_the_resolved_task() {
        let (store, internal) = store_with_tasks().await;
        let state = Path::new("/state");

        let github = resolve_in(&store, "owner/repo#5", None).await.unwrap();
        assert_eq!(
            dirs_under(state, &github),
            (
                PathBuf::from("/state/owner/repo/tasks/5-review"),
                PathBuf::from("/state/owner/repo/tasks/5"),
            )
        );

        let task = resolve_in(&store, &format!("internal:{internal}"), None)
            .await
            .unwrap();
        assert_eq!(
            dirs_under(state, &task).1,
            PathBuf::from(format!("/state/owner/other/tasks/internal:{internal}"))
        );
    }

    #[tokio::test]
    async fn bare_repo_name_needs_an_owner_only_when_two_owners_share_it() {
        let (store, _) = store_with_tasks().await;
        for repo in ["other/repo", "owner/repo"] {
            store
                .create(&NewTask {
                    external_id: Some("9".to_string()),
                    repo: repo.to_string(),
                    origin: "github".to_string(),
                    title: "Task 9".to_string(),
                    ..Default::default()
                })
                .await
                .unwrap();
        }

        let err = resolve_in(&store, "repo#9", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("owner/repo#9"), "{err}");

        let task = resolve_in(&store, "repo#9", Some("owner/repo"))
            .await
            .unwrap();
        assert_eq!(task.display_spec(), "owner/repo#9");

        // A repo name no project uses is reported as an invalid id, not as a clash
        let err = resolve_in(&store, "nosuch#9", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid task id"), "{err}");
    }

    #[tokio::test]
    async fn unknown_bare_id_outside_a_project_is_an_error() {
        let (store, _) = store_with_tasks().await;
        assert!(resolve_in(&store, "999", None).await.is_err());
        assert!(resolve_in(&store, "internal:999", None).await.is_err());
        let task = resolve_in(&store, "999", Some("owner/repo")).await.unwrap();
        assert_eq!(task.display_spec(), "owner/repo#999");
    }
}

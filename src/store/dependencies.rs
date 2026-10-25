//! Issue dependency rows and the claim gate that keeps a waiting task from starting.
//!
//! A dependency is satisfied when the blocking task is `done`, or its issue is
//! closed as completed. `not_planned` and `duplicate` are not done. `blocked`
//! is not done. Only `new` and `routed` are moved to `waiting`. Other statuses
//! stay where they are.

use super::{TaskStatus, TaskStore};
use anyhow::Context;
use sqlx::SqliteConnection;
use std::collections::HashMap;

/// Returned by routing and dispatch when a task still has an open dependency.
///
/// Callers log this and leave the task alone. It does not count as a routing failure
#[derive(Debug, thiserror::Error)]
#[error("waiting on {}", .blockers.join(", "))]
pub struct WaitingOnDependency {
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepSource {
    Github,
    Body,
    Cli,
}

impl DepSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Body => "body",
            Self::Cli => "cli",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "github" => Some(Self::Github),
            "body" => Some(Self::Body),
            "cli" => Some(Self::Cli),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewDependency {
    pub repo: String,
    pub number: i64,
    pub source: DepSource,
}

#[derive(Debug, Clone)]
pub struct TaskDependency {
    pub task_id: i64,
    pub depends_on_task_id: Option<i64>,
    pub depends_on_repo: String,
    pub depends_on_number: i64,
    pub source: String,
    pub blocker_status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SchedulableTask {
    pub id: i64,
    pub repo: String,
    pub status: String,
    pub external_id: Option<String>,
    pub body: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct DepEdge {
    pub task_id: i64,
    pub task_repo: String,
    pub task_external_id: Option<String>,
    pub task_title: String,
    pub depends_on_repo: String,
    pub depends_on_number: i64,
}

#[derive(Debug, Clone)]
struct BlockerView {
    label: String,
    satisfied: bool,
    failure: Option<&'static str>,
}

/// Map a GitHub issue `state` and `state_reason` onto the values stored in KV.
///
/// A classic close with no reason counts as completed. An open issue counts as open
pub fn normalize_close_reason(state: &str, state_reason: Option<&str>) -> &'static str {
    if !state.eq_ignore_ascii_case("closed") {
        return "open";
    }
    match state_reason.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "not_planned" => "not_planned",
        "duplicate" => "duplicate",
        "reopened" => "open",
        _ => "completed",
    }
}

pub fn close_reason_key(repo: &str, number: i64) -> String {
    format!("issue_close_reason:{repo}#{number}")
}

pub fn dep_label(task_repo: &str, dep_repo: &str, number: i64) -> String {
    if task_repo == dep_repo {
        format!("#{number}")
    } else {
        format!("{dep_repo}#{number}")
    }
}

/// `not_planned` and `duplicate` are never done. A completed close is done even
/// when orch has no task row. A `done` task is done unless the close reason says otherwise
pub fn blocker_is_satisfied(task_status: Option<&str>, close_reason: Option<&str>) -> bool {
    let reason = close_reason.unwrap_or("");
    if reason == "not_planned" || reason == "duplicate" {
        return false;
    }
    if reason == "completed" {
        return true;
    }
    task_status == Some("done")
}

/// `not_planned` and a `blocked` task each get one notice. `duplicate` does not
pub fn blocker_failure_kind(
    task_status: Option<&str>,
    close_reason: Option<&str>,
) -> Option<&'static str> {
    if close_reason == Some("not_planned") {
        return Some("not_planned");
    }
    if task_status == Some("blocked") && !blocker_is_satisfied(task_status, close_reason) {
        return Some("blocked");
    }
    None
}

impl TaskStore {
    /// Replace one task's dependency rows in a single transaction.
    pub async fn replace_dependencies(
        &self,
        task_id: i64,
        rows: &[NewDependency],
    ) -> anyhow::Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin dependency replace")?;
        sqlx::query("DELETE FROM task_dependencies WHERE task_id = ?")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        for row in rows {
            if row.number <= 0 || row.repo.is_empty() {
                continue;
            }
            sqlx::query(
                "INSERT INTO task_dependencies
                 (task_id, depends_on_repo, depends_on_number, source)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(task_id)
            .bind(&row.repo)
            .bind(row.number)
            .bind(row.source.as_str())
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE task_dependencies
             SET depends_on_task_id = (
                 SELECT id FROM tasks
                 WHERE tasks.repo = task_dependencies.depends_on_repo
                   AND tasks.external_id = CAST(task_dependencies.depends_on_number AS TEXT)
                 LIMIT 1
             )
             WHERE task_id = ?",
        )
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Point dependency rows at a task orch now knows.
    pub async fn link_known_task(
        &self,
        task_id: i64,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE task_dependencies
             SET depends_on_task_id = ?
             WHERE depends_on_repo = ? AND depends_on_number = ?",
        )
        .bind(task_id)
        .bind(repo)
        .bind(number)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_dependencies(&self, task_id: i64) -> anyhow::Result<Vec<TaskDependency>> {
        let rows = sqlx::query(
            "SELECT d.task_id, d.depends_on_task_id, d.depends_on_repo, d.depends_on_number, d.source,
                    t.status AS blocker_status
             FROM task_dependencies d
             LEFT JOIN tasks t
               ON t.repo = d.depends_on_repo
              AND t.external_id = CAST(d.depends_on_number AS TEXT)
             WHERE d.task_id = ?
             ORDER BY d.depends_on_repo, d.depends_on_number",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(TaskDependency {
                task_id: row.try_get("task_id")?,
                depends_on_task_id: row.try_get("depends_on_task_id")?,
                depends_on_repo: row.try_get("depends_on_repo")?,
                depends_on_number: row.try_get("depends_on_number")?,
                source: row.try_get("source")?,
                blocker_status: row.try_get("blocker_status")?,
            });
        }
        Ok(out)
    }

    pub async fn list_dependents(&self, repo: &str, number: i64) -> anyhow::Result<Vec<TaskDependency>> {
        let rows = sqlx::query(
            "SELECT d.task_id, d.depends_on_task_id, d.depends_on_repo, d.depends_on_number, d.source,
                    t.status AS blocker_status
             FROM task_dependencies d
             JOIN tasks t ON t.id = d.task_id
             WHERE d.depends_on_repo = ? AND d.depends_on_number = ?
             ORDER BY d.task_id",
        )
        .bind(repo)
        .bind(number)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(TaskDependency {
                task_id: row.try_get("task_id")?,
                depends_on_task_id: row.try_get("depends_on_task_id")?,
                depends_on_repo: row.try_get("depends_on_repo")?,
                depends_on_number: row.try_get("depends_on_number")?,
                source: row.try_get("source")?,
                blocker_status: row.try_get("blocker_status")?,
            });
        }
        Ok(out)
    }

    pub async fn list_schedulable(&self) -> anyhow::Result<Vec<SchedulableTask>> {
        let rows = sqlx::query(
            "SELECT id, repo, status, external_id, body, title
             FROM tasks
             WHERE status IN ('new', 'routed', 'waiting')
             ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(SchedulableTask {
                id: row.try_get("id")?,
                repo: row.try_get("repo")?,
                status: row.try_get("status")?,
                external_id: row.try_get("external_id")?,
                body: row.try_get("body")?,
                title: row.try_get("title")?,
            });
        }
        Ok(out)
    }

    pub async fn list_dependency_edges(&self) -> anyhow::Result<Vec<DepEdge>> {
        let rows = sqlx::query(
            "SELECT d.task_id, t.repo AS task_repo, t.external_id AS task_external_id, t.title AS task_title,
                    d.depends_on_repo, d.depends_on_number
             FROM task_dependencies d
             JOIN tasks t ON t.id = d.task_id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(DepEdge {
                task_id: row.try_get("task_id")?,
                task_repo: row.try_get("task_repo")?,
                task_external_id: row.try_get("task_external_id")?,
                task_title: row.try_get("task_title")?,
                depends_on_repo: row.try_get("depends_on_repo")?,
                depends_on_number: row.try_get("depends_on_number")?,
            });
        }
        Ok(out)
    }

    pub async fn close_reasons(&self) -> anyhow::Result<HashMap<String, String>> {
        let rows = sqlx::query(
            "SELECT key, value FROM kv WHERE key LIKE 'issue_close_reason:%'",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = HashMap::new();
        for row in rows {
            let key: String = row.try_get("key")?;
            let value: String = row.try_get("value")?;
            out.insert(key, value);
        }
        Ok(out)
    }

    /// Labels of unmet blockers, keyed by the waiting task id.
    pub async fn unmet_labels_by_task(&self) -> anyhow::Result<HashMap<i64, Vec<String>>> {
        let mut conn = self.pool.acquire().await?;
        let ids: Vec<i64> = sqlx::query("SELECT DISTINCT task_id FROM task_dependencies")
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|row| row.try_get("task_id"))
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = HashMap::new();
        for task_id in ids {
            let repo: String = sqlx::query("SELECT repo FROM tasks WHERE id = ?")
                .bind(task_id)
                .fetch_optional(&mut *conn)
                .await?
                .and_then(|row| row.try_get("repo").ok())
                .unwrap_or_default();
            let views = load_blockers(&mut conn, task_id, &repo).await?;
            let labels: Vec<String> = views
                .into_iter()
                .filter(|view| !view.satisfied)
                .map(|view| view.label)
                .collect();
            if !labels.is_empty() {
                out.insert(task_id, labels);
            }
        }
        Ok(out)
    }

    pub async fn unmet_blocker_labels(&self, task_id: i64) -> anyhow::Result<Vec<String>> {
        let mut conn = self.pool.acquire().await?;
        let repo: String = sqlx::query("SELECT repo FROM tasks WHERE id = ?")
            .bind(task_id)
            .fetch_optional(&mut *conn)
            .await?
            .and_then(|row| row.try_get("repo").ok())
            .unwrap_or_default();
        let views = load_blockers(&mut conn, task_id, &repo).await?;
        Ok(views
            .into_iter()
            .filter(|view| !view.satisfied)
            .map(|view| view.label)
            .collect())
    }

    /// Hold the task when a dependency is open.
    ///
    /// `None` means the task may be claimed. `Some` means it is waiting, and the
    /// vec is the blocking issue labels. The check and the status write share one
    /// immediate transaction, so two ticks cannot both route the same waiting task.
    /// Statuses other than `new`, `routed`, and `waiting` are not rewritten
    pub async fn block_claim_if_waiting(
        &self,
        task_id: i64,
    ) -> anyhow::Result<Option<Vec<String>>> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .context("begin dependency claim")?;
        let row = sqlx::query("SELECT status, agent, model, repo FROM tasks WHERE id = ?")
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let status: String = row.try_get("status")?;
        let agent: Option<String> = row.try_get("agent")?;
        let model: Option<String> = row.try_get("model")?;
        let repo: String = row.try_get("repo")?;
        let views = load_blockers(&mut tx, task_id, &repo).await?;
        let unmet: Vec<String> = views
            .iter()
            .filter(|view| !view.satisfied)
            .map(|view| view.label.clone())
            .collect();

        let schedulable = matches!(status.as_str(), "new" | "routed" | "waiting");
        if unmet.is_empty() {
            if status == "waiting" {
                sqlx::query(
                    "UPDATE tasks
                     SET status = 'new', block_reason = NULL,
                         updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                     WHERE id = ?",
                )
                .bind(task_id)
                .execute(&mut *tx)
                .await?;
                Self::append_activity_tx(
                    &mut tx,
                    task_id,
                    "status_change",
                    Some("waiting"),
                    Some("new"),
                    agent.as_deref(),
                    model.as_deref(),
                    None,
                )
                .await?;
            }
            tx.commit().await?;
            return Ok(None);
        }

        if schedulable && status != "waiting" {
            sqlx::query(
                "UPDATE tasks
                 SET status = 'waiting', branch = '', worktree = '', block_reason = NULL,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                 WHERE id = ?",
            )
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
            Self::append_activity_tx(
                &mut tx,
                task_id,
                "status_change",
                Some(&status),
                Some("waiting"),
                agent.as_deref(),
                model.as_deref(),
                None,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(Some(unmet))
    }

    /// Where `unblock` should leave a task. Waiting when any dependency is still open
    pub async fn status_after_unblock(&self, task_id: i64) -> anyhow::Result<TaskStatus> {
        match self.block_claim_if_waiting(task_id).await? {
            Some(_) => Ok(TaskStatus::Waiting),
            None => Ok(TaskStatus::New),
        }
    }
}

async fn load_blockers(
    conn: &mut SqliteConnection,
    task_id: i64,
    task_repo: &str,
) -> anyhow::Result<Vec<BlockerView>> {
    let rows = sqlx::query(
        "SELECT depends_on_repo, depends_on_number
         FROM task_dependencies
         WHERE task_id = ?
         ORDER BY depends_on_repo, depends_on_number",
    )
    .bind(task_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        let dep_repo: String = row.try_get("depends_on_repo")?;
        let number: i64 = row.try_get("depends_on_number")?;
        let status: Option<String> = sqlx::query(
            "SELECT status FROM tasks WHERE repo = ? AND external_id = ?",
        )
        .bind(&dep_repo)
        .bind(number.to_string())
        .fetch_optional(&mut *conn)
        .await?
        .and_then(|found| found.try_get("status").ok());
        let reason: Option<String> = sqlx::query("SELECT value FROM kv WHERE key = ?")
            .bind(close_reason_key(&dep_repo, number))
            .fetch_optional(&mut *conn)
            .await?
            .and_then(|found| found.try_get("value").ok());
        let status_ref = status.as_deref();
        let reason_ref = reason.as_deref();
        views.push(BlockerView {
            label: dep_label(task_repo, &dep_repo, number),
            satisfied: blocker_is_satisfied(status_ref, reason_ref),
            failure: blocker_failure_kind(status_ref, reason_ref),
        });
    }
    Ok(views)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewTask, TaskStore};

    async fn task(store: &TaskStore, repo: &str, number: &str, title: &str) -> i64 {
        store
            .create(&NewTask {
                external_id: Some(number.to_string()),
                repo: repo.to_string(),
                origin: "github".to_string(),
                title: title.to_string(),
                body: String::new(),
                source: "manual".to_string(),
                source_id: String::new(),
                author: String::new(),
                url: String::new(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn unmet_dependency_demotes_new_and_promotes_when_done() {
        let store = TaskStore::open_memory().await.unwrap();
        let a = task(&store, "owner/repo", "1", "A").await;
        let b = task(&store, "owner/repo", "2", "B").await;
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".to_string(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();

        let held = store.block_claim_if_waiting(b).await.unwrap();
        assert_eq!(held, Some(vec!["#1".to_string()]));
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);

        store.update_status(a, TaskStatus::Done).await.unwrap();
        let released = store.block_claim_if_waiting(b).await.unwrap();
        assert_eq!(released, None);
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::New);
    }

    #[tokio::test]
    async fn not_planned_beats_done_and_completed_satisfies_untracked() {
        let store = TaskStore::open_memory().await.unwrap();
        let a = task(&store, "owner/repo", "1", "A").await;
        let b = task(&store, "owner/repo", "2", "B").await;
        store.update_status(a, TaskStatus::Done).await.unwrap();
        store
            .kv_set(&close_reason_key("owner/repo", 1), "not_planned")
            .await
            .unwrap();
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".to_string(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();
        assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());

        let c = task(&store, "owner/repo", "3", "C").await;
        store
            .kv_set(&close_reason_key("other/repo", 9), "completed")
            .await
            .unwrap();
        store
            .replace_dependencies(
                c,
                &[NewDependency {
                    repo: "other/repo".to_string(),
                    number: 9,
                    source: DepSource::Body,
                }],
            )
            .await
            .unwrap();
        assert_eq!(store.block_claim_if_waiting(c).await.unwrap(), None);
    }

    #[tokio::test]
    async fn blocked_and_duplicate_stay_unmet_and_in_progress_is_not_rewritten() {
        let store = TaskStore::open_memory().await.unwrap();
        let a = task(&store, "owner/repo", "1", "A").await;
        let b = task(&store, "owner/repo", "2", "B").await;
        store.update_status(a, TaskStatus::Blocked).await.unwrap();
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".to_string(),
                    number: 1,
                    source: DepSource::Cli,
                }],
            )
            .await
            .unwrap();
        store
            .update_status(b, TaskStatus::InProgress)
            .await
            .unwrap();
        assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::InProgress);

        store
            .kv_set(&close_reason_key("owner/repo", 1), "duplicate")
            .await
            .unwrap();
        store.update_status(a, TaskStatus::Done).await.unwrap();
        assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn concurrent_claims_both_report_waiting() {
        let store = std::sync::Arc::new(TaskStore::open_memory().await.unwrap());
        let b = task(&store, "owner/repo", "2", "B").await;
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".to_string(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();
        let left = store.clone();
        let right = store.clone();
        let (a, c) = tokio::join!(
            left.block_claim_if_waiting(b),
            right.block_claim_if_waiting(b)
        );
        assert!(a.unwrap().is_some());
        assert!(c.unwrap().is_some());
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
    }

    #[tokio::test]
    async fn unblock_selection_skips_waiting_and_blocked_with_deps_stays_waiting() {
        let store = TaskStore::open_memory().await.unwrap();
        let a = task(&store, "owner/repo", "1", "A").await;
        let b = task(&store, "owner/repo", "2", "B").await;
        store.update_status(a, TaskStatus::Blocked).await.unwrap();
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".to_string(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();
        store
            .block_claim_if_waiting(b)
            .await
            .unwrap();
        let blocked = store
            .list_by_status("owner/repo", TaskStatus::Blocked)
            .await
            .unwrap();
        assert!(blocked.iter().all(|task| task.id != b));
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
        assert_eq!(
            store.status_after_unblock(a).await.unwrap(),
            TaskStatus::New
        );
        store.update_status(a, TaskStatus::Blocked).await.unwrap();
        store
            .replace_dependencies(
                a,
                &[NewDependency {
                    repo: "owner/repo".to_string(),
                    number: 9,
                    source: DepSource::Cli,
                }],
            )
            .await
            .unwrap();
        assert_eq!(
            store.status_after_unblock(a).await.unwrap(),
            TaskStatus::Waiting
        );
    }

    #[tokio::test]
    async fn entering_waiting_clears_a_stored_branch() {
        let store = TaskStore::open_memory().await.unwrap();
        let b = task(&store, "owner/repo", "2", "B").await;
        store
            .set_fields(
                b,
                &[
                    ("branch", serde_json::json!("old-branch")),
                    ("worktree", serde_json::json!("/tmp/old")),
                ],
            )
            .await
            .unwrap();
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".to_string(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();
        store.block_claim_if_waiting(b).await.unwrap();
        let task = store.get(b).await.unwrap();
        assert_eq!(task.status, TaskStatus::Waiting);
        assert!(task.branch.is_empty());
        assert!(task.worktree.is_empty());
    }

    #[tokio::test]
    async fn migration_028_applies_over_existing_tasks() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(":memory:")
            .pragma("foreign_keys", "ON");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("sql"))
            .collect();
        files.sort();
        for path in &files {
            let name = path.file_name().unwrap().to_string_lossy();
            if name.starts_with("028") {
                continue;
            }
            let sql = std::fs::read_to_string(path).unwrap();
            sqlx::raw_sql(&sql)
                .execute(&pool)
                .await
                .unwrap_or_else(|err| panic!("migration {name} failed: {err}"));
        }
        sqlx::query("INSERT INTO tasks (repo, title) VALUES ('owner/repo', 'existing')")
            .execute(&pool)
            .await
            .unwrap();
        let sql = std::fs::read_to_string(dir.join("028_task_dependencies.sql")).unwrap();
        sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
        let (id, title): (i64, String) =
            sqlx::query_as("SELECT id, title FROM tasks").fetch_one(&pool).await.unwrap();
        assert_eq!(title, "existing");
        sqlx::query(
            "INSERT INTO task_dependencies (task_id, depends_on_repo, depends_on_number, source)
             VALUES (?, 'owner/repo', 1, 'cli')",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM task_dependencies")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}

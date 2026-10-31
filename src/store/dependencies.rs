//! Issue dependency rows and the claim gate that keeps a waiting task from starting.
//!
//! A dependency is done when its blocker snapshot says the blocking pull request
//! was merged into the default branch, or the issue has no pull request and was
//! closed as completed. A local `done` row is not enough.
//! `not_planned` and `duplicate` are not done. `new`, `routed`, `waiting`,
//! `blocked`, and `needs_review` move to `waiting` while a dependency is open.
//! `in_progress` and `in_review` stay where they are.
//! A `blocked-external` note in the issue body is an unmet blocker until a later
//! import removes it

use super::{TaskStatus, TaskStore};
use crate::cluster::Command;
use anyhow::Context;
use sqlx::Row;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
}

/// Pull request fields the engine observed on GitHub.
///
/// The store does not talk to GitHub. The engine fills this and the snapshot
/// decides whether the blocker is done
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ObservedPullRequest {
    pub merged: bool,
    pub base_is_default: bool,
    pub approved: bool,
    pub closed_unmerged: bool,
}

/// What orch last read about one blocking issue.
///
/// Missing JSON fails closed: the dependency stays open
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlockerSnapshot {
    pub issue_state: String,
    pub state_reason: Option<String>,
    pub has_pr: bool,
    pub pr_merged: bool,
    pub pr_merged_into_default: bool,
    pub pr_approved: bool,
    pub pr_closed: bool,
}

impl BlockerSnapshot {
    pub fn observe(
        issue_state: &str,
        state_reason: Option<&str>,
        pr: Option<&ObservedPullRequest>,
    ) -> Self {
        let normalized = normalize_close_reason(issue_state, state_reason);
        let (has_pr, merged, into_default, approved, pr_closed) = match pr {
            Some(pr) => (
                true,
                pr.merged,
                pr.base_is_default,
                pr.approved,
                pr.closed_unmerged,
            ),
            None => (false, false, false, false, false),
        };
        Self {
            issue_state: if issue_state.eq_ignore_ascii_case("closed") {
                "closed".to_string()
            } else {
                "open".to_string()
            },
            state_reason: Some(normalized.to_string()),
            has_pr,
            pr_merged: merged,
            pr_merged_into_default: into_default,
            pr_approved: approved,
            pr_closed,
        }
    }

    /// Record a new issue state without clearing pull request fields.
    ///
    /// A later issue sync must not wipe a snapshot that already knows the PR
    pub fn note_issue_state(&mut self, issue_state: &str, state_reason: Option<&str>) {
        let normalized = normalize_close_reason(issue_state, state_reason);
        self.issue_state = if issue_state.eq_ignore_ascii_case("closed") {
            "closed".to_string()
        } else {
            "open".to_string()
        };
        self.state_reason = Some(normalized.to_string());
    }

    pub fn is_satisfied(&self) -> bool {
        let reason = self.state_reason.as_deref().unwrap_or("");
        if reason == "not_planned" || reason == "duplicate" {
            return false;
        }
        if self.has_pr {
            // A merge into the default branch is the decision, even without an approving review
            return self.pr_merged && self.pr_merged_into_default;
        }
        self.issue_state == "closed" && (reason.is_empty() || reason == "completed")
    }
}

/// Map a GitHub issue `state` and `state_reason` onto the values stored in KV.
///
/// A classic close with no reason counts as completed. An open issue counts as open
pub fn normalize_close_reason(state: &str, state_reason: Option<&str>) -> &'static str {
    if !state.eq_ignore_ascii_case("closed") {
        return "open";
    }
    match state_reason
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "not_planned" => "not_planned",
        "duplicate" => "duplicate",
        "reopened" => "open",
        _ => "completed",
    }
}

pub fn blocker_snapshot_key(repo: &str, number: i64) -> String {
    format!("dep_blocker:{repo}#{number}")
}

pub fn worktree_refresh_key(task_id: i64) -> String {
    format!("dep_worktree_refresh:{task_id}")
}

pub fn dep_label(task_repo: &str, dep_repo: &str, number: i64) -> String {
    if task_repo == dep_repo {
        format!("#{number}")
    } else {
        format!("{dep_repo}#{number}")
    }
}

/// A missing snapshot is not done. Local task status is not an input
pub fn blocker_is_satisfied(snapshot: Option<&BlockerSnapshot>) -> bool {
    snapshot.is_some_and(BlockerSnapshot::is_satisfied)
}

/// `not_planned` and a still-open `blocked` task each get one notice.
///
/// `duplicate` does not. No snapshot means no notice
pub fn blocker_failure_kind(
    task_status: Option<&str>,
    snapshot: Option<&BlockerSnapshot>,
) -> Option<&'static str> {
    let snap = snapshot?;
    if snap.state_reason.as_deref() == Some("not_planned") {
        return Some("not_planned");
    }
    if task_status == Some("blocked") && !snap.is_satisfied() {
        return Some("blocked");
    }
    None
}

impl TaskStore {
    /// Rename a repo slug on every stored row in one transaction.
    ///
    /// Refuses when the new slug already has tasks. Returns the number of moved tasks
    pub async fn move_repo(&self, old: &str, new: &str) -> anyhow::Result<u64> {
        let cmd = Command::MoveRepo {
            old: old.to_string(),
            new: new.to_string(),
        };
        Ok(u64::try_from(self.write_int(cmd).await?)?)
    }

    /// Replace one task's dependency rows in a single transaction.
    pub async fn replace_dependencies(
        &self,
        task_id: i64,
        rows: &[NewDependency],
    ) -> anyhow::Result<()> {
        let cmd = Command::DepReplace {
            task_id,
            rows: rows.to_vec(),
        };
        self.write(cmd).await.map(|_| ())
    }

    /// Point dependency rows at a task orch now knows.
    pub async fn link_known_task(
        &self,
        task_id: i64,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<()> {
        let cmd = Command::DepLinkKnown {
            task_id,
            repo: repo.to_string(),
            number,
        };
        self.write(cmd).await.map(|_| ())
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

    pub async fn list_dependents(
        &self,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<TaskDependency>> {
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

    pub async fn save_blocker_snapshot(
        &self,
        repo: &str,
        number: i64,
        snap: &BlockerSnapshot,
    ) -> anyhow::Result<()> {
        let raw = serde_json::to_string(snap).context("encode blocker snapshot")?;
        self.kv_set(&blocker_snapshot_key(repo, number), &raw).await
    }

    pub async fn blocker_snapshot(
        &self,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Option<BlockerSnapshot>> {
        let Some(raw) = self.kv_get(&blocker_snapshot_key(repo, number)).await? else {
            return Ok(None);
        };
        Ok(serde_json::from_str(&raw).ok())
    }

    /// Merge an issue state into the stored snapshot. Pull request fields stay.
    pub async fn note_blocker_issue_state(
        &self,
        repo: &str,
        number: i64,
        state: &str,
        reason: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut snap = match self.blocker_snapshot(repo, number).await? {
            Some(existing) => existing,
            None => BlockerSnapshot::observe(state, reason, None),
        };
        snap.note_issue_state(state, reason);
        self.save_blocker_snapshot(repo, number, &snap).await
    }

    /// Hold the task when a dependency is open.
    ///
    /// `None` means the task may be claimed. `Some` means it is waiting, and the
    /// vec is the blocking issue labels. The check and the status write share one
    /// immediate transaction, so two ticks cannot both route the same waiting task.
    /// `in_progress` and `in_review` are not rewritten
    pub async fn block_claim_if_waiting(
        &self,
        task_id: i64,
    ) -> anyhow::Result<Option<Vec<String>>> {
        match self.write(Command::DepBlockClaim { task_id }).await? {
            crate::cluster::Reply::Unmet(unmet) => Ok(unmet),
            other => anyhow::bail!("block_claim_if_waiting got {other:?}"),
        }
    }

    /// Where `unblock` should leave a task. Waiting when any dependency is still open
    pub async fn status_after_unblock(&self, task_id: i64) -> anyhow::Result<TaskStatus> {
        match self.block_claim_if_waiting(task_id).await? {
            Some(_) => Ok(TaskStatus::Waiting),
            None => Ok(TaskStatus::New),
        }
    }
}

/// Plan-import note that a blocker outside the milestone has no GitHub issue yet
pub fn blocked_external_from_body(body: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut push = |token: &str| {
        let token = token.trim();
        if token.is_empty() {
            return;
        }
        if !ids.iter().any(|have| have == token) {
            ids.push(token.to_string());
        }
    };
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("<!--") {
            let rest = rest.trim();
            let Some(rest) = rest.strip_prefix("orch-blocked-external:") else {
                continue;
            };
            let rest = rest.trim().trim_end_matches("-->").trim();
            for token in rest.split(',') {
                push(token);
            }
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if !lower.starts_with("blocked-external") {
            continue;
        }
        let rest = trimmed["blocked-external".len()..]
            .trim_start()
            .trim_start_matches(':')
            .trim();
        let rest = rest.strip_suffix("(no issue yet)").unwrap_or(rest).trim();
        for token in rest.split(',') {
            push(token);
        }
    }
    ids
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
        let raw: Option<String> = sqlx::query("SELECT value FROM kv WHERE key = ?")
            .bind(blocker_snapshot_key(&dep_repo, number))
            .fetch_optional(&mut *conn)
            .await?
            .and_then(|found| found.try_get("value").ok());
        let snap = raw.and_then(|text| serde_json::from_str::<BlockerSnapshot>(&text).ok());
        views.push(BlockerView {
            label: dep_label(task_repo, &dep_repo, number),
            satisfied: snap.as_ref().is_some_and(BlockerSnapshot::is_satisfied),
        });
    }
    Ok(views)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewTask, TaskStore};

    async fn task(store: &TaskStore, repo: &str, number: &str, title: &str) -> i64 {
        task_with_body(store, repo, number, title, "").await
    }

    async fn task_with_body(
        store: &TaskStore,
        repo: &str,
        number: &str,
        title: &str,
        body: &str,
    ) -> i64 {
        store
            .create(&NewTask {
                external_id: Some(number.to_string()),
                repo: repo.to_string(),
                origin: "github".to_string(),
                title: title.to_string(),
                body: body.to_string(),
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

    fn approved_merge(into_default: bool) -> BlockerSnapshot {
        BlockerSnapshot::observe(
            "closed",
            Some("completed"),
            Some(&ObservedPullRequest {
                merged: true,
                base_is_default: into_default,
                approved: true,
                closed_unmerged: false,
            }),
        )
    }

    #[tokio::test]
    async fn unmet_dependency_demotes_new_and_promotes_when_the_pr_is_done() {
        let store = TaskStore::open_memory().await.unwrap();
        let a = task(&store, "owner/repo", "1", "A").await;
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

        let held = store.block_claim_if_waiting(b).await.unwrap();
        assert_eq!(held, Some(vec!["#1".to_string()]));
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);

        store.update_status(a, TaskStatus::Done).await.unwrap();
        assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);

        store
            .save_blocker_snapshot("owner/repo", 1, &approved_merge(true))
            .await
            .unwrap();
        let released = store.block_claim_if_waiting(b).await.unwrap();
        assert_eq!(released, None);
        let task = store.get(b).await.unwrap();
        assert_eq!(task.status, TaskStatus::New);
        assert!(task.branch.is_empty());
        assert!(task.worktree.is_empty());
        assert_eq!(
            store
                .kv_get(&worktree_refresh_key(b))
                .await
                .unwrap()
                .as_deref(),
            Some("1")
        );
    }

    #[tokio::test]
    async fn not_planned_beats_a_merged_pr_and_a_completed_issue_with_no_pr_is_done() {
        let store = TaskStore::open_memory().await.unwrap();
        let a = task(&store, "owner/repo", "1", "A").await;
        let b = task(&store, "owner/repo", "2", "B").await;
        store.update_status(a, TaskStatus::Done).await.unwrap();
        let mut not_planned = approved_merge(true);
        not_planned.state_reason = Some("not_planned".to_string());
        store
            .save_blocker_snapshot("owner/repo", 1, &not_planned)
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
            .save_blocker_snapshot(
                "other/repo",
                9,
                &BlockerSnapshot::observe("closed", Some("completed"), None),
            )
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

    #[test]
    fn pull_request_completion_cases() {
        let open = BlockerSnapshot::observe("open", None, None);
        assert!(!open.is_satisfied());

        let pr_open = BlockerSnapshot::observe(
            "open",
            None,
            Some(&ObservedPullRequest {
                merged: false,
                base_is_default: true,
                approved: false,
                closed_unmerged: false,
            }),
        );
        assert!(!pr_open.is_satisfied());

        let approved_open = BlockerSnapshot::observe(
            "open",
            None,
            Some(&ObservedPullRequest {
                merged: false,
                base_is_default: true,
                approved: true,
                closed_unmerged: false,
            }),
        );
        assert!(!approved_open.is_satisfied());

        assert!(approved_merge(true).is_satisfied());
        assert!(!approved_merge(false).is_satisfied());

        let closed_unmerged = BlockerSnapshot::observe(
            "closed",
            Some("completed"),
            Some(&ObservedPullRequest {
                merged: false,
                base_is_default: true,
                approved: true,
                closed_unmerged: true,
            }),
        );
        assert!(!closed_unmerged.is_satisfied());

        let merged_unapproved = BlockerSnapshot::observe(
            "closed",
            Some("completed"),
            Some(&ObservedPullRequest {
                merged: true,
                base_is_default: true,
                approved: false,
                closed_unmerged: false,
            }),
        );
        assert!(merged_unapproved.is_satisfied());

        assert!(BlockerSnapshot::observe("closed", None, None).is_satisfied());
        assert!(BlockerSnapshot::observe("closed", Some("completed"), None).is_satisfied());
        assert!(!BlockerSnapshot::observe("closed", Some("not_planned"), None).is_satisfied());
        assert!(!BlockerSnapshot::observe("closed", Some("duplicate"), None).is_satisfied());
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
            .save_blocker_snapshot(
                "owner/repo",
                1,
                &BlockerSnapshot::observe("closed", Some("duplicate"), None),
            )
            .await
            .unwrap();
        store.update_status(a, TaskStatus::Done).await.unwrap();
        assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::InProgress);
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
        store.block_claim_if_waiting(b).await.unwrap();
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

    #[test]
    fn blocked_external_from_body_reads_the_note_and_the_marker() {
        let body = concat!(
            "blocked-external: M1-05, M1-01 (no issue yet)\n\n",
            "<!-- orch-blocked-external: M1-05, M1-01 -->\n",
            "<!-- orch-plan: M6-01 -->\n",
        );
        assert_eq!(
            blocked_external_from_body(body),
            vec!["M1-05".to_string(), "M1-01".to_string()]
        );
        assert!(blocked_external_from_body("<!-- orch-plan: M6-01 -->").is_empty());
    }

    #[tokio::test]
    async fn blocked_external_note_holds_the_task_and_clears_when_removed() {
        let store = TaskStore::open_memory().await.unwrap();
        let body = concat!(
            "blocked-external: M3-01 (no issue yet)\n\n",
            "<!-- orch-blocked-external: M3-01 -->\n",
        );
        let b = task_with_body(&store, "owner/repo", "4", "M4-01 API", body).await;
        let held = store.block_claim_if_waiting(b).await.unwrap();
        assert_eq!(held, Some(vec!["M3-01".to_string()]));
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);

        store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "4",
                title: "M4-01 API",
                body: "<!-- orch-plan: M4-01 -->\n",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        let released = store.block_claim_if_waiting(b).await.unwrap();
        assert_eq!(released, None);
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::New);
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
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
                .execute(&pool)
                .await
                .unwrap_or_else(|err| panic!("migration {name} failed: {err}"));
        }
        sqlx::query("INSERT INTO tasks (repo, title) VALUES ('owner/repo', 'existing')")
            .execute(&pool)
            .await
            .unwrap();
        let sql = std::fs::read_to_string(dir.join("028_task_dependencies.sql")).unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
            .execute(&pool)
            .await
            .unwrap();
        let (id, title): (i64, String) = sqlx::query_as("SELECT id, title FROM tasks")
            .fetch_one(&pool)
            .await
            .unwrap();
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

/// The SQL behind each replicated dependency write
pub(crate) mod apply {
    use super::*;

    pub(crate) async fn move_repo(
        conn: &mut SqliteConnection,
        old: &str,
        new: &str,
    ) -> anyhow::Result<u64> {
        let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE repo = ?")
            .bind(new)
            .fetch_one(&mut *conn)
            .await?;
        if existing > 0 {
            anyhow::bail!("{new} already has {existing} task(s), refusing to move {old} onto it");
        }
        let moved = sqlx::query("UPDATE tasks SET repo = ? WHERE repo = ?")
            .bind(new)
            .bind(old)
            .execute(&mut *conn)
            .await?
            .rows_affected();
        for sql in [
            "UPDATE task_dependencies SET depends_on_repo = ? WHERE depends_on_repo = ?",
            "UPDATE task_metrics SET repo = ? WHERE repo = ?",
            "UPDATE job_state SET repo = ? WHERE repo = ?",
            "UPDATE channel_subscriptions SET repo = ? WHERE repo = ?",
        ] {
            sqlx::query(sql)
                .bind(new)
                .bind(old)
                .execute(&mut *conn)
                .await?;
        }
        move_repo_kv(&mut *conn, old, new).await?;
        Ok(moved)
    }

    /**
     * Rename or drop the KV keys that embed a repo slug. Matching is by exact segment, so a
     * repo whose name merely starts with `old` keeps its keys, which a `LIKE '%old%'` would
     * have taken. Snapshot, read-at and batch-failure keys are dropped because the next
     * refresh rebuilds them. Notice, cycle-comment, approved-sha and ingest-cursor keys are
     * renamed, because dropping one posts its comment again or re-ingests from the start
     **/
    async fn move_repo_kv(conn: &mut SqliteConnection, old: &str, new: &str) -> anyhow::Result<()> {
        for prefix in [
            format!("dep_blocker:{old}#"),
            format!("dep_blocker_read_at:{old}#"),
        ] {
            sqlx::query("DELETE FROM kv WHERE substr(key, 1, ?) = ?")
                .bind(prefix_len(&prefix)?)
                .bind(&prefix)
                .execute(&mut *conn)
                .await?;
        }
        sqlx::query("DELETE FROM kv WHERE key = ?")
            .bind(format!("review_batch_fail:{old}"))
            .execute(&mut *conn)
            .await?;
        for (from, to) in [
            (
                format!("dep_cycle_comment:{old}#"),
                format!("dep_cycle_comment:{new}#"),
            ),
            (
                format!("review_approved_sha:{old}#"),
                format!("review_approved_sha:{new}#"),
            ),
            (format!("ci_check_ts:{old}#"), format!("ci_check_ts:{new}#")),
            (
                format!("ci_pending_since:{old}#"),
                format!("ci_pending_since:{new}#"),
            ),
        ] {
            sqlx::query(
                "UPDATE OR REPLACE kv SET key = ? || substr(key, ?) WHERE substr(key, 1, ?) = ?",
            )
            .bind(&to)
            .bind(prefix_len(&from)? + 1)
            .bind(prefix_len(&from)?)
            .bind(&from)
            .execute(&mut *conn)
            .await?;
        }
        // the repo sits mid-key, after the task id, and `:{repo}#` occurs once
        sqlx::query(
            "UPDATE OR REPLACE kv SET key = replace(key, ?, ?)
             WHERE key GLOB 'dep_blocker_notice:*' AND instr(key, ?) > 0",
        )
        .bind(format!(":{old}#"))
        .bind(format!(":{new}#"))
        .bind(format!(":{old}#"))
        .execute(&mut *conn)
        .await?;
        sqlx::query("UPDATE OR REPLACE kv SET key = ? WHERE key = ?")
            .bind(format!("issues_last_ingested:{new}"))
            .bind(format!("issues_last_ingested:{old}"))
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    /// `substr` and `length` count characters, so the prefix length must too
    fn prefix_len(prefix: &str) -> anyhow::Result<i64> {
        Ok(i64::try_from(prefix.chars().count())?)
    }

    pub(crate) async fn replace_dependencies(
        conn: &mut SqliteConnection,
        task_id: i64,
        rows: &[NewDependency],
    ) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM task_dependencies WHERE task_id = ?")
            .bind(task_id)
            .execute(&mut *conn)
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
            .execute(&mut *conn)
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
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub(crate) async fn link_known_task(
        conn: &mut SqliteConnection,
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
        .execute(conn)
        .await?;
        Ok(())
    }

    pub(crate) async fn block_claim(
        conn: &mut SqliteConnection,
        now: &str,
        task_id: i64,
    ) -> anyhow::Result<Option<Vec<String>>> {
        // sqlx 0.8.1 starts a deferred transaction. This write takes the reserved
        // lock before the read, so a second claim waits and then sees this one
        sqlx::query("UPDATE tasks SET updated_at = updated_at WHERE id = ?")
            .bind(task_id)
            .execute(&mut *conn)
            .await
            .context("lock task for dependency claim")?;
        let row = sqlx::query("SELECT status, agent, model, repo, body FROM tasks WHERE id = ?")
            .bind(task_id)
            .fetch_optional(&mut *conn)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let status: String = row.try_get("status")?;
        let agent: Option<String> = row.try_get("agent")?;
        let model: Option<String> = row.try_get("model")?;
        let repo: String = row.try_get("repo")?;
        let body: String = row.try_get("body").unwrap_or_default();
        let views = load_blockers(conn, task_id, &repo).await?;
        let mut unmet: Vec<String> = views
            .iter()
            .filter(|view| !view.satisfied)
            .map(|view| view.label.clone())
            .collect();
        for id in blocked_external_from_body(&body) {
            if !unmet.iter().any(|have| have == &id) {
                unmet.push(id);
            }
        }

        let holdable = matches!(
            status.as_str(),
            "new" | "routed" | "waiting" | "blocked" | "needs_review"
        );
        if unmet.is_empty() {
            if status == "waiting" {
                sqlx::query(
                    "UPDATE tasks
                     SET status = 'new', block_reason = NULL, branch = '', worktree = '',
                         updated_at = ?
                     WHERE id = ?",
                )
                .bind(now)
                .bind(task_id)
                .execute(&mut *conn)
                .await?;
                sqlx::query(
                    "INSERT INTO kv (key, value, updated_at)
                     VALUES (?, '1', ?)
                     ON CONFLICT(key) DO UPDATE
                       SET value = excluded.value, updated_at = excluded.updated_at",
                )
                .bind(worktree_refresh_key(task_id))
                .bind(now)
                .execute(&mut *conn)
                .await?;
                crate::store::tasks::apply::activity(
                    conn,
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
            return Ok(None);
        }

        if holdable && status != "waiting" {
            sqlx::query(
                "UPDATE tasks
                 SET status = 'waiting', branch = '', worktree = '', block_reason = NULL,
                     updated_at = ?
                 WHERE id = ?",
            )
            .bind(now)
            .bind(task_id)
            .execute(&mut *conn)
            .await?;
            crate::store::tasks::apply::activity(
                conn,
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
        Ok(Some(unmet))
    }
}

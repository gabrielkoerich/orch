use super::{
    Context, NewDependency, Row, SqliteConnection, blocked_external_from_body, load_blockers,
    worktree_refresh_key,
};
use crate::store::tasks::apply;

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
        (
            format!("review_ci_wait_since:{old}#"),
            format!("review_ci_wait_since:{new}#"),
        ),
        (
            format!("ci_bootstrap_wait_since:{old}#"),
            format!("ci_bootstrap_wait_since:{new}#"),
        ),
        (
            format!("ci_bootstrap_no_checks_comment:{old}#"),
            format!("ci_bootstrap_no_checks_comment:{new}#"),
        ),
        (
            format!("ci_bootstrap_never_run_comment:{old}#"),
            format!("ci_bootstrap_never_run_comment:{new}#"),
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
            apply::activity(
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
        apply::activity(
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

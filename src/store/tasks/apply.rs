use super::{
    ALLOWED_FIELDS, INCREMENTABLE_FIELDS, MemoryEntry, NewTask, Row, StoreRouteCmd, TaskStatus,
    UpsertExternalCmd, sanitize_activity_details, sql_value_for_task_field,
};
use sqlx::SqliteConnection;

async fn previous(
    conn: &mut SqliteConnection,
    id: i64,
) -> anyhow::Result<(String, Option<String>, Option<String>)> {
    let row = sqlx::query("SELECT status, agent, model FROM tasks WHERE id = ?")
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
    let status: String = row
        .try_get("status")
        .map_err(|e| anyhow::anyhow!("failed to read status column before update: {e}"))?;
    let agent: Option<String> = row
        .try_get("agent")
        .map_err(|e| anyhow::anyhow!("failed to read agent column before update: {e}"))?;
    let model: Option<String> = row
        .try_get("model")
        .map_err(|e| anyhow::anyhow!("failed to read model column before update: {e}"))?;
    Ok((status, agent, model))
}

#[expect(
    clippy::too_many_arguments,
    reason = "one argument per column of the activity row"
)]
pub(crate) async fn activity(
    conn: &mut SqliteConnection,
    task_id: i64,
    event_type: &str,
    from_status: Option<&str>,
    to_status: Option<&str>,
    agent: Option<&str>,
    model: Option<&str>,
    details: Option<&serde_json::Value>,
) -> anyhow::Result<()> {
    let details_json = match details {
        Some(v) => serde_json::to_string(&sanitize_activity_details(v))?,
        None => "{}".to_string(),
    };
    sqlx::query(
        "INSERT INTO task_activity
         (task_id, event_type, from_status, to_status, agent, model, details)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(task_id)
    .bind(event_type)
    .bind(from_status)
    .bind(to_status)
    .bind(agent)
    .bind(model)
    .bind(details_json)
    .execute(conn)
    .await?;
    Ok(())
}

async fn status_change(
    conn: &mut SqliteConnection,
    id: i64,
    previous: &(String, Option<String>, Option<String>),
    to: TaskStatus,
    details: Option<&serde_json::Value>,
) -> anyhow::Result<()> {
    activity(
        conn,
        id,
        "status_change",
        Some(previous.0.as_str()),
        Some(to.as_str()),
        previous.1.as_deref(),
        previous.2.as_deref(),
        details,
    )
    .await
}

/// Through Raft, ids come from the replicated rows so every node assigns the same one.
/// Otherwise NULL lets SQLite's AUTOINCREMENT pick it, as before
pub(crate) async fn create(
    conn: &mut SqliteConnection,
    now: &str,
    new: &NewTask,
    internal: bool,
    replicated: bool,
) -> anyhow::Result<i64> {
    let labels_json = serde_json::to_string(&new.labels)?;
    let row = sqlx::query(
        "INSERT INTO tasks (id, external_id, repo, origin, title, body, source, source_id, author, url, labels, parent_id, created_at, updated_at)
         VALUES ((SELECT CASE WHEN ? THEN COALESCE(MAX(id), 0) + 1 END FROM tasks), ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(replicated)
    .bind(&new.external_id)
    .bind(&new.repo)
    .bind(&new.origin)
    .bind(&new.title)
    .bind(&new.body)
    .bind(&new.source)
    .bind(&new.source_id)
    .bind(&new.author)
    .bind(&new.url)
    .bind(&labels_json)
    .bind(new.parent_id)
    .bind(now)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;
    let id: i64 = row
        .try_get("id")
        .map_err(|e| anyhow::anyhow!("INSERT RETURNING id returned NULL or invalid: {e}"))?;
    if internal {
        // resolve_task_id finds internal tasks by this external id
        sqlx::query("UPDATE tasks SET external_id = ? WHERE id = ?")
            .bind(format!("internal:{id}"))
            .bind(id)
            .execute(conn)
            .await?;
    }

    Ok(id)
}

pub(crate) async fn upsert_external(
    conn: &mut SqliteConnection,
    now: &str,
    ext: &UpsertExternalCmd,
    replicated: bool,
) -> anyhow::Result<i64> {
    let labels_json = serde_json::to_string(&ext.labels)?;
    let row = sqlx::query(
        "INSERT INTO tasks (id, external_id, repo, origin, title, body, author, url, labels, parent_id, created_at, updated_at)
         VALUES ((SELECT CASE WHEN ? THEN COALESCE(MAX(id), 0) + 1 END FROM tasks), ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(repo, external_id) DO UPDATE SET
            title = excluded.title,
            body = excluded.body,
            labels = excluded.labels,
            url = excluded.url,
            parent_id = COALESCE(excluded.parent_id, tasks.parent_id)
         RETURNING id",
    )
    .bind(replicated)
    .bind(&ext.ext_id)
    .bind(&ext.repo)
    .bind(&ext.origin)
    .bind(&ext.title)
    .bind(&ext.body)
    .bind(&ext.author)
    .bind(&ext.url)
    .bind(&labels_json)
    .bind(ext.parent_id)
    .bind(now)
    .bind(now)
    .fetch_one(conn)
    .await?;
    row.try_get("id")
        .map_err(|e| anyhow::anyhow!("INSERT RETURNING id returned NULL or invalid: {e}"))
}

fn ensure_allowed(updates: &[(String, serde_json::Value)]) -> anyhow::Result<()> {
    for (col, _) in updates {
        anyhow::ensure!(
            ALLOWED_FIELDS.contains(&col.as_str()),
            "column {col} is not in the update allowlist"
        );
    }

    Ok(())
}

pub(crate) async fn update_status_and_fields(
    conn: &mut SqliteConnection,
    now: &str,
    id: i64,
    status: TaskStatus,
    updates: &[(String, serde_json::Value)],
) -> anyhow::Result<()> {
    ensure_allowed(updates)?;
    let prev = previous(conn, id).await?;

    let mut set_parts = vec!["status = ?".to_string()];
    let mut values: Vec<Option<String>> = vec![Some(status.as_str().to_string())];
    let mut block_reason_in_updates = false;
    for (col, val) in updates {
        if col == "block_reason" {
            block_reason_in_updates = true;
        }

        set_parts.push(format!("{col} = ?"));
        match val {
            serde_json::Value::String(s) => values.push(Some(s.clone())),
            serde_json::Value::Number(n) => values.push(Some(n.to_string())),
            serde_json::Value::Bool(b) => {
                values.push(Some(if *b { "1" } else { "0" }.to_string()));
            }
            serde_json::Value::Null => values.push(None),
            other => values.push(Some(serde_json::to_string(other)?)),
        }
    }

    if !block_reason_in_updates && status != TaskStatus::Blocked {
        set_parts.push("block_reason = NULL".to_string());
    }

    set_parts.push("updated_at = ?".to_string());
    values.push(Some(now.to_string()));
    let sql = format!("UPDATE tasks SET {} WHERE id = ?", set_parts.join(", "));
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
    for v in &values {
        query = query.bind(v.as_deref());
    }
    query.bind(id).execute(&mut *conn).await?;
    status_change(conn, id, &prev, status, None).await
}

/// Moves the task to `status`, and when `expected` is set only if it still holds that status
pub(crate) async fn update_status(
    conn: &mut SqliteConnection,
    now: &str,
    id: i64,
    status: TaskStatus,
    expected: Option<TaskStatus>,
) -> anyhow::Result<bool> {
    let prev = previous(conn, id).await?;
    let clear_block = if status == TaskStatus::Blocked {
        ""
    } else {
        ", block_reason = NULL"
    };
    let guard = if expected.is_some() {
        " AND status = ?"
    } else {
        ""
    };
    let sql =
        format!("UPDATE tasks SET status = ?{clear_block}, updated_at = ? WHERE id = ?{guard}");
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(status.as_str())
        .bind(now)
        .bind(id);
    if let Some(expected) = expected {
        query = query.bind(expected.as_str());
    }

    if query.execute(&mut *conn).await?.rows_affected() == 0 {
        return Ok(false);
    }

    status_change(conn, id, &prev, status, None).await?;
    Ok(true)
}

pub(crate) async fn update_external_id(
    conn: &mut SqliteConnection,
    now: &str,
    id: i64,
    external_id: &str,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE tasks SET external_id = ?, updated_at = ? WHERE id = ?")
        .bind(external_id)
        .bind(now)
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

pub(crate) async fn touch(conn: &mut SqliteConnection, now: &str, id: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE tasks SET updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

pub(crate) async fn reset_to_new(
    conn: &mut SqliteConnection,
    now: &str,
    id: i64,
) -> anyhow::Result<()> {
    let prev = previous(conn, id).await?;
    sqlx::query(
        "UPDATE tasks SET status = 'new', branch = '', worktree = '', worktree_cleaned = 0, block_reason = NULL, updated_at = ? WHERE id = ?",
    )
    .bind(now)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    let details = serde_json::json!({ "op": "reset_to_new" });
    status_change(conn, id, &prev, TaskStatus::New, Some(&details)).await
}

/// `silent` leaves `updated_at` alone, for bookkeeping that must not look like activity
pub(crate) async fn set_fields(
    conn: &mut SqliteConnection,
    now: &str,
    id: i64,
    updates: &[(String, serde_json::Value)],
    silent: bool,
) -> anyhow::Result<()> {
    if updates.is_empty() {
        return Ok(());
    }

    ensure_allowed(updates)?;
    let mut set_parts = Vec::new();
    let mut values: Vec<Option<String>> = Vec::new();
    for (col, val) in updates {
        set_parts.push(format!("{col} = ?"));
        values.push(sql_value_for_task_field(col, val)?);
    }

    if !silent {
        set_parts.push("updated_at = ?".to_string());
        values.push(Some(now.to_string()));
    }

    let sql = format!("UPDATE tasks SET {} WHERE id = ?", set_parts.join(", "));
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
    for v in &values {
        query = query.bind(v.as_deref());
    }
    query.bind(id).execute(conn).await?;
    Ok(())
}

pub(crate) async fn increment(
    conn: &mut SqliteConnection,
    now: &str,
    id: i64,
    field: &str,
) -> anyhow::Result<i64> {
    anyhow::ensure!(
        INCREMENTABLE_FIELDS.contains(&field),
        "field {field} is not incrementable"
    );
    let sql = format!(
        "UPDATE tasks SET {field} = {field} + 1, updated_at = ? WHERE id = ? RETURNING {field} AS val"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(now)
        .bind(id)
        .fetch_one(conn)
        .await?;
    let val: i32 = row.try_get("val")?;
    Ok(val.into())
}

/// `failure_only` keeps `attempts`, `review_invocations`, `review_cycles`,
/// `ci_merge_failures` and `merge_conflict_retries`, see `reset_failure_counters`
pub(crate) async fn reset_counters(
    conn: &mut SqliteConnection,
    now: &str,
    id: i64,
    failure_only: bool,
) -> anyhow::Result<()> {
    let all = if failure_only {
        ""
    } else {
        "attempts = 0, merge_conflict_retries = 0, ci_merge_failures = 0, review_cycles = 0, review_invocations = 0,"
    };
    let sql = format!(
        "UPDATE tasks SET {all}
        route_attempts = 0,
        review_agent_failures = 0,
        review_resets = 0,
        pr_create_failures = 0,
        push_failures = 0,
        network_retries = 0,
        timeout_retries = 0,
        auto_unblock_count = 0,
        auto_unblock_last_at = '',
        auto_unblock_last_reason = '',
        ci_recovery_count = 0,
        no_code_reroutes = 0,
        no_code_last_agent = '',
        needs_review_refires = 0,
        updated_at = ?
     WHERE id = ?"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(now)
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

pub(crate) async fn store_route(
    conn: &mut SqliteConnection,
    now: &str,
    route: &StoreRouteCmd,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE tasks SET
        agent = ?, model = ?, complexity = ?, estimate = ?, route_reason = ?,
        agent_profile = ?, selected_skills = ?,
        updated_at = ?
     WHERE id = ?",
    )
    .bind(&route.agent)
    .bind(&route.model)
    .bind(&route.complexity)
    .bind(i64::from(route.estimate))
    .bind(&route.reason)
    .bind(&route.profile)
    .bind(&route.skills)
    .bind(now)
    .bind(route.id)
    .execute(&mut *conn)
    .await?;
    let details = serde_json::json!({
        "complexity": route.complexity,
        "estimate": route.estimate,
        "reason": route.reason,
        "skills": route.skills,
    });
    activity(
        conn,
        route.id,
        "dispatch",
        None,
        None,
        Some(&route.agent),
        route.model.as_deref(),
        Some(&details),
    )
    .await
}

pub(crate) async fn set_estimate_if_zero(
    conn: &mut SqliteConnection,
    now: &str,
    id: i64,
    estimate: u8,
) -> anyhow::Result<bool> {
    let result =
        sqlx::query("UPDATE tasks SET estimate = ?, updated_at = ? WHERE id = ? AND estimate = 0")
            .bind(i64::from(estimate))
            .bind(now)
            .bind(id)
            .execute(conn)
            .await?;
    Ok(result.rows_affected() > 0)
}

pub(crate) async fn append_memory(
    conn: &mut SqliteConnection,
    now: &str,
    id: i64,
    entry: &MemoryEntry,
) -> anyhow::Result<()> {
    let row = sqlx::query("SELECT memory FROM tasks WHERE id = ?")
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
    // A corrupt value must fail, an empty array in its place would lose the old entries
    let memory_opt: Option<String> = row
        .try_get::<Option<String>, _>("memory")
        .map_err(|e| anyhow::anyhow!("failed to read memory column for task {id}: {e}"))?;
    let memory_str = memory_opt.unwrap_or_default();
    let mut memory: Vec<MemoryEntry> = if memory_str.trim().is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(&memory_str)
            .map_err(|e| anyhow::anyhow!("failed to parse memory JSON for task {id}: {e}"))?
    };
    memory.push(entry.clone());
    sqlx::query("UPDATE tasks SET memory = ?, updated_at = ? WHERE id = ?")
        .bind(serde_json::to_string(&memory)?)
        .bind(now)
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

pub(crate) async fn mark_cleaned(
    conn: &mut SqliteConnection,
    now: &str,
    id: i64,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE tasks SET worktree_cleaned = 1, updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    activity(
        conn,
        id,
        "branch_delete",
        None,
        None,
        None,
        None,
        Some(&serde_json::json!({ "worktree_cleaned": true })),
    )
    .await
}

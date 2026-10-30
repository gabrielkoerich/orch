//! Whole replicated tables, copied in and out of a Raft snapshot

use sqlx::{SqliteConnection, SqlitePool};

/// Replicated tables a snapshot copies whole. `tasks` and `kv` have their own rules
pub(crate) const TABLES: [&str; 3] = ["task_dependencies", "job_state", "channel_subscriptions"];

/// `json_object` per chunk merged with `json_patch`, so the column count never hits SQLite's argument limit
pub(crate) fn json_row_sql(cols: &[&String]) -> String {
    cols.chunks(40)
        .map(|chunk| {
            let pairs: Vec<String> = chunk.iter().map(|c| format!("'{c}', {c}")).collect();
            format!("json_object({})", pairs.join(", "))
        })
        .reduce(|a, b| format!("json_patch({a}, {b})"))
        .unwrap_or_else(|| "json_object()".to_string())
}

async fn columns(conn: &mut SqliteConnection, table: &str) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(TABLES.contains(&table), "{table} is not a replicated table");
    let cols: Vec<(String,)> = sqlx::query_as("SELECT name FROM pragma_table_info(?)")
        .bind(table)
        .fetch_all(conn)
        .await?;
    Ok(cols.into_iter().map(|(c,)| c).collect())
}

pub(crate) async fn table_rows(
    pool: &SqlitePool,
    table: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut conn = pool.acquire().await?;
    let cols = columns(&mut conn, table).await?;
    let refs: Vec<&String> = cols.iter().collect();
    let sql = format!("SELECT {} FROM {table} ORDER BY rowid", json_row_sql(&refs));
    let rows: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
        .fetch_all(&mut *conn)
        .await?;
    rows.into_iter()
        .map(|(r,)| Ok(serde_json::from_str(&r)?))
        .collect()
}

pub(crate) async fn replace_table(
    conn: &mut SqliteConnection,
    table: &str,
    rows: &[serde_json::Value],
) -> anyhow::Result<()> {
    let allowed = columns(conn, table).await?;
    sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
        .execute(&mut *conn)
        .await?;
    for row in rows {
        let obj = row
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("snapshot {table} row is not an object"))?;
        let cols: Vec<&str> = obj.keys().map(String::as_str).collect();
        for c in &cols {
            anyhow::ensure!(
                allowed.iter().any(|a| a == c),
                "snapshot names unknown {table} column {c}"
            );
        }

        let values: Vec<String> = cols
            .iter()
            .map(|c| format!("json_extract(?1, '$.\"{c}\"')"))
            .collect();
        let sql = format!(
            "INSERT INTO {table} ({}) VALUES ({})",
            cols.join(", "),
            values.join(", ")
        );
        sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(serde_json::to_string(row)?)
            .execute(&mut *conn)
            .await?;
    }

    Ok(())
}

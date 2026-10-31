use super::{SqlitePool, TaskStore};
use crate::cluster::{Command, Reply};
use sqlx::SqliteConnection;

/// Key prefixes that stay on this node: metrics and the node's own Raft position
const LOCAL_PREFIXES: [&str; 2] = ["metrics:", "raft:"];

pub(crate) fn is_local(key: &str) -> bool {
    LOCAL_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// One replicated kv row: key, value and `updated_at`
pub(crate) type Row = (String, String, String);

pub(crate) async fn set(
    conn: &mut SqliteConnection,
    now: &str,
    key: &str,
    value: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO kv (key, value, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

/// A single statement, so the read-modify-write cannot race
pub(crate) async fn increment(
    conn: &mut SqliteConnection,
    now: &str,
    key: &str,
) -> anyhow::Result<u64> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO kv (key, value, updated_at) VALUES (?1, '1', ?2)
         ON CONFLICT(key) DO UPDATE
           SET value = CAST(value AS INTEGER) + 1,
               updated_at = ?2
         RETURNING CAST(value AS INTEGER)",
    )
    .bind(key)
    .bind(now)
    .fetch_one(conn)
    .await?;
    debug_assert!(
        row.0 >= 1,
        "kv_increment returned unexpected value {}",
        row.0
    );
    Ok(u64::try_from(row.0).unwrap_or(0))
}

/// Whoever lands first wins, and every caller reads back the stored winner
pub(crate) async fn insert_if_absent(
    conn: &mut SqliteConnection,
    now: &str,
    key: &str,
    value: &str,
) -> anyhow::Result<String> {
    sqlx::query("INSERT OR IGNORE INTO kv (key, value, updated_at) VALUES (?, ?, ?)")
        .bind(key)
        .bind(value)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    let (stored,): (String,) = sqlx::query_as("SELECT value FROM kv WHERE key = ?")
        .bind(key)
        .fetch_one(conn)
        .await?;
    Ok(stored)
}

pub(crate) async fn delete(conn: &mut SqliteConnection, key: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM kv WHERE key = ?")
        .bind(key)
        .execute(conn)
        .await?;
    Ok(())
}

pub(crate) async fn replicated_rows(pool: &SqlitePool) -> anyhow::Result<Vec<Row>> {
    let rows: Vec<Row> = sqlx::query_as("SELECT key, value, updated_at FROM kv ORDER BY key")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().filter(|r| !is_local(&r.0)).collect())
}

/// Swap every replicated row for `rows`, leaving local keys alone
pub(crate) async fn replace_replicated(
    conn: &mut SqliteConnection,
    rows: &[Row],
) -> anyhow::Result<()> {
    let keys: Vec<(String,)> = sqlx::query_as("SELECT key FROM kv")
        .fetch_all(&mut *conn)
        .await?;
    for (key,) in keys.iter().filter(|(k,)| !is_local(k)) {
        delete(conn, key).await?;
    }

    for (key, value, updated_at) in rows.iter().filter(|(k, _, _)| !is_local(k)) {
        set(conn, updated_at, key, value).await?;
    }

    Ok(())
}

impl TaskStore {
    pub async fn kv_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let result: Option<(String,)> = sqlx::query_as("SELECT value FROM kv WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(result.map(|r| r.0))
    }

    /// Set a value in the KV store (upsert).
    pub async fn kv_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let cmd = Command::KvSet {
            key: key.to_string(),
            value: value.to_string(),
        };
        self.write_kv(key, cmd).await.map(|_| ())
    }

    /// Atomically increment an integer counter in the KV store, inserting it as 1 if absent.
    /// Returns the new value after the increment.
    pub async fn kv_increment(&self, key: &str) -> anyhow::Result<u64> {
        let cmd = Command::KvIncrement {
            key: key.to_string(),
        };
        match self.write_kv(key, cmd).await? {
            Reply::Count(n) => Ok(n),
            other => anyhow::bail!("kv_increment got {other:?}"),
        }
    }

    /// Insert a value only if the key is absent, then return the stored winner.
    pub async fn kv_insert_if_absent(&self, key: &str, value: &str) -> anyhow::Result<String> {
        let cmd = Command::KvInsertIfAbsent {
            key: key.to_string(),
            value: value.to_string(),
        };
        match self.write_kv(key, cmd).await? {
            Reply::Value(v) => Ok(v),
            other => anyhow::bail!("kv_insert_if_absent got {other:?}"),
        }
    }

    /// Delete a key from the KV store.
    pub async fn kv_delete(&self, key: &str) -> anyhow::Result<()> {
        let cmd = Command::KvDelete {
            key: key.to_string(),
        };
        self.write_kv(key, cmd).await.map(|_| ())
    }

    /// Keys local to this node skip the Raft log
    async fn write_kv(&self, key: &str, cmd: Command) -> anyhow::Result<Reply> {
        if is_local(key) {
            return self.apply_direct(cmd).await;
        }

        self.write(cmd).await
    }

    /// List all (key, value) pairs where the key starts with `prefix`.
    ///
    /// The prefix is escaped so that literal `%` and `_` characters are treated as
    /// plain text rather than SQLite LIKE wildcards.
    pub async fn kv_list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<(String, String)>> {
        // Escape LIKE metacharacters so a prefix containing '%' or '_' (or the chosen
        // escape char '\') is matched literally.
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("{escaped}%");
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT key, value FROM kv WHERE key LIKE ? ESCAPE '\\'")
                .bind(&pattern)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }
}

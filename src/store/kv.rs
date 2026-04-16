use super::*;

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
        sqlx::query(
        "INSERT INTO kv (key, value, updated_at) VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .execute(&self.pool)
    .await?;
        Ok(())
    }

    /// Atomically increment an integer counter in the KV store, inserting it as 1 if absent.
    ///
    /// Executes a single SQL statement (`INSERT … ON CONFLICT … DO UPDATE`) so concurrent
    /// callers cannot race — SQLite's serialised write lock ensures the read-modify-write is
    /// indivisible.  Returns the new value after the increment.
    pub async fn kv_increment(&self, key: &str) -> anyhow::Result<u64> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO kv (key, value, updated_at) VALUES (?1, '1', strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
             ON CONFLICT(key) DO UPDATE
               SET value = CAST(value AS INTEGER) + 1,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             RETURNING CAST(value AS INTEGER)",
        )
        .bind(key)
        .fetch_one(&self.pool)
        .await?;
        // The SQL always returns a value >= 1 (insert path returns 1, update adds 1 to
        // the previous integer value). Previously this was clamped with `.max(1)`,
        // which silently masked database corruption or unexpected negative values.
        // Use a debug assertion to loudly detect unexpected values in debug builds
        // while returning the actual stored value in release builds.
        debug_assert!(
            row.0 >= 1,
            "kv_increment returned unexpected value {}",
            row.0
        );
        Ok(row.0 as u64)
    }

    /// Insert a value only if the key is absent, then return the stored winner.
    ///
    /// Uses `INSERT OR IGNORE` so concurrent callers racing to create the same key
    /// both converge on the same stored value — whoever lands first wins, and the
    /// loser's value is silently discarded.  The unconditional `SELECT` afterwards
    /// always reads back whichever value is actually in the store.
    pub async fn kv_insert_if_absent(&self, key: &str, value: &str) -> anyhow::Result<String> {
        sqlx::query(
            "INSERT OR IGNORE INTO kv (key, value, updated_at)
             VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        let (stored,): (String,) = sqlx::query_as("SELECT value FROM kv WHERE key = ?")
            .bind(key)
            .fetch_one(&self.pool)
            .await?;
        Ok(stored)
    }

    /// Delete a key from the KV store.
    pub async fn kv_delete(&self, key: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM kv WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
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

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
    pub async fn kv_increment(&self, key: &str) -> anyhow::Result<u32> {
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
        Ok(row.0.max(1) as u32)
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
    pub async fn kv_list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<(String, String)>> {
        let pattern = format!("{prefix}%");
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT key, value FROM kv WHERE key LIKE ?")
                .bind(&pattern)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }
}

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

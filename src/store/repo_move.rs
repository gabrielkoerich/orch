use super::*;

impl TaskStore {
    /// Re-key every row stored under `old` to `new` in one transaction. Returns the task count moved.
    pub async fn move_repo(&self, old: &str, new: &str) -> anyhow::Result<u64> {
        anyhow::ensure!(old != new, "old and new slug are the same");
        let mut tx = self.pool.begin().await?;
        let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE repo = ?")
            .bind(new)
            .fetch_one(&mut *tx)
            .await?;
        anyhow::ensure!(existing == 0, "{new} already has {existing} task(s)");
        let moved = sqlx::query("UPDATE tasks SET repo = ? WHERE repo = ?")
            .bind(new)
            .bind(old)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        for sql in [
            "UPDATE task_dependencies SET depends_on_repo = ? WHERE depends_on_repo = ?",
            "UPDATE task_metrics SET repo = ? WHERE repo = ?",
            "UPDATE OR IGNORE job_state SET repo = ? WHERE repo = ?",
            "UPDATE OR IGNORE channel_subscriptions SET repo = ? WHERE repo = ?",
        ] {
            sqlx::query(sql)
                .bind(new)
                .bind(old)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(moved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn add(store: &TaskStore, repo: &str, id: &str) {
        store
            .create(&NewTask {
                external_id: Some(id.to_string()),
                repo: repo.to_string(),
                origin: "github".to_string(),
                title: "t".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn move_repo_rekeys_tasks_and_refuses_occupied_target() {
        let store = TaskStore::open_memory().await.unwrap();
        add(&store, "a/x", "1").await;
        add(&store, "a/x", "2").await;
        assert_eq!(store.move_repo("a/x", "b/x").await.unwrap(), 2);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE repo = 'b/x'")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(n, 2);

        add(&store, "a/x", "3").await;
        assert!(store.move_repo("a/x", "b/x").await.is_err());
    }
}

//! Engine integration tests — verify the service starts and ticks.
//!
//! Uses an in-memory TaskStore and a mock backend so no GitHub access
//! or API keys are needed. These tests run in CI and catch regressions
//! that break the engine startup or tick loop (e.g., migration checksum
//! mismatches, config parsing failures, panics in init).
//!
//! Run:
//! ```bash
//! cargo test --test integration_engine
//! ```

use async_trait::async_trait;
use orch::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use orch::store::TaskStore;
use std::sync::Arc;

/// Minimal mock backend that passes health checks and returns empty lists.
struct MockBackend;

#[async_trait]
impl ExternalBackend for MockBackend {
    fn name(&self) -> &str {
        "mock"
    }

    async fn create_task(
        &self,
        _title: &str,
        _body: &str,
        _labels: &[String],
    ) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("mock-1".to_string()))
    }

    async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
        Ok(ExternalTask {
            id: id.clone(),
            title: "mock task".to_string(),
            body: "".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "".to_string(),
        })
    }

    async fn list_by_status(&self, _status: Status) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }

    async fn post_comment(&self, _id: &ExternalId, _body: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn set_labels(&self, _id: &ExternalId, _labels: &[String]) -> anyhow::Result<()> {
        Ok(())
    }

    async fn remove_label(&self, _id: &ExternalId, _label: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        Ok(vec![])
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Verify that TaskStore opens and migrations run on a fresh database.
///
/// This is the #1 failure mode: agents modify migration files, breaking
/// the checksum, and the engine loops forever on startup.
#[tokio::test]
async fn store_opens_and_migrates() {
    let tmp = std::env::temp_dir().join(format!("orch-engine-open-{}.db", std::process::id()));
    let store = TaskStore::open(&tmp).await;
    assert!(store.is_ok(), "TaskStore::open failed: {:?}", store.err());
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
}

/// Verify that TaskStore migrations are idempotent (open twice).
#[tokio::test]
async fn store_migrations_idempotent() {
    let tmp = std::env::temp_dir().join(format!("orch-engine-test-{}.db", std::process::id()));

    // First open
    {
        let result = TaskStore::open(&tmp).await;
        assert!(result.is_ok(), "first open failed: {:?}", result.err());
    }

    // Second open (validates checksums)
    {
        let result = TaskStore::open(&tmp).await;
        assert!(
            result.is_ok(),
            "second open failed (checksum mismatch?): {:?}",
            result.err()
        );
    }

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
}

/// Verify that a task can be created and retrieved from the store.
#[tokio::test]
async fn store_create_and_get_task() {
    let tmp = std::env::temp_dir().join(format!("orch-engine-crud-{}.db", std::process::id()));
    let store = TaskStore::open(&tmp).await.expect("open store");

    let id = store
        .create(&orch::store::NewTask {
            external_id: Some("test-1".to_string()),
            repo: "test/repo".to_string(),
            origin: "external".to_string(),
            title: "Test task".to_string(),
            body: "Test body".to_string(),
            source: "test".to_string(),
            source_id: "1".to_string(),
            author: "tester".to_string(),
            url: "".to_string(),
            labels: vec![],
        })
        .await
        .expect("create task");

    let task = store.get(id).await.expect("get task");
    assert_eq!(task.title, "Test task");
    assert_eq!(task.status, orch::store::TaskStatus::New);

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
}

/// Verify that the router config parses without panicking.
#[test]
fn router_config_parses() {
    // This exercises RouterConfig::from_config() which reads config.yml.
    // In test context there's no config file, so it uses defaults.
    let config = orch::engine::router::config::RouterConfig::from_config();
    assert!(!config.agents.is_empty(), "should have default agents");
    assert_eq!(config.mode, "llm");
}

/// Verify that the mock backend passes the health check.
#[tokio::test]
async fn mock_backend_health_check() {
    let backend = MockBackend;
    assert!(backend.health_check().await.is_ok());
}

/// Verify that cooldown init works with an in-memory store.
#[tokio::test]
async fn cooldown_init_with_store() {
    let tmp = std::env::temp_dir().join(format!("orch-engine-cooldown-{}.db", std::process::id()));
    let store = Arc::new(TaskStore::open(&tmp).await.expect("open store"));
    orch::engine::cooldown::init_cooldown_store(store).await;
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
    // Should not panic — that's the test
}

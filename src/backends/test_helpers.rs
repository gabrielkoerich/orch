//! Shared no-op backend for unit tests.
//!
//! Provides a minimal `ExternalBackend` implementation where every method
//! succeeds immediately with an empty/default result. Tests that only need a
//! backend "to exist" (e.g. to exercise status-transition logic without
//! side-effects) can use this instead of defining their own identical struct.
//!
//! Available only in test builds (`#[cfg(test)]`).

use super::*;
use async_trait::async_trait;

pub(crate) struct NoopBackend;

#[async_trait]
impl ExternalBackend for NoopBackend {
    fn name(&self) -> &str {
        "noop"
    }

    async fn create_task(&self, _t: &str, _b: &str, _l: &[String]) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("new".into()))
    }

    async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
        Ok(ExternalTask {
            id: id.clone(),
            title: "t".into(),
            body: "".into(),
            state: "open".into(),
            labels: vec![],
            author: "bot".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            url: "".into(),
        })
    }

    async fn list_by_status(&self, _s: Status) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }

    async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }

    async fn post_comment(&self, _id: &ExternalId, _b: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn set_labels(&self, _id: &ExternalId, _l: &[String]) -> anyhow::Result<()> {
        Ok(())
    }

    async fn remove_label(&self, _id: &ExternalId, _l: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        Ok(vec![])
    }

    async fn create_sub_task(
        &self,
        _p: &ExternalId,
        _t: &str,
        _b: &str,
        _l: &[String],
    ) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("child".into()))
    }

    async fn ensure_status_label(&self, _l: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn has_open_issue_with_title(&self, _t: &str, _l: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn is_pr_merged(&self, _b: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
        Ok(Some("bot".into()))
    }

    async fn get_mentions(&self, _s: &str) -> anyhow::Result<Vec<Mention>> {
        Ok(vec![])
    }

    async fn update_status(&self, _id: &ExternalId, _s: Status) -> anyhow::Result<()> {
        Ok(())
    }
}

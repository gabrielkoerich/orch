//! Store helper functions used by the engine/CLI.
//!
//! These helpers operate on the unified SQLite task store (`TaskStore`) but accept
//! `Option<Arc<TaskStore>>` because parts of the engine can run before the store is
//! initialized.

use crate::store::{CostEstimate, MemoryEntry, Task, TaskStore, TokenUsage};
use std::sync::Arc;

/// Load the full `Task` record from the store.
///
/// Returns `None` if the store is unavailable, the task cannot be resolved,
/// or the task row cannot be loaded.
pub async fn opt_store_get_task(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> Option<Task> {
    let s = store.as_ref()?;
    let store_id = s.resolve_task_id(repo, task_id).await.ok()??;
    s.get(store_id).await.ok()
}

/// Write fields to the task store.
///
/// `store` may be None if the store isn't initialized yet.
pub async fn store_set(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    store_fields: &[(&str, serde_json::Value)],
) {
    if let Some(ref store) = store {
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await {
            if let Err(e) = store.set_fields(store_id, store_fields).await {
                tracing::warn!(task_id, error = %e, "store set_fields failed");
            }
        }
    }
}

/// Append a lifecycle activity event to a task timeline.
#[allow(clippy::too_many_arguments)]
pub async fn store_log_activity(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    event_type: &str,
    from_status: Option<&str>,
    to_status: Option<&str>,
    agent: Option<&str>,
    model: Option<&str>,
    details: Option<&serde_json::Value>,
) {
    if let Some(ref s) = store {
        if let Ok(Some(store_id)) = s.resolve_task_id(repo, task_id).await {
            if let Err(e) = s
                .append_activity(
                    store_id,
                    event_type,
                    from_status,
                    to_status,
                    agent,
                    model,
                    details,
                )
                .await
            {
                tracing::warn!(task_id, event_type, error = %e, "store append_activity failed");
            }
        }
    }
}

pub async fn review_session_expected(store: &Arc<TaskStore>, repo: &str, task_id: &str) -> bool {
    match store.resolve_task_id(repo, task_id).await {
        Ok(Some(store_id)) => store
            .get(store_id)
            .await
            .map(|task| task.review_session_expected)
            .unwrap_or(false),
        Ok(None) | Err(_) => false,
    }
}

pub async fn set_review_session_expected(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    expected: bool,
) {
    store_set(
        &Some(Arc::clone(store)),
        repo,
        task_id,
        &[("review_session_expected", serde_json::json!(expected))],
    )
    .await;
}

/// Increment a counter in the task store.
///
/// Uses `store.increment()` for an atomic SQL `field + 1`.
/// Returns the new value, or 0 if the store is unavailable.
pub async fn store_increment(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    field: &str,
) -> u64 {
    if let Some(ref s) = store {
        if let Ok(Some(store_id)) = s.resolve_task_id(repo, task_id).await {
            if let Ok(new_val) = s.increment(store_id, field).await {
                return new_val as u64;
            }
        }
    }
    0
}

/// Reset all task counters in the task store.
pub async fn store_reset_counters(store: &Option<Arc<TaskStore>>, repo: &str, task_id: &str) {
    if let Some(ref store) = store {
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await {
            let _ = store.reset_counters(store_id).await;
        }
    }
}

/// Reset transient failure/retry counters, preserving `review_cycles`.
///
/// Use this after a `RequestChanges` review decision so that per-attempt
/// noise is cleared without undoing the cycle count set by `handle_review_changes`.
pub async fn store_reset_failure_counters(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) {
    if let Some(ref store) = store {
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await {
            let _ = store.reset_failure_counters(store_id).await;
        }
    }
}

/// Get token usage from the store.
pub async fn get_token_usage(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> TokenUsage {
    if let Some(ref s) = store {
        if let Ok(Some(store_id)) = s.resolve_task_id(repo, task_id).await {
            if let Ok(task) = s.get(store_id).await {
                return TokenUsage {
                    input_tokens: task.input_tokens as u64,
                    output_tokens: task.output_tokens as u64,
                };
            }
        }
    }
    TokenUsage::default()
}

/// Get cost estimate from the store.
pub async fn get_cost_estimate(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> CostEstimate {
    if let Some(ref s) = store {
        if let Ok(Some(store_id)) = s.resolve_task_id(repo, task_id).await {
            if let Ok(task) = s.get(store_id).await {
                return CostEstimate {
                    input_cost_usd: task.input_cost_usd,
                    output_cost_usd: task.output_cost_usd,
                    total_cost_usd: task.total_cost_usd,
                };
            }
        }
    }
    CostEstimate::default()
}

pub async fn get_total_tokens(store: &Option<Arc<TaskStore>>, repo: &str, task_id: &str) -> u64 {
    let usage = get_token_usage(store, repo, task_id).await;
    usage.total_tokens()
}

pub async fn get_recent_memory(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    max: usize,
) -> Vec<MemoryEntry> {
    if let Some(ref s) = store {
        if let Ok(Some(store_id)) = s.resolve_task_id(repo, task_id).await {
            return s.recent_memory(store_id, max).await.unwrap_or_default();
        }
    }
    Vec::new()
}

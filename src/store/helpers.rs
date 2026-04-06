//! Store helper functions used by the engine/CLI.
//!
//! These helpers operate on the unified SQLite task store (`TaskStore`) but accept
//! `Option<Arc<TaskStore>>` because parts of the engine can run before the store is
//! initialized.

use crate::store::{CostEstimate, MemoryEntry, Task, TaskStore, TokenUsage};
use anyhow::anyhow;
use std::sync::Arc;

/// Load the full `Task` record from the store.
///
/// Returns `None` if the store is unavailable, the task cannot be resolved,
/// or the task row cannot be loaded.
pub async fn opt_store_get_task_by_id(
    store: &Option<Arc<TaskStore>>,
    store_id: i64,
) -> Option<Task> {
    let s = store.as_ref()?;
    match s.get(store_id).await {
        Ok(task) => Some(task),
        Err(e) => {
            tracing::warn!(store_id, error = %e, "failed to fetch task from store");
            None
        }
    }
}

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
    let store_id = match s.resolve_task_id(repo, task_id).await {
        Ok(Some(id)) => id,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(task_id, error = %e, "failed to resolve task id in store");
            return None;
        }
    };
    opt_store_get_task_by_id(store, store_id).await
}

/// Write fields to the task store.
///
/// `store` may be None if the store isn't initialized yet.
pub async fn store_set_by_id(
    store: &Option<Arc<TaskStore>>,
    store_id: i64,
    store_fields: &[(&str, serde_json::Value)],
) {
    if let Some(ref store) = store {
        if let Err(e) = store.set_fields(store_id, store_fields).await {
            tracing::warn!(store_id, error = %e, "store set_fields failed");
        }
    }
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
        match store.resolve_task_id(repo, task_id).await {
            Ok(Some(store_id)) => {
                if let Err(e) = store.set_fields(store_id, store_fields).await {
                    tracing::warn!(task_id, error = %e, "store set_fields failed");
                }
            }
            Ok(None) => {} // task not in store — no-op is expected
            Err(e) => {
                tracing::warn!(task_id, error = %e, "resolve_task_id failed in store write helper");
            }
        }
    }
}

/// Write fields to the task store, returning an error if the write fails.
///
/// Unlike `store_set`, this variant propagates failures so callers can
/// abort further processing when a critical write (e.g. a watermark) fails.
/// Returns `Ok(())` when `store` is `None` (store not yet initialized).
pub async fn store_set_result_by_id(
    store: &Option<Arc<TaskStore>>,
    store_id: i64,
    store_fields: &[(&str, serde_json::Value)],
) -> anyhow::Result<()> {
    if let Some(ref store) = store {
        store.set_fields(store_id, store_fields).await?;
    }
    Ok(())
}

/// Write fields to the task store, returning an error if the write fails.
///
/// Unlike `store_set`, this variant propagates failures so callers can
/// abort further processing when a critical write (e.g. a watermark) fails.
/// Returns `Ok(())` when `store` is `None` (store not yet initialized).
pub async fn store_set_result(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    store_fields: &[(&str, serde_json::Value)],
) -> anyhow::Result<()> {
    if let Some(ref store) = store {
        let store_id = store
            .resolve_task_id(repo, task_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("task {} not found in store", task_id))?;
        store.set_fields(store_id, store_fields).await?;
    }
    Ok(())
}

/// Touch `updated_at` to now so the stuck-task timer is reset after a session exits.
///
/// No-op when the store is unavailable or the task cannot be resolved.
#[allow(dead_code)]
pub async fn store_touch_updated_at_by_id(store: &Option<Arc<TaskStore>>, store_id: i64) {
    if let Some(ref s) = store {
        if let Err(e) = s.touch_updated_at(store_id).await {
            tracing::warn!(store_id, error = %e, "store touch_updated_at failed");
        }
    }
}

/// Touch `updated_at` to now so the stuck-task timer is reset after a session exits.
///
/// No-op when the store is unavailable or the task cannot be resolved.
pub async fn store_touch_updated_at(store: &Option<Arc<TaskStore>>, repo: &str, task_id: &str) {
    if let Some(ref s) = store {
        match s.resolve_task_id(repo, task_id).await {
            Ok(Some(store_id)) => {
                if let Err(e) = s.touch_updated_at(store_id).await {
                    tracing::warn!(task_id, error = %e, "store touch_updated_at failed");
                }
            }
            Ok(None) => {} // task not in store — no-op is expected
            Err(e) => {
                tracing::warn!(task_id, error = %e, "resolve_task_id failed in store write helper");
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
        match s.resolve_task_id(repo, task_id).await {
            Ok(Some(store_id)) => {
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
            Ok(None) => {} // task not in store — no-op is expected
            Err(e) => {
                tracing::warn!(task_id, error = %e, "resolve_task_id failed in store write helper");
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
/// Returns `Ok(new_value)` on success or `Err(anyhow::Error)` when the
/// store is unavailable, the task cannot be resolved, or the underlying
/// SQL increment failed. Callers must handle errors explicitly — this
/// helper no longer silently returns 0 on error.
pub async fn store_increment_by_id(
    store: &Option<Arc<TaskStore>>,
    store_id: i64,
    field: &str,
) -> anyhow::Result<u64> {
    let s = store
        .as_ref()
        .ok_or_else(|| anyhow!("task store unavailable"))?;
    match s.increment(store_id, field).await {
        Ok(new_val) => Ok(new_val as u64),
        Err(e) => Err(anyhow!("store.increment failed: {}", e)),
    }
}

/// Increment a counter in the task store.
///
/// Uses `store.increment()` for an atomic SQL `field + 1`.
/// Returns `Ok(new_value)` on success or `Err(anyhow::Error)` when the
/// store is unavailable, the task cannot be resolved, or the underlying
/// SQL increment failed. Callers must handle errors explicitly — this
/// helper no longer silently returns 0 on error.
pub async fn store_increment(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    field: &str,
) -> anyhow::Result<u64> {
    let s = store
        .as_ref()
        .ok_or_else(|| anyhow!("task store unavailable"))?;
    match s.resolve_task_id(repo, task_id).await {
        Ok(Some(store_id)) => store_increment_by_id(store, store_id, field).await,
        Ok(None) => Err(anyhow!("task not present in store: {}/{}", repo, task_id)),
        Err(e) => {
            tracing::warn!(task_id, error = %e, "resolve_task_id failed in store write helper");
            Err(anyhow!("resolve_task_id failed: {}", e))
        }
    }
}

/// Reset all task counters in the task store.
pub async fn store_reset_counters(store: &Option<Arc<TaskStore>>, repo: &str, task_id: &str) {
    if let Some(ref store) = store {
        match store.resolve_task_id(repo, task_id).await {
            Ok(Some(store_id)) => {
                if let Err(e) = store.reset_counters(store_id).await {
                    tracing::warn!(task_id, err = %e, "store reset_counters failed");
                }
            }
            Ok(None) => {} // task not in store — no-op is expected
            Err(e) => {
                tracing::warn!(task_id, error = %e, "resolve_task_id failed in store write helper");
            }
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
        match store.resolve_task_id(repo, task_id).await {
            Ok(Some(store_id)) => {
                if let Err(e) = store.reset_failure_counters(store_id).await {
                    tracing::warn!(task_id, err = %e, "failed to reset failure counters after review dispatch");
                }
            }
            Ok(None) => {} // task not in store — no-op is expected
            Err(e) => {
                tracing::warn!(task_id, error = %e, "resolve_task_id failed in store write helper");
            }
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

/// Get both total tokens and cost estimate in a single store read.
pub async fn get_token_summary(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> (u64, CostEstimate) {
    if let Some(ref s) = store {
        if let Ok(Some(store_id)) = s.resolve_task_id(repo, task_id).await {
            if let Ok(task) = s.get(store_id).await {
                let usage = TokenUsage {
                    input_tokens: task.input_tokens as u64,
                    output_tokens: task.output_tokens as u64,
                };
                let total = usage.total_tokens();
                let cost = CostEstimate {
                    input_cost_usd: task.input_cost_usd,
                    output_cost_usd: task.output_cost_usd,
                    total_cost_usd: task.total_cost_usd,
                };
                return (total, cost);
            }
        }
    }
    (0, CostEstimate::default())
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

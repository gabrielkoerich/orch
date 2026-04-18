//! Store helper functions used by the engine/CLI.
//!
//! These helpers operate on the unified SQLite task store (`TaskStore`) but accept
//! `Option<Arc<TaskStore>>` because parts of the engine can run before the store is
//! initialized.

use crate::store::{CostEstimate, MemoryEntry, Task, TaskStore, TokenUsage};
use anyhow::anyhow;
use std::sync::Arc;

/// Result type for helpers that distinguish "not found" from "read error".
pub type StoreResult<T> = anyhow::Result<Option<T>>;

fn token_i64_to_u64_non_negative(tokens: i64) -> u64 {
    u64::try_from(tokens.max(0)).unwrap_or(0)
}

/// Resolve a task's numeric store ID from its external identifier.
///
/// Returns `None` if the store is unavailable, the task is not found, or the
/// lookup fails (with a warning logged in that case).
pub async fn resolve_store_id(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> Option<i64> {
    let s = store.as_ref()?;
    match s.resolve_task_id(repo, task_id).await {
        Ok(Some(id)) => Some(id),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(task_id, error = %e, "failed to resolve task id for store operations");
            None
        }
    }
}

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

/// Read a specific string field from a task record.
pub async fn get_task_field(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    field: &str,
) -> Option<String> {
    let task = opt_store_get_task(store, repo, task_id).await?;
    match field {
        "no_code_last_agent" => Some(task.no_code_last_agent),
        _ => {
            tracing::warn!(task_id, field, "get_task_field: unknown field");
            None
        }
    }
}

/// Read a specific string field from a task record (store always available).
pub async fn get_task_field_direct(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    field: &str,
) -> Option<String> {
    let store_opt: &Option<Arc<TaskStore>> = &Some(store.clone());
    get_task_field(store_opt, repo, task_id, field).await
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

/// Get token usage from the store, surfacing DB read errors.
pub async fn get_token_usage_result(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> StoreResult<TokenUsage> {
    let s = store
        .as_ref()
        .ok_or_else(|| anyhow!("task store unavailable"))?;
    let store_id = s
        .resolve_task_id(repo, task_id)
        .await?
        .ok_or_else(|| anyhow!("task {}/{} not found in store", repo, task_id))?;
    let task = s.get(store_id).await?;
    Ok(Some(TokenUsage {
        input_tokens: token_i64_to_u64_non_negative(task.input_tokens),
        output_tokens: token_i64_to_u64_non_negative(task.output_tokens),
    }))
}

/// Get token usage from the store.
///
/// Returns `TokenUsage::default()` when the store is unavailable or the task is not found.
/// DB read errors are logged as warnings and return the default.
pub async fn get_token_usage(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> TokenUsage {
    match get_token_usage_result(store, repo, task_id).await {
        Ok(Some(usage)) => usage,
        Ok(None) => TokenUsage::default(),
        Err(e) => {
            tracing::warn!(task_id, error = %e, "get_token_usage failed — returning zero usage");
            TokenUsage::default()
        }
    }
}

/// Get cost estimate from the store, surfacing DB read errors.
pub async fn get_cost_estimate_result(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> StoreResult<CostEstimate> {
    let s = store
        .as_ref()
        .ok_or_else(|| anyhow!("task store unavailable"))?;
    let store_id = s
        .resolve_task_id(repo, task_id)
        .await?
        .ok_or_else(|| anyhow!("task {}/{} not found in store", repo, task_id))?;
    let task = s.get(store_id).await?;
    Ok(Some(CostEstimate {
        input_cost_usd: task.input_cost_usd,
        output_cost_usd: task.output_cost_usd,
        total_cost_usd: task.total_cost_usd,
    }))
}

/// Get cost estimate from the store.
///
/// Returns `CostEstimate::default()` when the store is unavailable or the task is not found.
/// DB read errors are logged as warnings and return the default.
pub async fn get_cost_estimate(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> CostEstimate {
    match get_cost_estimate_result(store, repo, task_id).await {
        Ok(Some(cost)) => cost,
        Ok(None) => CostEstimate::default(),
        Err(e) => {
            tracing::warn!(task_id, error = %e, "get_cost_estimate failed — returning zero cost");
            CostEstimate::default()
        }
    }
}

pub async fn get_total_tokens(store: &Option<Arc<TaskStore>>, repo: &str, task_id: &str) -> u64 {
    let usage = get_token_usage(store, repo, task_id).await;
    usage.total_tokens()
}

/// Get both total tokens and cost estimate in a single store read, surfacing DB errors.
pub async fn get_token_summary_result(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> StoreResult<(u64, CostEstimate)> {
    let s = store
        .as_ref()
        .ok_or_else(|| anyhow!("task store unavailable"))?;
    let store_id = s
        .resolve_task_id(repo, task_id)
        .await?
        .ok_or_else(|| anyhow!("task {}/{} not found in store", repo, task_id))?;
    let task = s.get(store_id).await?;
    let usage = TokenUsage {
        input_tokens: token_i64_to_u64_non_negative(task.input_tokens),
        output_tokens: token_i64_to_u64_non_negative(task.output_tokens),
    };
    let total = usage.total_tokens();
    let cost = CostEstimate {
        input_cost_usd: task.input_cost_usd,
        output_cost_usd: task.output_cost_usd,
        total_cost_usd: task.total_cost_usd,
    };
    Ok(Some((total, cost)))
}

/// Get both total tokens and cost estimate in a single store read.
///
/// Returns `(0, CostEstimate::default())` when the store is unavailable or the task is not found.
/// DB read errors are logged as warnings and return the default.
pub async fn get_token_summary(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> (u64, CostEstimate) {
    match get_token_summary_result(store, repo, task_id).await {
        Ok(Some(summary)) => summary,
        Ok(None) => (0, CostEstimate::default()),
        Err(e) => {
            tracing::warn!(task_id, error = %e, "get_token_summary failed — returning zero tokens/cost");
            (0, CostEstimate::default())
        }
    }
}

/// Get recent memory entries for a task, surfacing DB read errors.
pub async fn get_recent_memory_result(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    max: usize,
) -> StoreResult<Vec<MemoryEntry>> {
    let s = store
        .as_ref()
        .ok_or_else(|| anyhow!("task store unavailable"))?;
    let store_id = s
        .resolve_task_id(repo, task_id)
        .await?
        .ok_or_else(|| anyhow!("task {}/{} not found in store", repo, task_id))?;
    let entries = s.recent_memory(store_id, max).await?;
    Ok(Some(entries))
}

/// Get recent memory entries for a task.
///
/// Returns an empty vec when the store is unavailable or the task is not found.
/// DB read errors are logged as warnings and return an empty vec.
pub async fn get_recent_memory(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    max: usize,
) -> Vec<MemoryEntry> {
    match get_recent_memory_result(store, repo, task_id, max).await {
        Ok(Some(entries)) => entries,
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::warn!(task_id, error = %e, "get_recent_memory failed — returning empty memory");
            Vec::new()
        }
    }
}

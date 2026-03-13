-- KV store, task metrics, and rate limits tables.
-- Ported from rusqlite (db.rs) to sqlx to consolidate all storage in one pool.

CREATE TABLE IF NOT EXISTS kv (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS task_metrics (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id          TEXT NOT NULL,
    agent            TEXT NOT NULL,
    model            TEXT DEFAULT NULL,
    complexity       TEXT DEFAULT NULL,
    outcome          TEXT NOT NULL,
    duration_seconds REAL DEFAULT 0,
    started_at       TEXT NOT NULL,
    completed_at     TEXT NOT NULL,
    attempts         INTEGER DEFAULT 1,
    files_changed    INTEGER DEFAULT 0,
    error_type       TEXT DEFAULT NULL,
    input_tokens     INTEGER DEFAULT NULL,
    output_tokens    INTEGER DEFAULT NULL,
    input_cost_usd   REAL DEFAULT NULL,
    output_cost_usd  REAL DEFAULT NULL,
    total_cost_usd   REAL DEFAULT NULL,
    created_at       TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_task_metrics_task_id ON task_metrics(task_id);
CREATE INDEX IF NOT EXISTS idx_task_metrics_agent ON task_metrics(agent);
CREATE INDEX IF NOT EXISTS idx_task_metrics_completed_at ON task_metrics(completed_at);
CREATE INDEX IF NOT EXISTS idx_task_metrics_outcome ON task_metrics(outcome);

CREATE TABLE IF NOT EXISTS rate_limits (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    agent       TEXT NOT NULL,
    limit_type  TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    task_id     TEXT DEFAULT NULL,
    created_at  TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_rate_limits_agent ON rate_limits(agent);
CREATE INDEX IF NOT EXISTS idx_rate_limits_occurred_at ON rate_limits(occurred_at);

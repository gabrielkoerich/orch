-- Per-task activity timeline for debugging and lifecycle forensics.
CREATE TABLE IF NOT EXISTS task_activity (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES tasks(id),
    timestamp TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    event_type TEXT NOT NULL,
    from_status TEXT,
    to_status TEXT,
    agent TEXT,
    model TEXT,
    details TEXT DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_task_activity_task_id_timestamp
    ON task_activity(task_id, timestamp, id);

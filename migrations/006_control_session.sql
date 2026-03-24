-- Control session: full conversation history
CREATE TABLE IF NOT EXISTS control_messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL DEFAULT 'default',  -- multi-session support (profiles)
    role            TEXT NOT NULL,       -- 'user', 'assistant'
    channel         TEXT NOT NULL,       -- 'cli', 'telegram', 'discord'
    channel_thread  TEXT,                -- thread/topic ID for reply routing
    content         TEXT NOT NULL,       -- full message text
    summary         TEXT,                -- one-line summary for context assembly
    model           TEXT,                -- which model responded (NULL for user)
    agent           TEXT,                -- which agent CLI was used
    tokens_used     INTEGER,
    cost_usd        REAL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_control_messages_session
    ON control_messages(session_id, created_at);
CREATE INDEX IF NOT EXISTS idx_control_messages_created
    ON control_messages(created_at);

-- Persistent key-value state for control session
CREATE TABLE IF NOT EXISTS control_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

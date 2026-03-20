-- Control session: full conversation history
CREATE TABLE IF NOT EXISTS control_messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
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

CREATE INDEX IF NOT EXISTS idx_control_messages_created
    ON control_messages(created_at);
CREATE INDEX IF NOT EXISTS idx_control_messages_role
    ON control_messages(role, created_at);

-- Persistent key-value state for control session
-- (Originally appended to 006_control_session.sql which had already been applied)
CREATE TABLE IF NOT EXISTS control_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

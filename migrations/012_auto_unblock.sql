-- Add auto-unblock tracking fields for recoverable failures.
ALTER TABLE tasks ADD COLUMN auto_unblock_count INTEGER DEFAULT 0;
ALTER TABLE tasks ADD COLUMN auto_unblock_last_at TEXT DEFAULT '';

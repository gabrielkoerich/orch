-- Add repo column to task_metrics for per-project stats
ALTER TABLE task_metrics ADD COLUMN repo TEXT DEFAULT '';

-- Channel notification subscriptions
CREATE TABLE IF NOT EXISTS channel_subscriptions (
    channel   TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    repo      TEXT NOT NULL,
    PRIMARY KEY (channel, thread_id, repo)
);

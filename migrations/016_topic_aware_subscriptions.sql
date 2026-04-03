-- Add topic_id column to channel_subscriptions for topic-aware notification delivery
ALTER TABLE channel_subscriptions ADD COLUMN topic_id TEXT DEFAULT '';

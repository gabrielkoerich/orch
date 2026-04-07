-- Add per-reviewer timestamp watermark to replace global last_review_ts
ALTER TABLE tasks ADD COLUMN review_ts_map TEXT DEFAULT '{}';
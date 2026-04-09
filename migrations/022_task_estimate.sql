-- Add Fibonacci effort estimate field to tasks.
-- 0 means not provided (legacy rows or non-LLM routes).
ALTER TABLE tasks ADD COLUMN estimate INTEGER NOT NULL DEFAULT 0;

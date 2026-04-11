-- Track which agent produced the last no-code failure for a task.
-- Used to detect same-agent loops: if the router selects the same agent
-- that just caused a no-code failure, the task is blocked immediately
-- instead of re-running the same agent into the same worktree state.
ALTER TABLE tasks ADD COLUMN no_code_last_agent TEXT NOT NULL DEFAULT '';

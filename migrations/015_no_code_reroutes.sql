-- Track how many times an external task has been re-routed because the agent
-- completed without producing any code changes. Used as a circuit-breaker:
-- when the count reaches workflow.max_reroute_attempts the task is blocked
-- for human review instead of being re-dispatched indefinitely.
ALTER TABLE tasks ADD COLUMN no_code_reroutes INTEGER NOT NULL DEFAULT 0;

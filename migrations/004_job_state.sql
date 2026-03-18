-- Job runtime state, separated from the declarative .orch.yml config.
-- Keyed by (repo, job_id) so jobs with the same name in different projects don't collide.
CREATE TABLE IF NOT EXISTS job_state (
    repo       TEXT NOT NULL,
    job_id     TEXT NOT NULL,
    last_run   TEXT,           -- ISO 8601 timestamp of last execution
    last_task_status TEXT,     -- "new", "done", "failed", etc.
    active_task_id   TEXT,     -- e.g. "internal:42" or GitHub issue number
    PRIMARY KEY (repo, job_id)
);

-- Additional composite index for the hot query path used by sync tick and internal task lookups.
-- list_internal_by_status() filters on (repo, origin='internal', status) every ~10s.
-- A three-column index avoids a full-table scan on deployments with thousands of tasks.
CREATE INDEX IF NOT EXISTS idx_tasks_repo_origin_status ON tasks(repo, origin, status);

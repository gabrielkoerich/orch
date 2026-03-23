-- Hot path: list_by_status() filters (repo, status) every sync tick
CREATE INDEX IF NOT EXISTS idx_tasks_repo_status ON tasks(repo, status);

-- Global status queries (orch task list --status, recovery tick, cleanup pruning)
CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);

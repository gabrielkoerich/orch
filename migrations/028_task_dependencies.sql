CREATE TABLE task_dependencies (
    task_id            INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    depends_on_task_id INTEGER REFERENCES tasks(id) ON DELETE CASCADE,
    depends_on_repo    TEXT NOT NULL,
    depends_on_number  INTEGER NOT NULL,
    source             TEXT NOT NULL CHECK (source IN ('github', 'body', 'cli')),
    created_at         TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (task_id, depends_on_repo, depends_on_number)
);
CREATE INDEX idx_task_dependencies_depends_on ON task_dependencies(depends_on_task_id);

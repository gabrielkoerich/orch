-- Unified tasks table: single source of truth for all tasks (internal + external).
-- Replaces: internal_tasks table (SQLite), sidecar JSON files, GitHub labels as status authority.

CREATE TABLE IF NOT EXISTS tasks (
    -- Identity
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    external_id     TEXT,                -- GitHub issue #, Linear ID, etc. NULL for internal
    repo            TEXT NOT NULL,       -- "owner/repo" scoping
    origin          TEXT NOT NULL DEFAULT 'internal', -- 'github', 'linear', 'internal'

    -- Core
    title           TEXT NOT NULL,
    body            TEXT DEFAULT '',
    status          TEXT NOT NULL DEFAULT 'new',
    source          TEXT DEFAULT '',     -- 'cron', 'mention', 'manual', 'webhook'
    source_id       TEXT DEFAULT '',     -- dedup key
    author          TEXT DEFAULT '',
    url             TEXT DEFAULT '',     -- external URL
    labels          TEXT DEFAULT '[]',   -- JSON array (cached from external)

    -- Routing
    agent           TEXT,
    model           TEXT,
    complexity      TEXT DEFAULT 'medium',
    route_reason    TEXT DEFAULT '',
    agent_profile   TEXT DEFAULT '',     -- JSON
    selected_skills TEXT DEFAULT '',
    route_attempts  INTEGER DEFAULT 0,

    -- Execution
    attempts        INTEGER DEFAULT 0,
    branch          TEXT DEFAULT '',
    worktree        TEXT DEFAULT '',
    worktree_cleaned INTEGER DEFAULT 0,
    summary         TEXT DEFAULT '',
    last_error      TEXT DEFAULT '',
    parent_id       INTEGER REFERENCES tasks(id),
    block_reason    TEXT,

    -- PR Review
    pr_number       INTEGER,
    pr_review_context TEXT DEFAULT '',
    last_review_ts  TEXT DEFAULT '',
    last_comment_review_ts TEXT DEFAULT '',
    merge_conflict_retries INTEGER DEFAULT 0,
    ci_merge_failures INTEGER DEFAULT 0,
    pr_create_failures INTEGER DEFAULT 0,
    review_agent_failures INTEGER DEFAULT 0,
    review_cycles   INTEGER DEFAULT 0,

    -- Tokens & Cost
    input_tokens    INTEGER DEFAULT 0,
    output_tokens   INTEGER DEFAULT 0,
    input_cost_usd  REAL DEFAULT 0.0,
    output_cost_usd REAL DEFAULT 0.0,
    total_cost_usd  REAL DEFAULT 0.0,

    -- Recovery
    model_reroute_chain  TEXT DEFAULT '',
    limit_reroute_chain  TEXT DEFAULT '',
    budget_warning       TEXT DEFAULT '',
    budget_exceeded      INTEGER DEFAULT 0,

    -- Structured data (JSON)
    memory          TEXT DEFAULT '[]',   -- Vec<MemoryEntry>
    delegations     TEXT DEFAULT '[]',

    -- Timestamps
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),

    UNIQUE(repo, external_id)
);

CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
CREATE INDEX IF NOT EXISTS idx_tasks_repo ON tasks(repo);
CREATE INDEX IF NOT EXISTS idx_tasks_repo_status ON tasks(repo, status);
CREATE INDEX IF NOT EXISTS idx_tasks_external_id ON tasks(external_id);
CREATE INDEX IF NOT EXISTS idx_tasks_origin ON tasks(origin);
CREATE INDEX IF NOT EXISTS idx_tasks_parent_id ON tasks(parent_id);

-- Per-attempt audit log: full prompt, command, output for every agent run
CREATE TABLE IF NOT EXISTS task_runs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id         INTEGER NOT NULL REFERENCES tasks(id),
    attempt         INTEGER NOT NULL,       -- 1-indexed attempt number
    run_type        TEXT NOT NULL,           -- 'agent', 'review', 'route'

    -- What we sent
    agent           TEXT NOT NULL,           -- claude, codex, opencode
    model           TEXT DEFAULT '',
    command         TEXT DEFAULT '',         -- full shell command invoked
    prompt          TEXT DEFAULT '',         -- system prompt / task prompt sent
    env_vars        TEXT DEFAULT '{}',       -- JSON: relevant env vars

    -- What we got back
    exit_code       INTEGER,
    stdout          TEXT DEFAULT '',         -- full agent output
    stderr          TEXT DEFAULT '',         -- stderr capture
    parsed_response TEXT DEFAULT '',         -- JSON: our parsed interpretation
    outcome         TEXT DEFAULT '',         -- 'success', 'failed', 'timeout', 'rate_limit', 'parse_error'
    error           TEXT DEFAULT '',         -- error message if failed

    -- Cost
    input_tokens    INTEGER DEFAULT 0,
    output_tokens   INTEGER DEFAULT 0,
    total_cost_usd  REAL DEFAULT 0.0,
    duration_secs   REAL DEFAULT 0.0,

    -- Timestamps
    started_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    completed_at    TEXT DEFAULT NULL,

    UNIQUE(task_id, attempt, run_type)
);

CREATE INDEX IF NOT EXISTS idx_task_runs_task_id ON task_runs(task_id);
CREATE INDEX IF NOT EXISTS idx_task_runs_outcome ON task_runs(outcome);

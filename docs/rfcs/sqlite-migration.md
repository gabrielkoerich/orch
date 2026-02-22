# SQLite Migration Plan

## Why

Current yq/YAML approach (`tasks.yml`) has scaling and reliability issues:

- **Performance**: 30+ yq calls per task during gh_push sync. yq parses the full YAML each time. With 50+ tasks, sync takes seconds per tick.
- **Concurrency**: File locking via `mkdir` is fragile. Two processes can race between lock check and write. No atomic transactions.
- **Querying**: Filtering requires `yq select` chains that are hard to read and slow. No joins, no indexes.
- **Data integrity**: No schema enforcement. Fields can be missing, wrong type, or duplicated (e.g. the dual-task id=1 bug).
- **Debugging**: YAML diffs are noisy. Hard to inspect state without yq.

## What changes

Replace `~/.orchestrator/tasks.yml` with `~/.orchestrator/orchestrator.db` (SQLite).

### Schema

```sql
CREATE TABLE tasks (
  id INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  body TEXT DEFAULT '',
  status TEXT NOT NULL DEFAULT 'new',
  agent TEXT,
  agent_model TEXT,
  agent_profile TEXT,
  complexity TEXT,
  parent_id INTEGER REFERENCES tasks(id),
  route_reason TEXT,
  route_warning TEXT,
  summary TEXT,
  reason TEXT,
  needs_help INTEGER DEFAULT 0,
  attempts INTEGER DEFAULT 0,
  last_error TEXT,
  prompt_hash TEXT,
  last_comment_hash TEXT,
  retry_at TEXT,
  review_decision TEXT,
  review_notes TEXT,
  dir TEXT,
  branch TEXT,
  worktree TEXT,
  worktree_cleaned INTEGER DEFAULT 0,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  -- GitHub sync fields
  gh_issue_number INTEGER,
  gh_state TEXT,
  gh_url TEXT,
  gh_updated_at TEXT,
  gh_synced_at TEXT,
  gh_last_feedback_at TEXT,
  gh_project_item_id TEXT,
  gh_archived INTEGER DEFAULT 0
);

CREATE TABLE task_labels (
  task_id INTEGER REFERENCES tasks(id),
  label TEXT NOT NULL,
  PRIMARY KEY (task_id, label)
);

CREATE TABLE task_history (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id INTEGER REFERENCES tasks(id),
  ts TEXT NOT NULL,
  status TEXT,
  note TEXT
);

CREATE TABLE task_files (
  task_id INTEGER REFERENCES tasks(id),
  file_path TEXT NOT NULL,
  PRIMARY KEY (task_id, file_path)
);

CREATE TABLE task_children (
  parent_id INTEGER REFERENCES tasks(id),
  child_id INTEGER REFERENCES tasks(id),
  PRIMARY KEY (parent_id, child_id)
);

CREATE TABLE task_accomplished (
  task_id INTEGER REFERENCES tasks(id),
  item TEXT NOT NULL
);

CREATE TABLE task_remaining (
  task_id INTEGER REFERENCES tasks(id),
  item TEXT NOT NULL
);

CREATE TABLE task_blockers (
  task_id INTEGER REFERENCES tasks(id),
  item TEXT NOT NULL
);

CREATE TABLE jobs (
  id TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  schedule TEXT NOT NULL,
  type TEXT NOT NULL DEFAULT 'task',
  command TEXT,
  body TEXT DEFAULT '',
  labels TEXT DEFAULT '',
  agent TEXT,
  dir TEXT,
  enabled INTEGER DEFAULT 1,
  active_task_id INTEGER,
  last_run TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE config (
  key TEXT PRIMARY KEY,
  value TEXT
);

-- Indexes for common queries
CREATE INDEX idx_tasks_status ON tasks(status);
CREATE INDEX idx_tasks_dir ON tasks(dir);
CREATE INDEX idx_tasks_gh_issue ON tasks(gh_issue_number);
CREATE INDEX idx_tasks_updated ON tasks(updated_at);
CREATE INDEX idx_history_task ON task_history(task_id);
```

### Shell wrapper: `scripts/db.sh`

Thin wrapper sourced by lib.sh. Replaces `task_field`, `task_set`, `task_count`, `task_tsv`, `create_task_entry`, `append_history`.

```bash
DB_PATH="${ORCH_HOME}/orchestrator.db"

db() {
  sqlite3 -separator $'\t' "$DB_PATH" "$@"
}

db_json() {
  sqlite3 -json "$DB_PATH" "$@"
}

task_field() {
  local id="$1" field="$2"
  db "SELECT $field FROM tasks WHERE id = $id"
}

task_set() {
  local id="$1" field="$2" value="$3"
  db "UPDATE tasks SET $field = '$value', updated_at = datetime('now') WHERE id = $id"
}

task_count() {
  local status="$1"
  db "SELECT COUNT(*) FROM tasks WHERE status = '$status'"
}

task_list() {
  local where="${1:-1=1}"
  db "SELECT id, status, agent, gh_issue_number, title FROM tasks WHERE $where ORDER BY id"
}

create_task() {
  local title="$1" body="${2:-}" dir="${3:-$PROJECT_DIR}"
  db "INSERT INTO tasks (title, body, dir, status, created_at, updated_at)
      VALUES ('$(sql_escape "$title")', '$(sql_escape "$body")', '$dir', 'new', datetime('now'), datetime('now'))
      RETURNING id"
}

append_history() {
  local id="$1" status="$2" note="$3"
  db "INSERT INTO task_history (task_id, ts, status, note)
      VALUES ($id, datetime('now'), '$status', '$(sql_escape "$note")')"
}

sql_escape() {
  printf '%s' "$1" | sed "s/'/''/g"
}
```

### Migration script: `scripts/migrate_to_sqlite.sh`

One-time migration from tasks.yml + jobs.yml to SQLite:

```bash
#!/usr/bin/env bash
# Reads tasks.yml and jobs.yml, creates orchestrator.db
set -euo pipefail
source "$(dirname "$0")/lib.sh"

DB_PATH="${ORCH_HOME}/orchestrator.db"
if [ -f "$DB_PATH" ]; then
  echo "Database already exists at $DB_PATH"
  exit 1
fi

# Create schema
sqlite3 "$DB_PATH" < "$(dirname "$0")/schema.sql"

# Migrate tasks
yq -o=json '.tasks[]' "$TASKS_PATH" | while read -r task_json; do
  # Insert each task from JSON
  ...
done

# Migrate jobs
yq -o=json '.jobs[]' "$JOBS_PATH" | while read -r job_json; do
  ...
done

echo "Migrated to $DB_PATH"
echo "Backup: $TASKS_PATH.bak"
cp "$TASKS_PATH" "${TASKS_PATH}.bak"
```

## Migration strategy

1. **Phase 1**: Add `db.sh` wrapper alongside existing yq helpers. New features use SQLite, old code unchanged.
2. **Phase 2**: Migrate `tasks.yml` → SQLite. Run both in parallel with consistency checks.
3. **Phase 3**: Remove yq dependency for task management. Keep yq only for config/project YAML files.
4. **Phase 4**: Optionally migrate `config.yml` and `jobs.yml` to SQLite tables.

## What stays YAML

- `.orchestrator.yml` — per-project config (lives in repo, checked into git)
- `config.yml` — global orchestrator config (small, rarely changed)
- Prompt templates — markdown files

## Benefits

- **10-100x faster** for task queries (indexed SQLite vs full YAML parse)
- **Atomic writes** — no more file locking bugs
- **Real queries** — `SELECT * FROM tasks WHERE status = 'new' AND dir = '...' ORDER BY id`
- **Schema enforcement** — NOT NULL, types, foreign keys
- **Inspectable** — `sqlite3 orchestrator.db` for ad-hoc queries
- **Concurrency** — SQLite WAL mode handles concurrent readers/writers
- **History** — proper task_history table instead of inline YAML arrays

## Dependencies

- `sqlite3` — available on macOS by default, `apt install sqlite3` on Linux
- Remove `yq` as a hard dependency for task operations (keep for config)

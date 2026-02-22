# Orchestrator v2: Beads + tmux + Project-Local Architecture

> **Status**: Planning
> **Branch**: `feat/beads-integration`
> **Goal**: Replace the SQLite↔GitHub bidirectional sync with Beads as per-project task storage, project-local worktrees, tmux for agent observability, and one-way GitHub publish.

---

## Table of Contents

1. [Pain Points & What Fixes Them](#1-pain-points--what-fixes-them)
2. [Requirements](#2-requirements)
3. [Architecture Overview](#3-architecture-overview)
4. [Detailed Design](#4-detailed-design)
5. [File-by-File Analysis](#5-file-by-file-analysis)
6. [Implementation Checklist](#6-implementation-checklist)
7. [Migration Path](#7-migration-path)
8. [Open Questions](#8-open-questions)

---

## 1. Pain Points & What Fixes Them

### Sync Layer Bugs (root cause: bidirectional sync)

- [x] **Comment spam** — 70+ duplicate comments on issues when dedup hash fails under concurrency
  - *Fix*: One-way publish. Beads is truth, GitHub is display. No dedup needed.
- [x] **Cross-project issue creation** — `gh_push` created issues in the wrong repo
  - *Fix*: Per-project Beads. Each project knows its own repo. No global task table.
- [x] **Timing bugs** — `updated_at` vs `gh_synced_at` race conditions
  - *Fix*: No sync timestamps. Beads publish is idempotent fire-and-forget.
- [x] **Status divergence** — local says `done`, GitHub says `open`
  - *Fix*: Beads is truth. GitHub labels updated on publish. If they drift, doesn't matter.
- [x] **Dirty tracking complexity** — `gh_synced_status`, `gh_state`, `last_comment_hash`, `gh_updated_at`
  - *Fix*: All deleted. No dirty tracking needed.
- [x] **Infinite reroute loops** — agent model mismatch persisted in DB across reroutes
  - *Fix*: Already fixed in v0.39.3. Beads doesn't have this architecture.
- [x] **Stuck timer false positives** — SQLite datetime format mismatch (56-year detection)
  - *Fix*: Beads uses its own timestamp format. No date parsing hacks.

### Global State Confusion

- [x] **All tasks in one DB** — orchestrator + oblivion tasks share `~/.orchestrator/orchestrator.db`
  - *Fix*: Per-project `.beads/`. Each project has its own task graph.
- [x] **Wrong repo references** — tasks inherit wrong `dir` when created inside worktrees
  - *Fix*: Tasks belong to the project's `.beads/`. No `dir` field needed.
- [x] **Cross-project queries** — `db_task_projects` scans all tasks to find unique dirs
  - *Fix*: Global config lists projects. Each project queried independently.

### Worktree Problems

- [x] **Worktree sprawl** — `~/.orchestrator/worktrees/` disconnected from projects
  - *Fix*: Project-local worktrees at `/path/to/project/.orchestrator/worktrees/`.
- [x] **Hard to find/clean up** — worktrees scattered in hidden global directory
  - *Fix*: `ls .orchestrator/worktrees/` shows all work for THIS project.
- [x] **Nested worktree bug** — agents inherit worktree path as project dir
  - *Fix*: Worktrees are siblings of `.git/`, clear ownership.

### Agent Observability

- [x] **Black box execution** — agents run in `-p` mode, zero visibility until done
  - *Fix*: tmux sessions. Attach to watch, interact, or just stream output.
- [x] **No way to guide stuck agents** — if agent loops, must wait for timeout
  - *Fix*: `orch task attach` — type instructions to the agent mid-run.
- [x] **Debugging requires log archaeology** — stderr files and log tailing
  - *Fix*: `orch task stream` — live output. `orch task resume` — continue failed conversation.

### Operational Pain

- [x] **~2,000 lines of sync code** — `gh_push.sh` (643), `gh_pull.sh` (203), `gh_sync.sh` (27), `db.sh` (948)
  - *Fix*: Delete all. Replace with ~200 lines of Beads helpers + ~100 lines of GitHub publish.
- [x] **SQLite WAL gotchas** — busy_timeout per-connection, IFS tab collapsing, boolean integers
  - *Fix*: Beads handles storage. No raw SQL.
- [x] **YAML migration baggage** — `migrate_to_sqlite.sh` still shipped
  - *Fix*: Delete. Clean slate.
- [x] **Review agent same as task agent** — no way to ensure independent review
  - *Fix*: Config `review_agent` per project, enforced to be different from `default_agent`.

---

## 2. Requirements

### Must Have

1. **Beads as hidden task layer** — user never runs `bd` directly. Orchestrator wraps all Beads commands.
2. **Per-project everything** — `.beads/`, `.orchestrator/config.yml`, `.orchestrator/worktrees/` inside each project.
3. **Global orchestrator view** — `~/.orchestrator/config.yml` lists all managed projects. `orch status` aggregates across projects.
4. **One-way GitHub publish** — Beads → GitHub labels + comments. No sync back.
5. **GitHub issue import** — new issues created by humans get imported into Beads.
6. **GitHub mentions listener** — detect `@orchestrator` or `@codex review` mentions in issue/PR comments, create tasks or trigger actions.
7. **Different review agents** — review agent MUST be different from the agent that did the task. Configurable per project.
8. **Auto-merge PRs** — when review passes and CI is green, auto-merge with configured strategy (squash/merge/rebase).
9. **tmux agent sessions** — every agent run gets a tmux session for observability.
10. **Project-local worktrees** — at `$PROJECT_DIR/.orchestrator/worktrees/$BRANCH/`.
11. **Keep current system running** — migration must be incremental. Current system (v0.39.3) continues operating during development.

### Nice to Have

12. **GitHub App** — replace polling with webhooks for instant task dispatch.
13. **Beads hooks** — `bd` post-update hooks trigger GitHub publish automatically.
14. **Memory decay** — Beads auto-summarizes closed tasks to keep agent context lean.
15. **`orch task resume`** — resume a failed Claude conversation from where it left off.
16. **`orch task attach`** — interactively guide a running agent via tmux.

---

## 3. Architecture Overview

### Current Architecture

```
~/.orchestrator/
  orchestrator.db          ← Global SQLite: ALL tasks, ALL projects
  config.yml               ← Global + project config merged
  jobs.yml                 ← (migrated to SQLite)
  worktrees/               ← ALL worktrees for ALL projects
    orchestrator/branch/
    oblivion/branch/
  .orchestrator/           ← Runtime state (logs, locks, prompts)

Sync: SQLite ←→ GitHub Issues (bidirectional, 870 lines)
      gh_push.sh (643) + gh_pull.sh (203) + gh_sync.sh (27)
```

### New Architecture

```
/path/to/project/                          ← User-managed project
├── .beads/                                ← Task graph (git-tracked)
│   ├── issues.jsonl                       ← Tasks: status, deps, history
│   ├── messages.jsonl                     ← Agent output, comments
│   └── config.yaml                        ← Beads config (custom statuses)
├── .orchestrator/
│   ├── config.yml                         ← Project-specific orchestrator config
│   └── worktrees/                         ← Agent worktrees for THIS project
│       ├── gh-task-42-add-auth/
│       └── gh-task-86-fix-tests/
├── .git/
└── src/ ...

~/.orchestrator/                           ← Global orchestrator state
├── config.yml                             ← Global config (API keys, model_map)
├── projects.yml                           ← List of managed projects + paths
├── jobs.yml                               ← Cron jobs (global, cross-project)
└── state/                                 ← Ephemeral runtime
    ├── orchestrator.log
    ├── orchestrator.pid
    ├── locks/
    └── prompts/                           ← Rendered prompt cache

Sync: Beads → GitHub (one-way publish, ~200 lines)
      gh_publish.sh (new, replaces gh_push.sh)
Import: GitHub → Beads (one-way import, ~100 lines)
      gh_import.sh (new, replaces gh_pull.sh)
```

### For bare-cloned projects (`orch project add`)

```
~/.orchestrator/projects/owner/repo.git/   ← Bare clone
├── .beads/                                ← Task graph inside bare repo
├── .orchestrator/
│   ├── config.yml
│   └── worktrees/
└── (bare git objects)
```

### Data Flow

```
                    ┌─────────────┐
                    │   Human     │
                    │ (GitHub UI) │
                    └──────┬──────┘
                           │ creates issue / comments
                           ▼
                    ┌─────────────┐
                    │   GitHub    │  ← Display layer
                    │   Issues    │  ← Labels = status
                    └──────┬──────┘  ← Comments = agent output
                           │
              ┌────────────┼────────────┐
              │ import     │            │ publish
              ▼            │            ▲
        ┌──────────┐       │      ┌──────────┐
        │ gh_import│       │      │gh_publish │
        └────┬─────┘       │      └─────┬────┘
             │             │            │
             ▼             │            ▲
        ┌──────────────────┴────────────────┐
        │          Beads (.beads/)           │  ← Source of truth
        │  tasks, deps, status, history     │
        └──────────────────┬────────────────┘
                           │
                    ┌──────┴──────┐
                    │ Orchestrator│
                    │  (serve.sh) │
                    └──────┬──────┘
                           │ polls bd ready
                           ▼
                    ┌─────────────┐
                    │ Agent (tmux)│  ← claude/codex/opencode
                    │ in worktree │
                    └─────────────┘
```

---

## 4. Detailed Design

### 4.1 Beads as Hidden Task Layer

The user never runs `bd` directly. All Beads operations go through orchestrator wrapper functions in `scripts/beads.sh`:

```bash
# scripts/beads.sh — Beads wrapper functions

# All functions operate on the current project's .beads/ directory
# PROJECT_DIR must be set before calling

bd_init() {
  (cd "$PROJECT_DIR" && bd init --no-db)
  # Configure custom statuses matching orchestrator workflow
  (cd "$PROJECT_DIR" && bd config set statuses "new,routed,in_progress,blocked,needs_review,in_review,done")
}

bd_create() {
  local title="$1" body="${2:-}" parent="${3:-}"
  local args=(create "$title")
  [ -n "$body" ] && args+=(--body "$body")
  [ -n "$parent" ] && args+=(--parent "$parent")
  (cd "$PROJECT_DIR" && bd "${args[@]}" --format json)
}

bd_ready() {
  # Returns tasks with no open blockers in status=new or status=routed
  (cd "$PROJECT_DIR" && bd ready --format json 2>/dev/null) || echo '[]'
}

bd_claim() {
  local task_id="$1" agent="$2"
  (cd "$PROJECT_DIR" && bd update "$task_id" --status in_progress --field agent="$agent")
}

bd_update() {
  local task_id="$1"; shift
  (cd "$PROJECT_DIR" && bd update "$task_id" "$@")
}

bd_show() {
  local task_id="$1"
  (cd "$PROJECT_DIR" && bd show "$task_id" --format json)
}

bd_list() {
  (cd "$PROJECT_DIR" && bd list --format json "$@")
}

bd_log() {
  local task_id="$1" message="$2"
  (cd "$PROJECT_DIR" && bd message "$task_id" "$message")
}

bd_tree() {
  (cd "$PROJECT_DIR" && bd tree "$@")
}

bd_context() {
  local budget="${1:-4000}"
  (cd "$PROJECT_DIR" && bd context --budget "$budget" 2>/dev/null) || echo ""
}
```

### 4.2 Project Registry (`~/.orchestrator/projects.yml`)

```yaml
# ~/.orchestrator/projects.yml
projects:
  - name: orchestrator
    path: /Users/gb/Projects/orchestrator
    repo: gabrielkoerich/orchestrator
    type: user-managed          # user cloned, runs orch init

  - name: oblivion
    path: /Users/gb/Projects/oblivion
    repo: gabrielkoerich/oblivion
    type: user-managed

  - name: some-lib
    path: ~/.orchestrator/projects/owner/some-lib.git
    repo: owner/some-lib
    type: orch-managed           # bare clone via orch project add
```

### 4.3 Project-Local Config (`.orchestrator/config.yml`)

Each project has its own config inside `.orchestrator/`:

```yaml
# /path/to/project/.orchestrator/config.yml
repo: gabrielkoerich/orchestrator
default_agent: claude
review_agent: codex              # MUST be different from default_agent
enable_review: true
merge_strategy: squash
publish_to_github: true
import_from_github: true

agents:
  claude:
    allowed_tools:
      - "Bash(bun:*)"
      - "Bash(anchor:*)"
  codex:
    sandbox: full-auto
```

### 4.4 One-Way GitHub Publish (`gh_publish.sh`)

Replaces `gh_push.sh` (643 lines) with a simpler one-way flow (~200 lines):

```bash
# For each task in beads that needs publishing
for task in $(bd_list --format json | jq -c '.[]'); do
  TASK_ID=$(jq -r '.id' <<< "$task")
  STATUS=$(jq -r '.status' <<< "$task")
  GH_ISSUE=$(jq -r '.fields.gh_issue // empty' <<< "$task")

  # Create issue if none exists
  if [ -z "$GH_ISSUE" ]; then
    TITLE=$(jq -r '.title' <<< "$task")
    BODY=$(jq -r '.description // ""' <<< "$task")
    GH_ISSUE=$(gh issue create --repo "$REPO" --title "$TITLE" --body "$BODY" \
      --label "status:$STATUS" -q '.number')
    bd_update "$TASK_ID" --field gh_issue="$GH_ISSUE"
  fi

  # Update labels to match status
  gh issue edit "$GH_ISSUE" --repo "$REPO" \
    --remove-label "status:new,status:routed,status:in_progress,status:blocked,status:needs_review,status:in_review,status:done" \
    --add-label "status:$STATUS" 2>/dev/null || true
done
```

Key differences from current `gh_push.sh`:
- No dirty tracking (`gh_synced_at`, `gh_synced_status`, `last_comment_hash`)
- No comment dedup logic (post once, done)
- No GraphQL project field mutations (optional, separate script)
- No `db_load_task` / `db_should_skip_comment` / `db_store_comment_hash`

### 4.5 GitHub Import (`gh_import.sh`)

Replaces `gh_pull.sh` (203 lines) with a simpler one-way import (~100 lines):

```bash
# Fetch open issues not yet in beads
ISSUES=$(gh api "repos/$REPO/issues" -f state=open -f per_page=50 --paginate)

for issue in $(echo "$ISSUES" | jq -c '.[]'); do
  ISSUE_NUM=$(jq -r '.number' <<< "$issue")

  # Check if already imported
  EXISTING=$(bd_list --field gh_issue="$ISSUE_NUM" 2>/dev/null | jq -r '.[0].id // empty')
  [ -n "$EXISTING" ] && continue

  TITLE=$(jq -r '.title' <<< "$issue")
  BODY=$(jq -r '.body // ""' <<< "$issue")

  # Import as new task
  TASK_ID=$(bd_create "$TITLE" "$BODY" | jq -r '.id')
  bd_update "$TASK_ID" --field gh_issue="$ISSUE_NUM"
done
```

### 4.6 GitHub Mentions Listener

New capability — detect and act on GitHub mentions:

```bash
# scripts/gh_mentions.sh
# Check for @mentions in issue/PR comments since last check

SINCE=$(cat "$STATE_DIR/mentions_since" 2>/dev/null || echo "1970-01-01T00:00:00Z")

# Search for mentions
MENTIONS=$(gh api "repos/$REPO/issues/comments" -f since="$SINCE" -f per_page=50 \
  | jq -c '.[] | select(.body | test("@orchestrator|@codex review|@claude"))')

for mention in $MENTIONS; do
  BODY=$(jq -r '.body' <<< "$mention")
  ISSUE_URL=$(jq -r '.issue_url' <<< "$mention")
  ISSUE_NUM=$(basename "$ISSUE_URL")
  AUTHOR=$(jq -r '.user.login' <<< "$mention")

  case "$BODY" in
    *"@codex review"*|*"@orchestrator review"*)
      # Trigger code review on the linked PR
      trigger_review "$ISSUE_NUM" "$AUTHOR"
      ;;
    *"@orchestrator merge"*|*"merge"*|*"lgtm"*|*"ship it"*)
      # Auto-merge the PR
      trigger_merge "$ISSUE_NUM" "$AUTHOR"
      ;;
    *"@orchestrator"*)
      # General mention — create a task or respond
      create_mention_task "$ISSUE_NUM" "$BODY" "$AUTHOR"
      ;;
  esac
done

date -u +"%Y-%m-%dT%H:%M:%SZ" > "$STATE_DIR/mentions_since"
```

### 4.7 Review Agent Enforcement

The review agent MUST be different from the task agent:

```bash
# In review_prs.sh (or new review logic)
TASK_AGENT=$(bd_show "$TASK_ID" | jq -r '.fields.agent // ""')
REVIEW_AGENT=$(config_get '.review_agent // "codex"')

# Enforce different agent for review
if [ "$REVIEW_AGENT" = "$TASK_AGENT" ]; then
  # Pick the other agent
  case "$TASK_AGENT" in
    claude) REVIEW_AGENT="codex" ;;
    codex)  REVIEW_AGENT="claude" ;;
    *)      REVIEW_AGENT="claude" ;;
  esac
fi
```

### 4.8 Auto-Merge

After review passes and CI is green:

```bash
# Check if PR is approved and CI passes
PR_STATUS=$(gh pr view "$PR_NUM" --repo "$REPO" --json reviewDecision,statusCheckRollup \
  -q '{decision: .reviewDecision, checks: [.statusCheckRollup[].conclusion]}')

DECISION=$(jq -r '.decision' <<< "$PR_STATUS")
ALL_PASS=$(jq -r '.checks | all(. == "SUCCESS")' <<< "$PR_STATUS")

if [ "$DECISION" = "APPROVED" ] && [ "$ALL_PASS" = "true" ]; then
  STRATEGY=$(config_get '.merge_strategy // "squash"')
  gh pr merge "$PR_NUM" --repo "$REPO" "--$STRATEGY" --auto
  bd_update "$TASK_ID" --status done --note "PR #$PR_NUM merged ($STRATEGY)"
fi
```

### 4.9 tmux Agent Sessions

Every agent run wrapped in a tmux session:

```bash
# In run_task.sh — agent invocation
SESSION="orch-${TASK_ID}"
RESPONSE_FILE="${STATE_DIR}/${FILE_PREFIX}-response.json"
DONE_MARKER="${STATE_DIR}/${FILE_PREFIX}-done"

# Run agent inside tmux session (autonomous mode with observability)
tmux new-session -d -s "$SESSION" -x 200 -y 50 -- bash -c "
  cd '$WORKTREE_DIR'
  export GIT_AUTHOR_NAME='${TASK_AGENT}[bot]'
  claude -p --model '$MODEL' \
    --output-format json \
    --append-system-prompt '$SYSTEM_PROMPT' \
    '$MESSAGE' \
    > '$RESPONSE_FILE' 2>'$STDERR_FILE'
  echo \"EXIT_CODE=\$?\" > '$DONE_MARKER'
"

# Wait for completion (check both session existence and done marker)
while [ ! -f "$DONE_MARKER" ]; do
  if ! tmux has-session -t "$SESSION" 2>/dev/null; then
    break
  fi
  sleep 5
done

# Read response as before
RESPONSE=$(cat "$RESPONSE_FILE")
```

CLI commands:
```bash
orch task attach 42     # tmux attach -t orch-42
orch task stream 42     # tmux capture-pane / pipe-pane (read-only)
orch task live          # list active tmux sessions
orch task kill 42       # tmux kill-session + mark needs_review
orch task resume 42     # claude --resume (continue failed conversation)
```

### 4.10 Poll Cycle (Revised)

```bash
# serve.sh — iterate managed projects
PROJECTS=$(yq -r '.projects[].path' ~/.orchestrator/projects.yml)

for PROJECT_DIR in $PROJECTS; do
  [ -d "$PROJECT_DIR" ] || continue
  export PROJECT_DIR

  # Check beads for ready tasks
  READY=$(bd_ready)
  [ "$READY" = "[]" ] && continue

  # Check for existing tmux sessions (tasks already running)
  echo "$READY" | jq -r '.[].id' | while read -r TASK_ID; do
    if tmux has-session -t "orch-${TASK_ID}" 2>/dev/null; then
      continue  # Already running
    fi
    run_task.sh "$PROJECT_DIR" "$TASK_ID" &
  done
done

# Jobs tick (global, not per-project)
jobs_tick.sh

# GitHub import + publish (per project, on longer interval)
for PROJECT_DIR in $PROJECTS; do
  gh_import.sh "$PROJECT_DIR"
  gh_publish.sh "$PROJECT_DIR"
  gh_mentions.sh "$PROJECT_DIR"
  review_prs.sh "$PROJECT_DIR"
done
```

### 4.11 Project-Local Worktrees

```bash
# In run_task.sh
WORKTREE_BASE="${PROJECT_DIR}/.orchestrator/worktrees"
mkdir -p "$WORKTREE_BASE"

BRANCH="gh-task-${GH_ISSUE:-$TASK_ID}-$(slugify "$TITLE")"
WORKTREE_DIR="${WORKTREE_BASE}/${BRANCH}"

# For bare repos, detect default branch
if is_bare_repo "$PROJECT_DIR"; then
  DEFAULT_BRANCH=$(git -C "$PROJECT_DIR" symbolic-ref --short HEAD 2>/dev/null || echo main)
  git -C "$PROJECT_DIR" fetch origin
else
  DEFAULT_BRANCH=$(git -C "$PROJECT_DIR" symbolic-ref --short HEAD 2>/dev/null || echo main)
fi

git -C "$PROJECT_DIR" worktree add "$WORKTREE_DIR" -b "$BRANCH" "$DEFAULT_BRANCH"
```

---

## 5. File-by-File Analysis

### Files to DELETE (~2,100 lines)

| File | Lines | Why |
|------|-------|-----|
| `scripts/db.sh` | 948 | Replaced by Beads. All `db_*` functions eliminated. |
| `scripts/gh_push.sh` | 643 | Replaced by `gh_publish.sh` (~200 lines). |
| `scripts/gh_pull.sh` | 203 | Replaced by `gh_import.sh` (~100 lines). |
| `scripts/gh_sync.sh` | 27 | Replaced by separate import + publish calls. |
| `scripts/schema.sql` | 133 | SQLite schema no longer needed. |
| `scripts/migrate_to_sqlite.sh` | 258 | Migration tool no longer needed. |
| **Total** | **~2,212** | |

### Files to CREATE (~600 lines)

| File | Est. Lines | Purpose |
|------|-----------|---------|
| `scripts/beads.sh` | ~150 | Beads wrapper functions (`bd_*`) |
| `scripts/gh_publish.sh` | ~200 | One-way Beads → GitHub publish |
| `scripts/gh_import.sh` | ~100 | One-way GitHub → Beads import |
| `scripts/gh_mentions.sh` | ~80 | GitHub mentions listener |
| `scripts/task_attach.sh` | ~20 | Attach to tmux session |
| `scripts/task_live.sh` | ~30 | List active tmux sessions |
| `scripts/task_kill.sh` | ~20 | Kill tmux session + mark needs_review |
| **Total** | **~600** | |

### Files to MODIFY

| File | Lines | Changes |
|------|-------|---------|
| `scripts/lib.sh` | 938 | Remove: `source db.sh`, all db helper refs. Add: `source beads.sh`, project registry helpers, tmux helpers. Remove `ORCH_WORKTREES`. (~300 lines changed) |
| `scripts/run_task.sh` | 931 | Replace all `db_*` calls with `bd_*`. Wrap agent in tmux. Use project-local worktrees. (~400 lines changed) |
| `scripts/poll.sh` | 117 | Replace `db_task_ids_by_status` with `bd_ready`. Replace worktree cleanup queries. tmux session detection. (~80 lines changed) |
| `scripts/serve.sh` | 253 | Iterate projects from `projects.yml`. Replace `db_task_projects`. Separate import/publish calls. (~60 lines changed) |
| `scripts/route_task.sh` | 256 | Replace `db_task_field`/`db_task_update` with `bd_show`/`bd_update`. (~40 lines changed) |
| `scripts/review_prs.sh` | 331 | Enforce different review agent. Add auto-merge. Replace db lookups with beads. (~80 lines changed) |
| `scripts/jobs_tick.sh` | 130 | Replace `db_load_job`/`db_job_set` with YAML reads/writes via `yq`. (~60 lines changed) |
| `scripts/add_task.sh` | 78 | Replace `db_create_task` with `bd_create`. Add `--project` flag using `projects.yml`. (~30 lines changed) |
| `scripts/list_tasks.sh` | 17 | Replace `db_task_display_tsv` with `bd_list`. (~15 lines changed) |
| `scripts/status.sh` | 68 | Replace `db_scalar`/`db_status_json` with `bd_list --format json` aggregation. (~40 lines changed) |
| `scripts/tree.sh` | 62 | Replace `db_task_roots`/`db_task_children` with `bd tree`. (~50 lines changed, mostly simpler) |
| `scripts/dashboard.sh` | 94 | Replace db queries with beads queries. (~50 lines changed) |
| `scripts/progress_report.sh` | 135 | Replace db queries with beads list + aggregation. (~60 lines changed) |
| `scripts/init.sh` | 322 | Add `bd init` step. Write `.orchestrator/config.yml` instead of `.orchestrator.yml`. Register in `projects.yml`. (~40 lines changed) |
| `scripts/project_add.sh` | 76 | Add `bd init` for bare repos. Register in `projects.yml`. (~20 lines changed) |
| `scripts/output.sh` | 60 | Unchanged (pure formatting). |
| `scripts/chat.sh` | 262 | Replace db context with `bd_context`. (~20 lines changed) |
| `scripts/plan_chat.sh` | 181 | Replace db context with `bd_context`. (~20 lines changed) |
| `scripts/retry_task.sh` | ~30 | Replace `db_task_update` with `bd_update`. (~10 lines changed) |
| `scripts/unblock_all.sh` | ~30 | Replace db queries with beads dependency resolution. (~15 lines changed) |
| `scripts/set_agent.sh` | ~20 | Replace `db_task_update` with `bd_update`. (~10 lines changed) |
| `scripts/stop.sh` | ~30 | Add tmux session cleanup. (~10 lines changed) |
| `scripts/stream_task.sh` | ~30 | Use tmux pipe-pane instead of file tailing. (~20 lines changed) |
| `scripts/unlock.sh` | ~30 | Unchanged (lock mechanism stays mkdir-based). |
| `scripts/service_install.sh` | 71 | Unchanged. |
| `scripts/setup.sh` | 112 | Add beads dependency check. (~5 lines changed) |
| `scripts/agents.sh` | ~20 | Unchanged. |
| `scripts/normalize_json.py` | 213 | Unchanged (agent response parsing). |
| `scripts/cron_match.py` | 129 | Unchanged (cron scheduling). |
| `justfile` | 448 | Update recipes for new scripts. Add `task attach`, `task live`, `task kill`, `task resume`. Remove `gh sync` (replace with `gh import`/`gh publish`). (~40 lines changed) |
| `tests/orchestrator.bats` | 4,875 | Major rewrite: replace all `tdb`/`tdb_field` SQLite helpers with beads CLI assertions. (~2,000 lines changed) |

### Files UNCHANGED

| File | Lines | Why |
|------|-------|-----|
| `scripts/normalize_json.py` | 213 | Pure JSON parsing, no DB dependency |
| `scripts/cron_match.py` | 129 | Pure cron matching, no DB dependency |
| `scripts/output.sh` | 60 | Pure formatting |
| `scripts/agents.sh` | ~20 | Lists available CLIs |
| `scripts/service_install.sh` | 71 | Launchd plist management |
| `prompts/*.md` | ~8 files | Template content unchanged |
| `bin/security-audit` | ~80 | Security scanning |
| `.github/workflows/release.yml` | 267 | CI/CD (add `bd` install step) |

### Net Impact

- **Deleted**: ~2,200 lines
- **Created**: ~600 lines
- **Modified**: ~1,200 lines across 25 files
- **Net reduction**: ~1,600 lines of code

---

## 6. Implementation Checklist

### Phase 0: Foundation (no behavior change)

- [ ] Install beads as dependency (`brew install beads` or `bun add -g @anthropic/beads`)
- [ ] Add `bd` to CI install steps in `release.yml`
- [ ] Create `scripts/beads.sh` with wrapper functions
- [ ] Create `~/.orchestrator/projects.yml` with current project list
- [ ] Add `.orchestrator/worktrees/` to `.gitignore` of each project
- [ ] Run `bd init` in orchestrator and oblivion repos
- [ ] Import existing active tasks into beads (script: iterate SQLite, call `bd create`)
- [ ] Write tests for beads wrapper functions

### Phase 1: Project-local worktrees (independent of Beads)

- [ ] Change `WORKTREE_BASE` in `run_task.sh` to `$PROJECT_DIR/.orchestrator/worktrees/`
- [ ] Migrate existing worktrees with `git worktree move`
- [ ] Remove `ORCH_WORKTREES` env var from `lib.sh`
- [ ] Update `poll.sh` worktree cleanup to use project-local paths
- [ ] Update worktree detection in `dashboard.sh`
- [ ] Test: new task creates worktree in project-local directory
- [ ] Test: cleanup removes project-local worktree
- [ ] Clean up old `~/.orchestrator/worktrees/` directory

### Phase 2: tmux agent sessions

- [ ] Wrap agent invocation in `run_task.sh` with `tmux new-session`
- [ ] Add done marker / response file pattern
- [ ] Add tmux session check to `poll.sh` (skip tasks with active sessions)
- [ ] Create `scripts/task_attach.sh`
- [ ] Create `scripts/task_live.sh`
- [ ] Create `scripts/task_kill.sh`
- [ ] Add justfile recipes: `task attach`, `task live`, `task kill`
- [ ] Add tmux session cleanup to `stop.sh` and serve.sh EXIT trap
- [ ] Test: agent runs in tmux, output captured correctly
- [ ] Test: `orch task attach` connects to running session
- [ ] Test: `orch task kill` terminates session and marks needs_review
- [ ] Handle codex (no interactive mode — tmux for watch-only)

### Phase 3: Beads replaces SQLite for task CRUD

- [ ] `poll.sh`: replace `db_task_ids_by_status` with `bd_ready`
- [ ] `run_task.sh`: replace `db_load_task` / `db_task_field` with `bd_show`
- [ ] `run_task.sh`: replace `db_task_update` / `db_store_agent_response` with `bd_update`
- [ ] `run_task.sh`: replace `db_append_history` with `bd_log`
- [ ] `route_task.sh`: replace db reads/writes with beads
- [ ] `add_task.sh`: replace `db_create_task` with `bd_create`
- [ ] `list_tasks.sh`: replace `db_task_display_tsv` with `bd_list`
- [ ] `status.sh`: replace db queries with beads aggregation
- [ ] `tree.sh`: replace `db_task_roots`/`db_task_children` with `bd tree`
- [ ] `dashboard.sh`: replace db queries with beads
- [ ] `progress_report.sh`: replace db queries with beads
- [ ] `chat.sh` / `plan_chat.sh`: replace db context with `bd_context`
- [ ] `retry_task.sh`: replace `db_task_update` with `bd_update`
- [ ] `set_agent.sh`: replace `db_task_update` with `bd_update`
- [ ] `unblock_all.sh`: replace db queries with beads dependency resolution
- [ ] `lib.sh`: remove `source db.sh`, add `source beads.sh`
- [ ] Run both systems in parallel for 1 week — verify beads state matches SQLite
- [ ] Test: all 217 bats tests pass with beads backend

### Phase 4: Replace sync with publish + import

- [ ] Create `scripts/gh_publish.sh` (one-way Beads → GitHub)
- [ ] Create `scripts/gh_import.sh` (one-way GitHub → Beads)
- [ ] Create `scripts/gh_mentions.sh` (mentions listener)
- [ ] Update `serve.sh` to call import/publish/mentions instead of `gh_sync.sh`
- [ ] Remove `gh_push.sh`, `gh_pull.sh`, `gh_sync.sh`
- [ ] Test: new beads task creates GitHub issue
- [ ] Test: human-created GitHub issue imports into beads
- [ ] Test: status changes publish to GitHub labels
- [ ] Test: @mention triggers appropriate action

### Phase 5: Review agent + auto-merge

- [ ] Enforce different review agent in `review_prs.sh`
- [ ] Add auto-merge logic (review approved + CI green → merge)
- [ ] Add merge strategy config per project
- [ ] Test: task done by claude gets reviewed by codex
- [ ] Test: approved PR with passing CI auto-merges

### Phase 6: Jobs move to YAML

- [ ] Move jobs from SQLite `jobs` table to `~/.orchestrator/jobs.yml`
- [ ] Rewrite `jobs_tick.sh` to use `yq` for job reads/writes
- [ ] Rewrite `jobs_add.sh`, `jobs_list.sh`, `jobs_remove.sh`, `jobs_enable.sh`, `jobs_disable.sh`, `jobs_info.sh`
- [ ] Test: job add/remove/enable/disable works with YAML
- [ ] Test: cron tick fires jobs correctly

### Phase 7: Delete SQLite layer

- [ ] Delete `scripts/db.sh` (948 lines)
- [ ] Delete `scripts/schema.sql` (133 lines)
- [ ] Delete `scripts/migrate_to_sqlite.sh` (258 lines)
- [ ] Remove SQLite auto-migration from `serve.sh`
- [ ] Remove `DB_PATH` env var
- [ ] Remove `sqlite3` from CI dependencies
- [ ] Update all references in `lib.sh`
- [ ] Final test run: all tests pass without SQLite

### Phase 8: Test rewrite

- [ ] Replace `tdb`/`tdb_field`/`tdb_set`/`tdb_count` helpers with beads assertions
- [ ] Replace `tdb_job_field` with YAML assertions
- [ ] Mock `bd` command in tests (like we mock `gh`)
- [ ] Add tests for: beads wrapper functions, project registry, tmux sessions, gh_publish, gh_import, gh_mentions
- [ ] Target: 200+ tests covering all new code paths

### Phase 9: Cleanup & docs

- [ ] Update `AGENTS.md` with new architecture
- [ ] Update `specs.md`
- [ ] Remove stale plan documents (`plan-github-native.md`, `plan-tmux-sessions.md`)
- [ ] Update README with new setup instructions
- [ ] Remove old worktree directories (`~/.orchestrator/worktrees/`)
- [ ] Remove orphaned SQLite databases

---

## 7. Migration Path

### Strategy: Incremental, parallel-safe

The migration MUST be incremental. At no point does the system stop working. Each phase is independently deployable and testable.

```
Current (v0.39.x)  →  Phase 1-2 (v0.40.x)  →  Phase 3-4 (v0.41.x)  →  Phase 5-7 (v1.0.0)
SQLite + sync          + local worktrees        beads primary             SQLite deleted
                       + tmux sessions          + one-way publish         clean architecture
```

### Task Import Script

One-time migration of active tasks from SQLite to Beads:

```bash
#!/usr/bin/env bash
# scripts/import_sqlite_to_beads.sh
# Run once per project to seed .beads/ from orchestrator.db

PROJECT_DIR="${1:-.}"
DB_PATH="${ORCH_HOME:-$HOME/.orchestrator}/orchestrator.db"

# Get active tasks for this project
sqlite3 "$DB_PATH" "SELECT id, title, body, status, agent FROM tasks
  WHERE status NOT IN ('done') AND (dir = '$PROJECT_DIR' OR dir IS NULL)
  ORDER BY id;" | while IFS='|' read -r id title body status agent; do

  echo "Importing task $id: $title"
  TASK_ID=$(cd "$PROJECT_DIR" && bd create "$title" --body "$body" --format json | jq -r '.id')

  # Set status and agent
  (cd "$PROJECT_DIR" && bd update "$TASK_ID" --status "$status")
  [ -n "$agent" ] && (cd "$PROJECT_DIR" && bd update "$TASK_ID" --field agent="$agent")

  # Link to GitHub issue if exists
  GH_ISSUE=$(sqlite3 "$DB_PATH" "SELECT gh_issue FROM tasks WHERE id = $id;")
  [ -n "$GH_ISSUE" ] && (cd "$PROJECT_DIR" && bd update "$TASK_ID" --field gh_issue="$GH_ISSUE")
done
```

### Rollback Plan

Each phase can be rolled back independently:
- **Phase 1** (worktrees): revert `WORKTREE_BASE`, `git worktree move` back
- **Phase 2** (tmux): remove tmux wrapper, revert to direct subprocess
- **Phase 3** (beads): re-enable `db.sh`, SQLite data is still there
- **Phase 4** (publish): re-enable `gh_push.sh`/`gh_pull.sh`

### 4.12 Agent Profiles (Multi-Agent via Claude Code)

Some agents (Kimi, Minimax) don't have their own CLI — they use Claude Code as the runtime with a different model provider. The agent system needs to support "profiles": same CLI, different configuration.

```yaml
# ~/.orchestrator/config.yml — agent definitions
agents:
  claude:
    cli: claude
    model_map:
      simple: haiku
      medium: sonnet
      complex: opus
      review: sonnet

  codex:
    cli: codex
    model_map:
      simple: gpt-5.1-codex-mini
      medium: gpt-5.2
      complex: gpt-5.3-codex
      review: gpt-5.2

  opencode:
    cli: opencode
    model_map:
      simple: gemini-2.5-flash
      medium: gemini-2.5-pro
      complex: gemini-2.5-pro

  kimi:
    cli: claude                    # Uses claude-code as the runtime
    model_map:
      simple: kimi-k2
      medium: kimi-k2
      complex: kimi-k2
    env:                           # Extra env vars for the agent process
      ANTHROPIC_MODEL: kimi-k2
      # Or: CLAUDE_MODEL_OVERRIDE, depending on how claude-code routes

  minimax:
    cli: claude
    model_map:
      simple: minimax-m1
      medium: minimax-m1
      complex: minimax-m1
    env:
      ANTHROPIC_MODEL: minimax-m1
```

In `run_task.sh`, agent invocation becomes:

```bash
AGENT_CONFIG=$(config_get ".agents.${TASK_AGENT}")
AGENT_CLI=$(echo "$AGENT_CONFIG" | yq -r '.cli // ""')
[ -z "$AGENT_CLI" ] && AGENT_CLI="$TASK_AGENT"  # Fallback: agent name = CLI name

# Apply agent-specific env vars
AGENT_ENV=$(echo "$AGENT_CONFIG" | yq -r '.env // {} | to_entries[] | .key + "=" + .value' 2>/dev/null)
while IFS= read -r env_line; do
  [ -n "$env_line" ] && export "$env_line"
done <<< "$AGENT_ENV"

# Invoke using the resolved CLI
case "$AGENT_CLI" in
  claude) RESPONSE=$(claude -p --model "$MODEL" ...) ;;
  codex)  RESPONSE=$(codex exec --model "$MODEL" ...) ;;
  opencode) RESPONSE=$(opencode run --model "$MODEL" ...) ;;
esac
```

This means:
- `model_for_complexity()` looks up `agents.$AGENT.model_map.$COMPLEXITY`
- The router can route to `kimi` or `minimax` just like `claude` or `codex`
- The agent badge in `gh_publish.sh` maps each agent to its emoji
- `available_agents()` checks if the CLI for each agent is installed
- Per-project config can override: `agents.kimi.allowed_tools: [...]`

### 4.13 Lifecycle Hooks (inspired by dmux)

The orchestrator should expose lifecycle hooks at key points in the task/agent lifecycle. Hooks are shell commands configured per-project in `.orchestrator.yml` or globally in `config.yml`.

```yaml
# .orchestrator.yml or config.yml
hooks:
  # Task lifecycle
  on_task_created: "scripts/notify.sh created"
  on_task_routed: ""
  on_task_started: ""           # Agent session about to start
  on_task_completed: ""         # Agent finished successfully
  on_task_failed: ""            # Agent failed / needs_review
  on_task_blocked: ""           # Task blocked (deps or max_attempts)

  # Agent session lifecycle
  on_agent_session_start: ""    # tmux session created, agent about to run
  on_agent_session_end: ""      # tmux session ended (success or failure)
  on_agent_idle: ""             # Agent appears idle (future: status detection)

  # Git/PR lifecycle
  on_worktree_created: ""       # Worktree just created for a task
  on_branch_pushed: ""          # Agent's branch pushed to remote
  on_pr_created: ""             # PR created for a task
  on_pr_merged: ""              # PR merged
  on_pr_review_requested: ""    # Review agent assigned

  # Service lifecycle
  on_service_start: ""          # Orchestrator daemon started
  on_service_stop: ""           # Orchestrator daemon stopping
  on_job_fired: ""              # Cron job triggered
```

Hook execution:

```bash
# scripts/hooks.sh
run_hook() {
  local hook_name="$1"; shift
  local cmd
  cmd=$(config_get ".hooks.${hook_name} // \"\"")
  [ -z "$cmd" ] || [ "$cmd" = "null" ] && return 0

  # Export context as env vars
  export ORCH_HOOK="$hook_name"
  export ORCH_TASK_ID="${TASK_ID:-}"
  export ORCH_TASK_TITLE="${TASK_TITLE:-}"
  export ORCH_TASK_AGENT="${TASK_AGENT:-}"
  export ORCH_TASK_STATUS="${AGENT_STATUS:-}"
  export ORCH_PROJECT_DIR="${PROJECT_DIR:-}"
  export ORCH_WORKTREE_DIR="${WORKTREE_DIR:-}"
  export ORCH_BRANCH="${BRANCH_NAME:-}"
  export ORCH_PR_URL="${PR_URL:-}"
  export ORCH_TMUX_SESSION="${TMUX_SESSION:-}"

  # Run async (don't block the main flow)
  (eval "$cmd" "$@" &>/dev/null &) || true
}
```

Call sites in existing scripts:

| Script | Hook |
|--------|------|
| `add_task.sh` | `run_hook on_task_created` |
| `route_task.sh` | `run_hook on_task_routed` |
| `run_task.sh` (pre-agent) | `run_hook on_task_started`, `run_hook on_agent_session_start` |
| `run_task.sh` (post-agent) | `run_hook on_agent_session_end`, `run_hook on_task_completed` or `on_task_failed` |
| `run_task.sh` (worktree) | `run_hook on_worktree_created` |
| `run_task.sh` (push) | `run_hook on_branch_pushed` |
| `run_task.sh` (PR) | `run_hook on_pr_created` |
| `review_prs.sh` | `run_hook on_pr_review_requested` |
| `serve.sh` | `run_hook on_service_start`, `on_service_stop` |
| `jobs_tick.sh` | `run_hook on_job_fired` |

### 4.14 Agent Observability (Web Dashboard — future)

Phase 1 (current): tmux sessions for live terminal attachment (`orch task attach <id>`, `orch task live`).

Phase 2 (future, inspired by dmux's Vue 3 dashboard):

- **SSE endpoint**: `serve.sh` exposes an HTTP endpoint (via `socat` or `ncat`) that streams task events as Server-Sent Events
- **Terminal capture**: Periodically dump tmux pane content via `tmux capture-pane -p -t orch-$ID` for web display
- **Dashboard**: Simple HTML page (no build step) that connects to SSE and shows:
  - Active agent sessions with live terminal output
  - Task queue (pending, in_progress, blocked)
  - Recent history (done, needs_review)
  - Service health (uptime, last poll, last sync)

This is a separate feature and should NOT block the beads migration. Track as a future issue.

### 4.15 A/B Agent Comparison (future)

Run two agents on the same task in parallel, compare results:

```bash
# orch task ab <id> claude codex
# Creates two worktrees, two tmux sessions, compares outputs
```

Useful for evaluating agent quality on real tasks. Track as a future issue.

---

## 8. Open Questions

1. **Beads in bare repos**: Beads needs a working tree for `.beads/`. For bare clones (`orch project add`), do we create a permanent worktree just for beads state? Or store `.beads/` outside the git repo?

2. **Beads performance**: Is `bd ready` fast enough for a 10s poll cycle across 5+ projects? Need to benchmark.

3. **Beads custom fields**: Can Beads store arbitrary fields (agent_model, prompt_hash, tokens, duration, gh_issue)? The `--field` flag suggests yes, but need to verify persistence.

4. **Beads git tracking**: Should `.beads/` be committed? For personal projects yes (state travels with branch). For open source, maybe `.gitignore` it.

5. **tmux dependency**: Is tmux always available? macOS yes (brew). Linux servers maybe not. Fallback to direct subprocess (current behavior)?

6. **Job catch-up**: The plan at `~/.claude/plans/rosy-sprouting-kahn.md` describes catch-up logic for missed cron jobs. Should this be implemented before or during Phase 6?

7. **Beads + GitHub Projects V2**: Any synergy? Could Beads custom fields map to Project fields for board views?

8. **Multiple orchestrator instances**: If two machines run orchestrator for the same project, Beads JSONL could conflict. Is this a real concern? (Probably not — single-machine use case.)

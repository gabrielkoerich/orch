# Plan: GitHub-Native Architecture (v2)

> **Status**: Planning — gathering data from current system before implementation.
> **Goal**: Replace SQLite + bidirectional sync with GitHub Issues as the single source of truth.

## Problem

The current architecture maintains two data stores (local SQLite + GitHub Issues) with a bidirectional sync layer (`gh_push.sh`, `gh_pull.sh`, `gh_sync.sh`). This sync is the source of most bugs:

- **Comment spam**: 70+ duplicate comments on issues when dedup hash fails under concurrency
- **Cross-project issue creation**: `gh_push` created issues in wrong repos
- **Timing bugs**: `updated_at` vs `gh_synced_at` race conditions
- **Status divergence**: local says `done`, GitHub says `open`
- **Infinite reroute loops**: agent model mismatch persisted in DB across reroutes
- **Dirty tracking complexity**: `gh_synced_status`, `gh_state`, `last_comment_hash`, `gh_updated_at`

Every bug fix adds more guards and edge cases. The sync layer is ~600 lines of bash and the root cause of most operational issues.

## Proposed Architecture

```
GitHub Issues (source of truth)
  ├── Title, body → task definition
  ├── Labels → status, agent, complexity, project
  ├── Comments → agent output, progress, errors
  └── Projects V2 → board views, custom fields (optional)

Local filesystem (ephemeral runtime)
  ├── Locks → flock/mkdir (prevent concurrent execution)
  ├── Prompt files → rendered prompts + hash (debug artifacts)
  ├── Agent output → raw JSON response, stderr (temp files)
  └── jobs.yml → cron schedules + last_run (only persistent local state)
```

### What changes

| Component | Current | Proposed |
|-----------|---------|----------|
| Task store | SQLite `tasks` table | GitHub Issues |
| Status | `tasks.status` column | Labels: `status:new`, `status:in_progress`, `status:done` |
| Agent assignment | `tasks.agent` column | Labels: `agent:claude`, `agent:codex`, `agent:opencode` |
| Complexity | `tasks.complexity` column | Labels: `complexity:simple`, `complexity:medium`, `complexity:complex` |
| Task output | SQLite fields (summary, accomplished, remaining, etc.) | Issue comment (structured markdown) |
| Token tracking | SQLite fields (input_tokens, output_tokens, duration) | Comment metadata table or local log |
| Parent/child | `task_children` table + `parent_id` | Task lists in issue body or `parent:#N` labels |
| Jobs | SQLite `jobs` table | `jobs.yml` file (id, schedule, last_run, type, command) |
| Sync | `gh_push.sh` + `gh_pull.sh` + `gh_sync.sh` | **Deleted** |
| Dirty tracking | `gh_synced_at`, `gh_synced_status`, `last_comment_hash` | **Deleted** |
| Migration script | `migrate_to_sqlite.sh` | **Deleted** |

### What stays the same

- `run_task.sh` — agent invocation (claude/codex/opencode), prompt building, response parsing
- `poll.sh` — picks tasks, but reads from GitHub instead of SQLite
- `serve.sh` — main loop (poll, jobs_tick)
- `route_task.sh` — LLM router (reads issue body instead of DB field)
- `review_prs.sh` — PR review (already GitHub-native)
- `jobs_tick.sh` — cron scheduler (reads `jobs.yml` instead of SQLite)
- Prompt templates, agent profiles, config — unchanged

## Detailed Design

### 1. Label Convention

```
status:new              — ready for agent pickup
status:routed           — agent assigned, not yet started
status:in_progress      — agent working
status:blocked          — waiting on dependency or human
status:needs_review     — agent failed, needs human attention
status:done             — completed
status:in_review        — PR created, awaiting review

agent:claude
agent:codex
agent:opencode

complexity:simple
complexity:medium
complexity:complex

project:<name>          — for multi-project setups (optional)
scheduled               — created by a cron job
no-agent                — skip during polling
```

### 2. Poll Cycle (`poll.sh`)

```bash
# Fetch issues labeled status:new (the task queue)
ISSUES=$(gh api "repos/$REPO/issues" \
  -f labels="status:new" \
  -f state=open \
  -f per_page=10 \
  -f sort=created \
  -f direction=asc)

# For each issue, check lock, claim it, run agent
for issue in $ISSUES; do
  ISSUE_NUM=$(jq '.number' <<< "$issue")

  # Atomic claim: add status:in_progress, remove status:new
  # If another process already claimed it, the label won't be status:new
  # (GitHub label operations are not atomic, so use flock locally)
  acquire_lock "$ISSUE_NUM" || continue

  # Swap labels
  gh issue edit "$ISSUE_NUM" --repo "$REPO" \
    --remove-label "status:new" --add-label "status:in_progress"

  run_task "$ISSUE_NUM" &
done
```

### 3. Task Execution (`run_task.sh`)

```bash
# Read task from GitHub issue
ISSUE_JSON=$(gh api "repos/$REPO/issues/$ISSUE_NUM")
TITLE=$(jq -r '.title' <<< "$ISSUE_JSON")
BODY=$(jq -r '.body' <<< "$ISSUE_JSON")
LABELS=$(jq -r '[.labels[].name] | join(",")' <<< "$ISSUE_JSON")

# Extract agent from labels
AGENT=$(echo "$LABELS" | tr ',' '\n' | grep '^agent:' | sed 's/^agent://' | head -1)
COMPLEXITY=$(echo "$LABELS" | tr ',' '\n' | grep '^complexity:' | sed 's/^complexity://' | head -1)

# If no agent, route it
if [ -z "$AGENT" ]; then
  AGENT=$(route_task "$ISSUE_NUM" "$TITLE" "$BODY")
  gh issue edit "$ISSUE_NUM" --repo "$REPO" --add-label "agent:$AGENT"
fi

# Read previous comments for context (retry awareness)
COMMENTS=$(gh api "repos/$REPO/issues/$ISSUE_NUM/comments" -f per_page=10)

# Build prompt, run agent (same as current)
# ...

# Post results as comment
gh issue comment "$ISSUE_NUM" --repo "$REPO" --body "$RESULT_COMMENT"

# Update status label
gh issue edit "$ISSUE_NUM" --repo "$REPO" \
  --remove-label "status:in_progress" \
  --add-label "status:$FINAL_STATUS"
```

### 4. Adding Tasks

```bash
# orch task add "title" → gh issue create
gh issue create --repo "$REPO" \
  --title "$TITLE" \
  --body "$BODY" \
  --label "status:new"

# With agent pre-selected
gh issue create --repo "$REPO" \
  --title "$TITLE" \
  --body "$BODY" \
  --label "status:new,agent:claude,complexity:medium"
```

### 5. Job Scheduler

Replace SQLite `jobs` table with a simple YAML file:

```yaml
# ~/.orchestrator/jobs.yml
morning-review:
  title: "Daily morning review"
  schedule: "0 8 * * *"
  type: task          # creates a GitHub issue
  labels: "scheduled,agent:claude"
  body: "Review stuck tasks, test gaps, and errors."
  last_run: "2026-02-20T08:00:00Z"

progress-report:
  schedule: "*/30 * * * *"
  type: bash          # runs a local command
  command: "scripts/progress_report.sh"
  last_run: "2026-02-20T14:30:00Z"

evening-retrospective:
  title: "Daily evening retrospective"
  schedule: "0 18 * * *"
  type: task
  labels: "scheduled,agent:claude"
  body: |
    Review completed and failed tasks, and suggest improvements.

    Before creating improvement tasks, search existing open issues for similar titles.
    Use: gh issue list --repo owner/repo --state open --search "<topic>"
    Only create a new issue if no similar open issue exists.
  last_run: "2026-02-20T18:00:00Z"
```

`jobs_tick.sh` reads YAML with `yq`, checks cron match, creates issues or runs commands.

### 6. Locking (Concurrency Control)

GitHub API label operations are not atomic. Two poll processes could both see `status:new` and try to claim the same issue. Solutions:

**Option A: Local flock (current approach, simplest)**
```bash
LOCK_DIR="/tmp/orchestrator-lock-$ISSUE_NUM"
mkdir "$LOCK_DIR" 2>/dev/null || return 1  # atomic on local filesystem
```
This works for single-machine orchestrator. Multiple machines would need Option B.

**Option B: GitHub-native locking via assignee**
```bash
# Claim by assigning bot user — GitHub prevents duplicate assigns
gh issue edit "$ISSUE_NUM" --repo "$REPO" --add-assignee "$BOT_USER"
# Check if we got the assignment
ASSIGNEE=$(gh api "repos/$REPO/issues/$ISSUE_NUM" -q '.assignees[0].login')
[ "$ASSIGNEE" = "$BOT_USER" ] || return 1  # someone else claimed it
```

**Option C: GitHub App installation (future)**
Use a GitHub App with issue event webhooks instead of polling. The App receives `issues.labeled` events and dispatches to agents. No polling, no locking race.

**Recommendation**: Start with Option A (local flock), move to Option C when the GitHub App (task #47) is implemented.

### 7. Parent/Child Relationships

GitHub doesn't have native subtask support. Options:

**Option A: Task lists in issue body**
```markdown
## Subtasks
- [ ] #123 Add validation
- [ ] #124 Write tests
- [x] #125 Update docs
```
GitHub renders these as tracked tasks. The orchestrator can parse them.

**Option B: Labels**
```
parent:#42 on child issues
```
Simple, queryable via API: `gh api search/issues -f q="label:parent:#42 repo:$REPO"`

**Option C: GitHub Projects V2 custom fields**
Add a "Parent" field to the project. Queryable via GraphQL.

**Recommendation**: Option A (task lists) — GitHub already renders progress bars for these, and agents can update them by editing the issue body.

### 8. Attempt Tracking

Currently stored in `tasks.attempts` column. Options:

- **Count comments**: each agent run posts a comment, count = attempts
- **Label**: `attempt:3` (simple but noisy)
- **Comment metadata**: include `<!-- attempt:3 -->` HTML comment in the structured output

**Recommendation**: Count structured agent comments. Each agent run posts a comment with a known header pattern (e.g., `## 🤖 Agent Run`). Count those to get attempt number.

### 9. Multi-Project Support

Currently, tasks have a `dir` field pointing to the project directory. With GitHub-native:

- Each repo has its own issues — natural separation
- The orchestrator config lists managed repos:
  ```yaml
  projects:
    - repo: gabrielkoerich/orchestrator
    - repo: gabrielkoerich/oblivion
  ```
- `poll.sh` iterates each repo, fetches `status:new` issues, runs agents
- No more cross-project confusion — issues belong to their repo

### 10. Progress Report / Monitoring

`progress_report.sh` switches from SQLite queries to `gh api` calls:

```bash
# Count by status label
for status in new in_progress done needs_review blocked; do
  COUNT=$(gh api "repos/$REPO/issues" -f labels="status:$status" -f state=open -q 'length')
  echo "$status: $COUNT"
done

# Recent closures
gh api "repos/$REPO/issues" -f state=closed -f sort=updated -f per_page=5
```

## Migration Path

### Phase 1: Read from GitHub, write to both (parallel run)
- `poll.sh` reads from GitHub labels instead of SQLite
- `run_task.sh` writes results to both GitHub (comments/labels) and SQLite
- Verify: GitHub state matches SQLite state after each run
- Duration: 1-2 weeks of observation

### Phase 2: Write to GitHub only
- Remove SQLite writes from `run_task.sh`
- Remove `gh_push.sh`, `gh_pull.sh`, `gh_sync.sh`
- Keep SQLite as read-only cache for `status.sh` / `dashboard.sh` (optional)
- Duration: 1 week

### Phase 3: Remove SQLite
- Delete `db.sh`, `schema.sql`, `migrate_to_sqlite.sh`
- Rewrite `status.sh`, `dashboard.sh`, `list_tasks.sh` to use `gh api`
- Replace `jobs` table with `jobs.yml`
- Remove all `db_*` function calls
- Duration: 1-2 weeks

### Phase 4: GitHub App (optional, future)
- Replace polling with webhook-driven execution
- GitHub App receives events, dispatches to agents
- Eliminates poll latency and rate limit concerns

## Files to Delete

```
scripts/gh_push.sh          (~350 lines)
scripts/gh_pull.sh          (~200 lines)
scripts/gh_sync.sh          (~20 lines)
scripts/db.sh               (~950 lines)
scripts/schema.sql          (~120 lines)
scripts/migrate_to_sqlite.sh (~150 lines)
```

**~1,800 lines of code deleted.**

## Files to Modify

```
scripts/poll.sh             — read from gh api instead of SQLite
scripts/run_task.sh         — write to GitHub instead of SQLite
scripts/jobs_tick.sh        — read jobs.yml instead of SQLite
scripts/add_task.sh         — gh issue create instead of db_create_task
scripts/route_task.sh       — read issue from GitHub
scripts/status.sh           — gh api queries
scripts/dashboard.sh        — gh api queries
scripts/list_tasks.sh       — gh api queries
scripts/tree.sh             — task list parsing from issues
scripts/progress_report.sh  — gh api queries
scripts/lib.sh              — remove db.sh sourcing, simplify helpers
scripts/output.sh           — unchanged
```

## Open Questions

1. **Rate limits**: With 10s poll cycle across 2+ repos, are we safe at 5000 req/hr? Need to measure actual API calls per cycle.
2. **Webhook vs polling**: Should we wait for the GitHub App before migrating, or go polling-first?
3. **Token/cost data**: Store in comments (visible) or local log (private)? Comments are simpler but make issues noisy.
4. **Offline fallback**: Worth supporting local-only mode, or assume GitHub connectivity?
5. **Branch/worktree management**: Currently tied to SQLite fields. Move to reading from git directly + issue metadata?
6. **Performance**: `gh api` calls add latency. Cache with ETags? How much does it matter for a 10s poll cycle?

## Data to Gather

While the current system runs, collect:
- [ ] Average API calls per poll/sync cycle (to estimate rate limit usage)
- [ ] Failure modes that would be eliminated by this change
- [ ] Any features that genuinely need local-only state
- [ ] GitHub API latency for issue list/read/update operations
- [ ] Whether GitHub Projects V2 fields are needed or if labels suffice

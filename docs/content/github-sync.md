+++
title = "GitHub Backend"
description = "GitHub Issues as the native task backend"
weight = 8
+++

Tasks are stored in a unified SQLite database (`~/.orch/orch.db`) and synced to GitHub Issues (when `gh.enabled: true`). External tasks from GitHub are ingested into the store on each sync tick, so the store always has the latest data. Orch centralizes GitHub API calls via the server and uses a native HTTP client (`reqwest`) or the `gh` CLI internally.

## Setup

1. Install and authenticate (preferred methods shown):

```bash
# Recommended: set a Personal Access Token (non-interactive)
export GH_TOKEN="ghp_xxxxxxxxxxxxxxxxxxxx"

# Or: configure GitHub App credentials in ~/.orch/config.yml
```

Optional legacy interactive flow:

```bash
gh auth login
```

2. Configure in `config.yml`:
```yaml
gh:
  enabled: true
  repo: "owner/repo"
```

3. Optionally set up a GitHub Project v2:
```bash
orch project board list             # list accessible Project v2 boards
orch project board link PVT_xxx     # link this repo to a board
orch project board sync             # (re-)discover field/option IDs and persist to .orch.yml
orch project board info             # show current board config
```

## How It Works

**Task ID = Issue Number.** When you run `orch task add "Fix the login bug"`, it creates a GitHub issue and returns the issue number.

### Field → GitHub Mapping

| Task Field | GitHub Storage |
|------------|----------------|
| title | Issue title |
| body | Issue body |
| status | Label `status:new`, `status:routed`, etc. |
| agent | Label `agent:claude`, `agent:codex`, etc. |
| complexity | Label `complexity:simple/medium/complex` |
| labels | Issue labels (non-prefixed) |
| parent_id | Sub-issue relationship |
| summary, response | Structured comment with `<!-- orch:agent-response -->` marker |
| branch, worktree | SQLite store (`~/.orch/orch.db`) |

### Agent Response Comments

When an agent completes a task, orch posts a structured comment:

- **Agent badges**: 🟣 Claude, 🟢 Codex, 🔵 OpenCode
- **Metadata table**: status, agent, model, attempt, duration, tokens, prompt hash
- **Sections**: errors & blockers, accomplished, remaining, files changed
- **Collapsed sections**: stderr output and full prompt (with hash)
- **Content-hash dedup**: identical comments not re-posted

### Labels

- `status:<status>` — task status (`new`, `routed`, `in_progress`, `done`, `needs_review`, `blocked`)
- `agent:<agent>` — assigned agent
- `complexity:<level>` — routing complexity
- `blocked` (red) — applied when task is blocked, removed when unblocked
- `scheduled` and `job:{id}` — for job-created tasks
- When task is `done`, `auto_close` controls whether to close the issue

### Local Task State

Machine-specific fields (branch, worktree path, attempt count, agent, model, pr_number, memory) are stored in the unified SQLite database (`~/.orch/orch.db`). These fields are not synced to GitHub.

## Backoff

When GitHub rate limits or abuse detection triggers, orch sleeps and retries with exponential backoff:

```yaml
gh:
  backoff:
    base_seconds: 30     # initial backoff duration
    max_seconds: 900     # maximum backoff duration
```

## Projects (Optional)

Link tasks to a GitHub Project v2 board:

```yaml
gh:
  project_id: "PVT_..."
  project_status_field_id: "PVTSSF_..."
  project_status_map:
    backlog: "option-id-1"
    in_progress: "option-id-2"
    review: "option-id-3"
    done: "option-id-4"
```

Discover IDs automatically:
```bash
orch project board info          # show current project field/option IDs
orch project board sync          # (re-)discover and persist to .orch.yml
```

### Keep board Status aligned with status labels

The default integration above projects Orch's internal lifecycle into four
columns. For boards with their own workflow, an operator can instead enable
label-authoritative reconciliation. For example, `status:held` maps to **Held**,
and replacing it with `status:todo` moves the existing card to **Todo**.

```yaml
gh:
  project_label_sync:
    enabled: true
    max_updates_per_sync: 5
    projects:
      - repo: "owner/repo"
        project_id: "PVT_..."
        field_name: "Status"
        options:
          "status:backlog": "Backlog"
          "status:todo": "Todo"
          "status:in-progress": "In Progress"
          "status:paused": "Paused"
          "status:blocked": "Blocked"
          "status:review": "Review"
          "status:done": "Done"
          "status:archived": "Archived"
          "status:held": "Held"
```

Each entry explicitly scopes one repository and one Project V2. Repeat entries
for additional boards or repositories. All configured option names must already
exist, match exactly and be unique in that board's named single-select field.
Their IDs are discovered separately for each board; IDs are never copied between
projects. The token needs Projects write permission (`project` for a classic PAT).

The first sync repairs existing cards, including blank Status values. Subsequent
syncs read current labels and field values, so changes made outside Orch and
manual board drift are also repaired. Normal polling is sufficient (the engine's
`sync_interval`, normally 45 seconds); an already configured webhook can trigger
the same sync sooner. No public endpoint is needed. Item, field and label
connections are paginated, and matching values cause no mutation. The default
cap is five changed fields per repository per sync, shared across its configured
boards; larger backlogs converge over later syncs. This cap is not a separate API
quota: reads and writes use the existing GitHub client's backoff/circuit handling,
and other engine operations share the account's limits. Large portfolios may
need a longer engine sync interval or a lower cap.

Exactly one `status:*` label is authoritative. No status label leaves the card
unchanged; multiple status labels, an unmapped label, ambiguous/missing options,
incomplete pagination or partial GraphQL errors are reported rather than guessed.
A missing mutation acknowledgement stops the pass; field mutations make a single
HTTP attempt, so transport/5xx failures cannot resend a write before the next
sync reads the board again. Shared rate tracking, backoff and circuit checks
still apply. A preflight failure before sending does not consume the unknown
outcome limit. After three consecutive unknown outcomes for the same
item and option, that backend stops attempting the update until it observes a
matching value, the desired option changes, or the operator restarts the process.
Other items can still converge. This avoids unlimited writes after unknown results.

To run one bounded reconciliation pass without starting the engine or agents:

```bash
orch project board reconcile --repo owner/repo
```

The command reports the number of acknowledged field updates (zero can be a
no-op), warns about skipped items, and exits unsuccessfully on configuration or
API errors. It does not initialize task ingestion, routing, a TaskStore, or agent
processes. It uses the same configured write cap; repeat intentionally to finish
a larger backlog. Per-backend consecutive-error state lives in memory; this
one-shot command makes only one attempt per item in its single pass.

This reconciler only writes the configured field of existing, non-archived
Issue items in open Projects. It does not create issues, alter memberships,
unarchive items or projects, close/reopen issues, or edit labels. Existing legacy
ingestion can still add a task to its configured `gh.project_id`; when that board
is label-managed, its internal-status column writer defers to this reconciler.
To show a task in several boards, add it through GitHub's normal membership or
auto-add workflow first. A board view grouped by this field shows the selected
column; view filters and hidden columns remain user-controlled.

**Board state is not execution permission.** Labels such as `execution:held` and
historical `source-status:*` are unrelated and untouched. Custom `status:held`
does not add a new Orch execution state or pause an agent: the standard engine
can ingest custom-status open issues as new tasks. Configure execution admission
separately before enabling a live engine on a held backlog. Orch's internal task
transitions still own its standard lifecycle labels and `workflow.auto_close`;
the board reconciler itself never transitions tasks or dispatches agents. When
using standard lifecycle labels, map their exact names too, for example
`status:new`/`status:routed` to Todo and `status:needs_review`/`status:in_review`
to Review. Do not attach multiple status labels at once.

## Owner Feedback

When the repo owner comments on a GitHub issue linked to a completed task, orch detects the feedback and re-activates the task:

1. Tasks with status `done`, `in_review`, or `needs_review` are checked for new owner comments
2. If new comments are found, the task is reset to `routed` (keeping its agent assignment)
3. The feedback is appended to the task context so the agent sees it on re-run

### Slash Commands

Owner comments can also start with a slash command on the **first line** (case-insensitive). Parsed by `src/engine/commands.rs`.

| Command | Action |
|---|---|
| `/retry` | Reset task to `status:new` (clears agent + attempts) |
| `/reroute [agent]` | Re-route the task (optionally forcing a specific agent) |
| `/unblock` | Clear `blocked` state and reset to `status:new` |
| `/block [reason]` | Mark the task blocked with an optional reason |
| `/close` | Mark `status:done` and close the issue |
| `/review` | Trigger / re-trigger the review agent on the linked PR |

To change agent or complexity without using a slash command, add the corresponding label directly on the issue (`agent:claude`, `complexity:medium`, etc.) — the engine picks up label changes on the next sync.

## Notes

- The repo is resolved from `config.yml` or `gh repo view`
- Tasks with `no_gh` or `local-only` labels are skipped
- Agents never call GitHub directly; orch handles all API calls so it can back off safely. The runner injects `GH_TOKEN` at spawn time for convenience, but agents should not perform repository writes or API calls — orch performs these actions to maintain a single place for backoff and rate-limit handling.

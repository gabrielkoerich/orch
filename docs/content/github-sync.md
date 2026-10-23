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

### Keep board Status aligned with board labels

For a custom board workflow, enable label-authoritative reconciliation with a
separate namespace. `board:held` selects **Held**; replacing it with `board:todo`
selects **Todo**. Orch owns `status:*` for its task lifecycle and removes those
labels on transitions, so this feature rejects that namespace.

```yaml
gh:
  project_label_sync:
    enabled: true
    projects:
      - repo: "owner/repo"
        project_id: "PVT_..."
        field_name: "Status"
        label_prefix: "board:"
        options:
          "board:todo": "Todo"
          "board:held": "Held"
          "board:review": "Review"
          "board:done": "Done"
```

Each entry scopes one repository and Project V2. `field_name` defaults to
`Status`, and `label_prefix` defaults to `board:`. A custom prefix must end in
`:` and cannot use `status:`, `source-status:` or `execution:`. All mapping keys
must belong to that prefix. Option names must already exist and match exactly
and uniquely; field and option IDs are resolved separately for each board.
The token needs Projects write permission (`project` for a classic PAT).

Startup/periodic sync reads existing Issue cards and repairs blank or drifted
fields, including external label changes. Fields, items and labels are paginated;
matching values are no-ops. Exactly one label in the configured prefix is
required. Missing board labels leave the card unchanged. Multiple or unmapped
board labels also leave it unchanged and are logged at debug level. Incomplete or ambiguous schema responses and
GraphQL errors cannot produce guessed updates. Reads and idempotent field
updates use the normal shared GitHub client's retries, rate tracking, backoff
and circuit breaker, without a separate write cap or suspension counter.

To reconcile once without starting ingestion, a TaskStore, routing or agents:

```bash
orch project board reconcile --repo owner/repo
```

The command reports acknowledged field updates and exits unsuccessfully on
configuration/API errors. Skipped items are visible with debug logging.
Reconciliation only changes the configured field on existing, non-archived Issue
items in open Projects. It never edits labels, issue state or memberships.
Legacy ingestion can still add cards to `gh.project_id`; its lifecycle field
writer defers only when that board is explicitly label-managed.

**Use `no-agent` to hold execution.** For example, attach both `board:held` and
`no-agent`: the former selects the column, while the latter makes Orch skip
routing and dispatch. Replacing `board:held` with `board:todo` alone keeps that
hold; removing `no-agent` releases it on the next sync. Board labels survive
Orch's lifecycle transitions and do not themselves pause or resume work.
Historical `source-status:*` and unrelated `execution:held` remain untouched;
they gain no admission semantics from this feature.

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

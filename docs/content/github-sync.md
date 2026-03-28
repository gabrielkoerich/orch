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
orch project info --fix     # auto-fills project field/option IDs into config
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
| complexity | Label `complexity:low/med/high` |
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

When GitHub rate limits or abuse detection triggers, orch sleeps and retries:

```yaml
gh:
  backoff:
    mode: "wait"         # "wait" (default) or "skip"
    base_seconds: 30     # initial backoff duration
    max_seconds: 900     # maximum backoff duration
```

## Projects (Optional)

Link tasks to a GitHub Project v2 board:

```yaml
gh:
  project_id: "PVT_..."
  project_status_field_id: "PVTSSF_..."
  # Optional: set these if your Project "Status" options aren't
  # exactly: Backlog, In Progress, Review, Done
  project_status_names:
    backlog: "Todo"
    in_progress: "Doing"
    review: ["In Review", "Needs Review"]
    done: "Done"
  project_status_map:
    backlog: "option-id-1"
    in_progress: "option-id-2"
    review: "option-id-3"
    done: "option-id-4"
```

Discover IDs automatically:
```bash
orch project info        # show current project field/option IDs
orch project info --fix  # auto-fill into config
```

## Owner Feedback

When the repo owner comments on a GitHub issue linked to a completed task, orch detects the feedback and re-activates the task:

1. Tasks with status `done`, `in_review`, or `needs_review` are checked for new owner comments
2. If new comments are found, the task is reset to `routed` (keeping its agent assignment)
3. The feedback is appended to the task context so the agent sees it on re-run

### Slash Commands

Owner comments can also start with a slash command on the **first line** (case-insensitive):

| Command | Action |
|---|---|
| `/retry` | Reset task to `status:new` (clears agent + attempts) |
| `/assign <agent>` | Set agent (`claude`, `codex`, `opencode`) and move to `status:routed` |
| `/unblock` | Clear `blocked`/error state and reset to `status:new` |
| `/close` | Mark `status:done` and close the issue |
| `/context <text>` | Append text to task context and move to `status:routed` |
| `/priority <low|medium|high>` | Set complexity (`simple|medium|complex`) and move to `status:routed` |
| `/help` | Show supported commands |

## Notes

- The repo is resolved from `config.yml` or `gh repo view`
- Tasks with `no_gh` or `local-only` labels are skipped
- Agents never call GitHub directly; orch handles all API calls so it can back off safely. The runner injects `GH_TOKEN` at spawn time for convenience, but agents should not perform repository writes or API calls — orch performs these actions to maintain a single place for backoff and rate-limit handling.

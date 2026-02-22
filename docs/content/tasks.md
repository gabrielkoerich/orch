+++
title = "Task Model"
description = "Task schema, lifecycle, and delegation"
weight = 2
+++

## Task Schema

Each task in `tasks.yml` includes:

| Field | Description |
|-------|-------------|
| `id` | Unique integer ID |
| `title`, `body`, `labels` | Task description and metadata |
| `status` | `new`, `routed`, `in_progress`, `done`, `in_review`, `blocked`, `needs_review` |
| `agent` | Executor (`codex`, `claude`, or `opencode`) |
| `agent_model` | Model chosen by the router |
| `agent_profile` | Dynamically generated role/skills/tools/constraints |
| `selected_skills` | Skill IDs from `skills.yml` |
| `complexity` | Router-assigned complexity (`simple`, `medium`, `complex`) |
| `parent_id`, `children` | For delegation (parent-child relationships) |
| `summary`, `reason` | What was done / why blocked |
| `accomplished`, `remaining`, `blockers` | Progress tracking |
| `files_changed` | List of modified files |
| `attempts`, `last_error` | Retry tracking |
| `duration` | Execution time in seconds |
| `input_tokens`, `output_tokens` | Token usage |
| `prompt_hash` | SHA-256 prefix of the prompt |
| `review_decision`, `review_notes` | Review agent output |
| `history` | Status changes with timestamps |

GitHub metadata (optional): `gh_issue_number`, `gh_url`, `gh_state`, `gh_updated_at`, `gh_synced_at`.

## Task Lifecycle

```
new → routed → in_progress → done → in_review
                            → blocked
                            → needs_review
```

- **new** — task created (via `task add`, `gh pull`, or `jobs tick`)
- **routed** — LLM router assigned agent, model, profile, skills
- **in_progress** — agent is running
- **done** — agent completed successfully (no open PR)
- **in_review** — agent completed and a PR is open (triggers review agent if enabled)
- **blocked** — agent hit a blocker, crashed, or exceeded max attempts
- **needs_review** — agent needs human help, or review agent requested changes

## Delegation & Decomposition

Complex tasks can be decomposed into subtasks:

1. Router sets `decompose: true` for complex multi-system tasks
2. Agent receives a planning prompt instead of an execution prompt
3. Agent returns `delegations` — a list of child tasks
4. Parent task is blocked until all children are `done`
5. When children finish, parent is unblocked and re-run (rejoin)

Force decomposition manually:
```bash
orch task plan "Complex feature" "Detailed description"
# or add the "plan" label
orch task add "title" "body" "plan"
```

The orchestrator will:
- Create child tasks with `parent_id` set
- Block the parent until children are done
- Re-run the parent via `poll` or `rejoin`

## Concurrency & Locking

- `poll` runs new tasks in parallel (up to `POLL_JOBS=4`)
- File writes are protected by a global lock (`tasks.yml.lock`)
- Each task also has a per-task lock to prevent double-run
- Stale locks are auto-cleared after `LOCK_STALE_SECONDS` (default 600)

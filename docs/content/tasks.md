+++
title = "Task Model"
description = "Task schema, lifecycle, and delegation"
weight = 2
+++

## Task Schema

Tasks are stored in a unified SQLite database (`~/.orch/orch.db`). Both internal tasks (cron jobs, mentions) and external tasks (GitHub Issues) share the same schema and lifecycle. External tasks are also synced to GitHub Issues via labels and comments.

Key fields:

| Field | Description |
|-------|-------------|
| `id` | Internal store ID (auto-increment) |
| `external_id` | GitHub issue number (if external) |
| `repo` | Repository slug (`owner/repo`) |
| `title`, `body`, `labels` | Task description and metadata |
| `status` | `new`, `routed`, `in_progress`, `done`, `in_review`, `blocked`, `needs_review` |
| `agent` | Executor (`codex`, `claude`, `opencode`, `kimi`, `minimax`) |
| `model` | Model chosen by the router |
| `complexity` | Router-assigned complexity (`simple`, `medium`, `complex`) |
| `summary`, `reason` | What was done / why blocked |
| `attempts`, `last_error` | Retry tracking |
| `branch`, `worktree` | Git worktree path and branch name |
| `pr_number` | Associated PR number |
| `input_tokens`, `output_tokens` | Token usage |
| `input_cost_usd`, `output_cost_usd`, `total_cost_usd` | Cost tracking |
| `origin` | Source: `github`, `internal`, `mention`, `job` |
| `source`, `source_id` | For dedup (e.g., mention ID, job ID) |

## Task Lifecycle

```
new → routed → in_progress → needs_review → in_review → done (merged)
                            → done (no PR)
                            → blocked
```

- **new** — task created (via `task add`, `gh pull`, or `jobs tick`)
- **routed** — LLM router assigned agent, model, profile, skills
- **in_progress** — agent is running
- **done** — PR merged (or agent completed with no code changes)
- **in_review** — review agent is actively running on the PR
- **blocked** — agent hit a blocker or crashed and requires human intervention (rare)
- **needs_review** — agent needs human help, or review agent requested changes. After repeated failures the engine moves tasks to `needs_review` and removes any forced `agent:*` label so an owner can reassign.

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

Orch will:
- Create child tasks with `parent_id` set
- Block the parent until children are done
- Re-run the parent via `poll` or `rejoin`

## Concurrency

- The engine dispatches tasks concurrently using a `tokio::sync::Semaphore` (configurable concurrency limit)
- SQLite WAL mode allows concurrent reads; writes are serialized by SQLite
- The `needs_review` to `in_review` status transition serves as an atomic guard against duplicate review agent spawns
- Worktree cleanup runs in a background task so it does not block routing or dispatch

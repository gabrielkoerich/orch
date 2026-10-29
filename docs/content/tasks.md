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
| `agent` | Executor (`codex`, `claude`, `opencode`, `kimi`, `minimax`, `grok`) |
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
  ↑                         → done (no PR)
  │                         → blocked
waiting
```

- **new** — task created (via `task add`, `gh pull`, or `jobs tick`)
- **waiting** — at least one dependency is still open. The router, the dispatcher, `orch task route`, `orch task run`, and `orch task retry` leave the task alone. `orch task unblock all` does not select it. When the last dependency is done, the next tick moves it back to `new`
- **routed** — LLM router assigned agent, model, profile, skills
- **in_progress** — agent is running
- **done** — PR merged (or agent completed with no code changes)
- **in_review** — review agent is actively running on the PR
- **blocked** — agent hit a blocker or crashed and requires human intervention (rare)
- **needs_review** — agent needs human help, or review agent requested changes. After repeated failures the engine moves tasks to `needs_review` and removes any forced `agent:*` label so an owner can reassign.

## Dependencies

A task can wait on other issues. Orch stores GitHub blocked-by links and `Depends on #12`, `Blocked by #12, #13`, and `owner/repo#12` lines from the issue body, deduplicated by repository and number. An issue that names itself is skipped. `orch task add --depends-on` writes a local row when GitHub cannot take the link, and that row is kept across syncs. When the blocked-by read fails, the stored rows stay as they are.

A dependency is done when the blocking pull request is approved and merged into the default branch. Orch posts a `## Automated Review` comment from the reviewer account, and that comment counts as approval. An issue with no pull request is done when it is closed as completed. A local `done` row, an approved pull request that is still open, a merge into another branch, a pull request closed without merge, and an issue closed as not planned do not count. Refresh reads the blocker's issue state from GitHub, including blockers that were already closed when the waiting task was imported. Each open blocker is read at most once every five minutes, with a per-blocker offset so the reads spread out. A blocker orch manages is read at once when its task reaches `done`, so its dependents start on the next tick.

`orch task add "title" --depends-on 12 --depends-on owner/other#4` creates the issue and writes the native links. `orch task deps <id>` lists both directions. `orch task tree --deps` shows blocked-by edges. `orch task list`, `get`, and `inspect` print the open blocker numbers on a waiting task.

A cycle runs no task in the cycle. Orch comments on each issue in the cycle once. If a blocker is blocked, or its issue closes as not planned, the waiting task stays waiting. Orch comments once and sends one notification.

Parent and child tasks are a different link. A parent waits for its children. A dependency says this task does not start until the other task's pull request is merged. The two are stored separately.

When a dependency's pull request merges, the waiting task's next worktree is created from the updated default branch.

## Delegation

When the agent's response includes a `delegations` array, orch creates child tasks and blocks the parent until they finish.

1. The agent emits `delegations: [{ title, body, labels }, ...]` in its JSON response (see `src/parser.rs::Delegation`)
2. The runner (`src/engine/runner/mod.rs::process_delegations`) creates each as a child task with `parent_id` set
3. Parent moves to `blocked` and waits
4. When every child reaches `done`, the engine auto-unblocks the parent (Phase 4 of the engine tick) and re-dispatches it so it can integrate the results

There is no `decompose:true` router flag, no `plan` label, and no `orch task plan` subcommand — delegation is purely agent-driven via the response payload.

## Concurrency

- The engine dispatches tasks concurrently up to `engine.max_parallel` (semaphore-guarded)
- SQLite WAL mode allows concurrent reads; writes are serialized by SQLite
- The `needs_review` to `in_review` status transition serves as an atomic guard against duplicate review agent spawns
- Worktree cleanup runs in a background task so it does not block routing or dispatch

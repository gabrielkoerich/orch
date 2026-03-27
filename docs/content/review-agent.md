+++
title = "Review Agent"
description = "Automated PR reviews using a different agent"
weight = 10
+++

The review agent automatically reviews pull requests after an agent completes a task. It picks a different agent from the one that wrote the code (and from any agents that previously reviewed the same task), and posts a real GitHub PR review.

## How It Works

1. Agent completes a task and returns `status: done`
2. Orchestrator detects an open PR on the task's branch → sets status to `in_review`
3. If `enable_review_agent` is `true`, the review agent runs:
   - Picks a reviewer via round-robin, excluding the task executor and all agents that have previously reviewed this task.
   - Fetches the PR diff via `gh pr diff`
   - Sends the diff, task summary, and files changed to the review agent
   - Parses the review decision

4. Based on the decision:
   - **approve** → posts a comment with `## Automated Review -- Approve` header on the PR, auto-merges if CI passes
   - **request_changes** → posts a comment with `## Automated Review -- Changes Requested` header, task re-routes to `New` for the agent to fix
   - **reject** → posts review comment, closes the PR, task goes to `needs_review`

   Review comments use a `## Automated Review` header as the protocol. A CI workflow (`.github/workflows/orch-review.yml`) reads PR comments with this header to determine approval status.

## Agent Selection

The review agent is chosen via round-robin, excluding the task executor and all agents that have previously reviewed this task:

1. Build an exclude list: the task's executor + all agents that have previously run as reviewer on this task
2. Pick the next available agent (not disabled, installed on the system) not in the exclude list
3. Falls back to `workflow.review_agent` config if no different agent is available
4. Last resort: uses the same agent as the executor

Examples:
- codex wrote the code → claude reviews (first cycle); if claude fails, kimi reviews (second cycle)
- claude wrote the code → codex reviews (first cycle)
- Only one agent installed → that agent reviews (same as executor)

Override the reviewer for a specific run:
```bash
REVIEW_AGENT=claude orch task run <id>
```

## Config

```yaml
workflow:
  enable_review_agent: true    # enable automatic PR reviews (default: false)
  review_agent: "claude"       # fallback reviewer if opposite_agent can't find a different one
```

The `review_agent` config key is now a fallback — `opposite_agent()` handles primary selection. You only need to set `enable_review_agent: true`.

## Review Prompt

The review prompt (`prompts/review_task.md`) sends:

- Task ID and title
- PR number
- Task summary from the executor
- Files changed
- PR diff (first 500 lines via `gh pr diff`)

The reviewer is asked to return JSON:
```json
{
  "decision": "approve|request_changes|reject",
  "notes": "detailed feedback with file paths and line references"
}
```

## Decision Criteria

| Decision | When to use | What happens |
|----------|-------------|--------------|
| **approve** | Changes correctly implement the task, no obvious bugs or security issues | `gh pr review --approve`, task records `review_decision: approve` |
| **request_changes** | Changes are on the right track but have fixable issues (missing edge cases, style, minor bugs) | `gh pr review --request-changes`, task goes to `needs_review` |
| **reject** | Changes are fundamentally wrong — hallucinated APIs, empty diff, broken code, unrelated changes | `gh pr review --request-changes` + `gh pr close`, task goes to `needs_review` |

## GitHub PR Reviews

The review agent posts real GitHub PR reviews (not just issue comments):

- **Approve**: shows as a green checkmark review on the PR
- **Request changes**: shows as a red X review requiring changes before merge
- **Reject**: same as request changes + the PR is closed automatically

All `gh pr review` calls are non-fatal (`|| true`) — if the API fails, the review decision is still recorded locally.

## Task Flow

```
agent completes (done) → PR detected → needs_review → in_review → review agent runs
                                                                   ├─ approve → auto-merge → done
                                                                   ├─ request_changes → re-route to new → agent fixes
                                                                   └─ reject → needs_review (PR closed)
```

Review re-routing: when a review requests changes, the task is re-routed back to `Routed` status (not a child task). The agent reuses the existing worktree/branch and commits fixes — the engine handles pushing to the same PR. If `review_cycles >= max_review_cycles`, the task is blocked for human review.

Without the review agent enabled, tasks with open PRs still transition to `in_review` but skip the automated review step.

## Observability

Review events appear in:
- **SQLite store**: `review_decision` field and review counters
- **GitHub PR**: review comment with `## Automated Review` header
- **Server logs**: structured tracing spans for review agent lifecycle
- **Per-task artifacts**: prompts and agent output at `~/.orch/state/{repo}/tasks/{id}/attempts/`

+++
title = "Review Agent"
description = "Automated PR reviews using a different agent when one is available"
weight = 10
+++

The review agent automatically reviews pull requests after an agent completes a task. It picks a different agent from the one that wrote the code (and from any agents that previously reviewed the same task), and posts its decision on the PR, see [Review mode](@/configuration.md#review-mode). If no other agent is available, the authoring agent reviews and the fallback is logged as a `review_agent_fallback` task activity and noted in the review comment.

## How It Works

1. Agent completes a task and returns `status: done`
2. Orch detects an open PR on the task's branch → sets status to `needs_review`, then `in_review` when the review starts
3. If `enable_review_agent` is `true` (the default), the review agent runs, limited to `workflow.review_timeout_seconds` (default 900, never under 300):
   - Picks a reviewer via round-robin, excluding the task executor and all agents that have previously reviewed this task.
   - Runs in the task's worktree with the diff, git log, task body and the agent's summary (`prompts/review_task.md`)
   - Parses the review decision

4. Based on the decision:
   - **approve** → posts a comment with `## Automated Review -- Approve` header on the PR and moves the task to `approved`. orch then waits for CI and merges with no agent running, and the merged PR moves the task to `done`. A merge conflict or red CI sends the task back to `routed`, and a PR closed without merge blocks it. An approve that reports `test_results: fail` or an `error` severity issue counts as request_changes. Required checks come from classic branch protection and from repository rulesets. A missing required check is pending, so a PR with no check runs yet does not merge. GitHub `unstable` (a non-required check failing or still running) still merges once every required check has passed. A repository with no required checks waits until running checks finish.
   - **request_changes** → posts a comment with `## Automated Review -- Changes Requested` header, task goes back to `routed` with the same agent and worktree to fix it

   Review comments use a `## Automated Review` header as the protocol. orch also posts the decision as a commit status on the PR head SHA, context `orch/review`: `pending` while the review runs, `success` for approve, `failure` for changes requested. A new push moves the head SHA, so an old approval does not carry over. `orch init` no longer installs `.github/workflows/orch-review.yml`. A repository that has it can delete the file and the `ORCH_REVIEWER` variable.

   With `review.mode: app` and the GitHub App installed on the repository, the review agent's decision becomes a real App review with inline comments, and `orch/review` is a check run from the App instead of a commit status. See [Review mode](../configuration/#review-mode).

## Agent Selection

The review agent is chosen via round-robin, excluding the task executor and all agents that have previously reviewed this task:

1. Build an exclude list: the task's executor + all agents that have previously run as reviewer on this task
2. Pick the next routable agent (installed, not cooled, not degraded) outside the exclude list that has a `model_map.review` model
3. If no such agent is left, the first routable agent reviews, which can be the executor. orch logs a `review_agent_fallback` activity and says so in the review comment

Examples:
- codex wrote the code → claude reviews (first cycle); if claude fails, kimi reviews (second cycle)
- claude wrote the code → codex reviews (first cycle)
- Only one agent installed → that agent reviews (same as executor)

## Config

```yaml
workflow:
  enable_review_agent: true    # default true, false skips the review agent
  review_timeout_seconds: 900  # review time limit, never under 300
  max_review_cycles: 2         # change requests before the task is blocked
review:
  mode: comment                # comment or app
```

A review that times out puts no model in cooldown. The task goes back to `needs_review` and the next review picks an agent as usual. On startup, every `in_review` task with no review session goes back to `needs_review`. While orch runs, an `in_review` task with no review session for `engine.in_review_no_session_stuck_timeout` goes back to `needs_review`. orch keeps that limit above the review timeout plus 120 seconds, so a finished review still on its way is not dropped.

## Review Prompt

The review prompt (`prompts/review_task.md`, with `prompts/review_system.md`) sends:

- Task ID, title and body
- PR number and default branch
- Task summary from the executor
- Git log and diff of the branch

The reviewer rebases, checks the required CI checks, and returns JSON:
```json
{
  "decision": "approve|request_changes",
  "notes": "detailed feedback with file paths and line references",
  "test_results": "pass|fail|skipped",
  "issues": [{"file": "src/foo.rs", "line": 42, "severity": "error|warning", "description": "..."}]
}
```

## Decision Criteria

| Decision | When to use | What happens |
|----------|-------------|--------------|
| **approve** | Changes correctly implement the task, no obvious bugs or security issues | `orch/review` passes, the task moves to `approved` and orch merges once CI passes |
| **request_changes** | The changes have fixable issues, or the reviewer could not resolve a conflict | `orch/review` fails, the task goes back to `routed` |

In `comment` mode the decision is a `## Automated Review` comment and an `orch/review` commit status. In `app` mode it is also a real `APPROVE` or `REQUEST_CHANGES` review from the GitHub App with inline comments, and `orch/review` is a check run. See [Review mode](@/configuration.md#review-mode).

## Task Flow

```
agent completes (done) → PR detected → needs_review → in_review → review agent runs
                                                                   ├─ approve → approved → CI passes, merge → done
                                                                   └─ request_changes → routed → agent fixes
```

Review re-routing: when a review requests changes, the task goes back to `routed` (not a child task). The agent reuses the existing worktree/branch and commits fixes — the engine handles pushing to the same PR. If `review_cycles >= max_review_cycles`, the task is blocked for human review.

With `enable_review_agent: false`, no review agent starts for a task that enters `needs_review`.

## Observability

Review events appear in:
- **SQLite store**: `review_decision` field and review counters
- **GitHub PR**: review comment with `## Automated Review` header
- **Server logs**: structured tracing spans for review agent lifecycle
- **Per-task artifacts**: prompts and agent output at `~/.orch/state/{repo}/tasks/{id}/attempts/`

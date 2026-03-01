+++
title = "Review Agent"
description = "Automated PR reviews using a different agent"
weight = 10
+++

The review agent automatically reviews pull requests after an agent completes a task. It picks a different agent from the one that wrote the code (e.g. if codex wrote the code, claude reviews it), and posts a real GitHub PR review.

## How It Works

1. Agent completes a task and returns `status: done`
2. Orchestrator detects an open PR on the task's branch → sets status to `in_review`
3. If `enable_review_agent` is `true`, the review agent runs:
   - Picks the **opposite agent** (first available agent different from the executor)
   - Fetches the PR diff via `gh pr diff`
   - Sends the diff, task summary, and files changed to the review agent
   - Parses the review decision

4. Based on the decision:
   - **approve** → posts `gh pr review --approve`, task stays `in_review` with `review_decision: approve`
   - **request_changes** → posts `gh pr review --request-changes`, task goes to `needs_review`
   - **reject** → posts `gh pr review --request-changes`, closes the PR, task goes to `needs_review`

## Agent Selection

The `opposite_agent()` function picks the reviewer:

1. Iterates available agents (not disabled, installed on the system)
2. Returns the first agent that differs from the task's executor
3. Falls back to `workflow.review_agent` config if no different agent is available
4. Last resort: uses the same agent

Examples:
- codex wrote the code → claude reviews
- claude wrote the code → codex reviews
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
agent completes (done) → PR detected → in_review → review agent runs
                                                   ├─ approve → stays in_review (PR ready to merge)
                                                   ├─ request_changes → needs_review
                                                   └─ reject → needs_review (PR closed)
```

Without the review agent enabled, tasks with open PRs still transition to `in_review` but skip the automated review step.

## Observability

Review events appear in:
- **Task history**: `review approved by claude`, `review requested changes`, `review rejected`
- **Task YAML**: `review_decision` and `review_notes` fields
- **GitHub PR**: real PR review with the reviewer's notes as the review body
- **Server logs**: `[run] task=N starting review by claude for PR #42`

---
name: orch
description: "Use when monitoring or operating orch: status reports, watching tasks, investigating slow, failed or blocked tasks, retrying or unblocking a task, routing and cooldown checks, weekly reviews and agent debugging. Do not use it for ordinary feature work on orch's code."
---

# Orch Operations Skill

Use this skill to monitor orch, find out why a task is slow or blocked, and decide what to do about it. Every step uses the `orch` CLI. Do not query or edit `~/.orch/orch.db` directly: the CLI shows the same data, and a direct write skips orch's own state handling.

## Non-negotiable policy

- Do not tell agents to run `brew` commands.
- Do not recommend `brew upgrade`, `brew install`, `brew services restart`, or similar shared-state changes as a normal fix.
- Do not report CLI, service or `main` version drift as an issue, finding or action item.
- Do not suggest `orch -V` or version-parity checks as follow-up.
- Do not use `orch task retry`, `orch task unblock` or `orch task kill` as the default fix. Find the cause first (see "Retry, unblock and reroute").
- Do not edit `~/.orch/config.yml`, `config.example.yml` or a project's `.orch.yml`. If a config change is needed, describe it for the operator.
- Do not name the projects orch works on in issues, PRs or this file, and do not include local machine paths. Describe the behavior generically.

## Task lifecycle

| Status | Meaning | Needs a person? |
|---|---|---|
| `new` | Created, waiting to be routed | No |
| `routed` | Agent and model chosen, waiting to dispatch | No |
| `waiting` | Waits on another task (`orch task deps <id>` shows which) | No, unless that task is human-only |
| `in_progress` | An agent works in its tmux session | No |
| `needs_review` | The agent finished, the review agent picks it up | No |
| `in_review` | The review agent is reviewing the PR | No |
| `done` | PR merged, worktree removed. Terminal: nothing moves a task out of `done` | No |
| `blocked` | orch could not continue (failure limit, review cycles, a missing permission) | Yes, after reading why |

`needs_review` means the review agent's turn. Only `blocked` waits for a person.

## Monitoring workflow

Start with observation:

```bash
orch task list --global          # every task in every project
orch task list --status blocked  # only what needs attention
orch stats dashboard             # tasks, sessions and recent activity
orch log 100                     # last 100 lines of the service log
orch log watch                   # live tail, never ends: run it with a timeout or in a background watcher
orch cooldown list               # agent and model cooldowns
orch service status
```

For one task:

```bash
orch task inspect <id>   # status, agent, attempts, last error, PR, block reason
orch task runs <id>      # every agent and review run, with outcome and error
orch task logs <id>      # post-mortem with the run trail and exit codes
orch task history <id>   # routing history per attempt
orch task deps <id>      # what it waits on, and what waits on it
orch task watch <id>     # its agent work live, including the review
orch task session <id>   # the agent's session output, readable
orch task cost <id>      # tokens and cost
```

Use explicit ids such as `owner/repo#58`. A bare number can match an internal task in another project.

## Where to look when a task is slow, retried or blocked

Read before guessing. Each task keeps its full state on disk, under `~/.orch/state/<owner>/<repo>/tasks/<n>/`:

| What | Where |
|---|---|
| Why orch chose this agent and model | `routing/prompt.txt` and `routing/response.txt` |
| What the agent was told | `attempts/<k>/prompt-msg.md` and `prompt-sys.md` |
| What the agent did | `attempts/<k>/output.json`, the agent's stream, which grows while it runs, and `stderr.txt` |
| The review | `../<n>-review/attempts/<k>/output.json` |

Common signals in `orch task logs` and `orch task runs`:

| Signal | Meaning |
|---|---|
| exit code 124 | The agent hit the 30 minute run limit. Look at what it was waiting on |
| "silence detection" | The agent stopped producing output |
| the same command repeated in `output.json` | The agent retries something that hangs or fails the same way each time |
| `blocked` with a permission or push error | The agent tried something orch does for it, or lacks access |

Workflow:

1. `orch task inspect <id>` and `orch task runs <id>` for the state and the last error.
2. Check that `prompt-msg.md` has the current issue text. An agent that started before the issue changed works from the old text.
3. Read the end of `output.json` for the agent's last reasoning and tool calls.
4. Name the root cause. Typical causes: a test that never ends, a call that waits for a person (a sign-in, a permission prompt, a purchase sheet), a missing rule in the project's `AGENTS.md`, an issue that asks for something the environment cannot do.
5. Fix the cause where every agent sees it: the issue text, or the project's `AGENTS.md`. A rule in `AGENTS.md` reaches every later task, and a fix in one issue reaches only that task. If it is an orch bug, file a generic issue (see "Checks before filing a new issue").
6. Let orch end and rerun the task. A hung run ends at the 30 minute limit and the next attempt reads the updated issue.

## Retry, unblock and reroute

These commands change a task's state. Use them after the cause is understood or fixed, one task at a time, and never to clear a backlog.

| Command | Use when | Effect |
|---|---|---|
| `orch task unblock <id>` | A blocked task's cause is fixed, for example access was granted or a billing problem was solved | Moves it back so orch routes it again, attempts reset |
| `orch task retry <id>` | A finished or failed task must run again from the start, for example after its issue was rewritten | Resets it to `new`, attempts reset |
| `orch task reroute <id> --agent <agent> [--model <model>]` | One agent or model keeps failing the same task and another fits better | Same as retry, with the agent forced |
| `orch task kill <id>` | Rarely: a session that is truly dead (an idle shell, no agent) and blocks dispatch | Ends the tmux session. The run stays open until its limit, so a retry right after still waits |
| `orch cooldown clear` | A cooldown was recorded by mistake, or an agent stays degraded from rate rows | Removes the cooldown and the degraded rate-limit window |
| `orch task close <id>` | The work was done outside orch | Marks it `done` without running an agent |

Do not kill or retry a running task to make it pick up a changed issue. It ends on its own and the next attempt reads the new text. Never use `orch task unblock all`.

## Planning tasks with `orch plan import`

- `orch plan import <roadmap> --dry-run` shows what it would create, update and link. Run it first.
- `--milestone <id>` imports one milestone, and `--skip-unmet` skips a task whose blocker outside the milestone has no issue yet.
- Put human-only tasks (accounts, recordings, legal review, store setup) in their own milestone at the end. An agent task that depends on a human-only task waits until a person finishes it, and every task behind it waits too. Where an agent task needs something only a person can set up, give it a local stand-in instead, such as a local test configuration or a stubbed service.
- Each issue carries a plan marker. Two issues with the same marker make the import fail, so remove the marker from the duplicate.

## Service

```bash
orch service status
orch service doctor    # state that disagrees between orch and GitHub
orch service prune     # worktrees no task owns
orch service restart   # operator only
```

On a restart, orch moves `in_progress` tasks back to `routed` and `in_review` tasks back to `needs_review`, ends all `orch-*` tmux sessions, and dispatches again one task per tick. A run with outcome `aborted` and a "graceful shutdown" error is expected after a restart.

## Log freshness

Before reporting an error from the service log, compare its timestamp with `date`. An error older than the last tick (10 seconds by default) is history. The log rotates, so a live watch must reopen the file after rotation (`orch log watch` does).

## How to respond to problems

1. Identify the concrete failure mode.
2. Check whether repo policy already says the behavior is expected.
3. Check whether the bug was already fixed recently.
4. File or update a root-cause issue only if the problem is still real on current `HEAD`.
5. Take a manual step only when policy allows it and the root cause or external dependency is addressed.

Good findings name a root cause: parser or classifier gaps, cooldown or routing bugs, stale session cleanup, PR and task state that disagree, external failures at the right boundary (such as GitHub Actions billing at merge time). Avoid "retry everything" or "drain the blocked queue".

## Manual intervention policy

Manual steps are the operator's choice, taken after the cause is clear.

Allowed after the cause is fixed or understood:

- unblock the tasks a fixed billing or access problem blocked
- clear a session confirmed dead
- re-run work after its root cause is fixed in code

Not allowed as routine advice:

- `orch task unblock all` to drain a backlog
- `orch task retry` instead of understanding the failure
- editing orch's database directly
- "upgrade and restart orch" to pick up a fix

## Version and deployment guidance

Do not compare the running binary to `main`, flag "not deployed yet" as a bug, tell the operator to upgrade or restart, or treat deployment lag as something agents manage. If a fix exists only on `HEAD`, say the bug is fixed in the repo and stop there.

## Reporting format

For recurring status reports, use a table:

```text
# 22:30 UTC

ID | Status | Agent | Model | PR | Age | Title
-- | ------ | ----- | ----- | -- | --- | -----
1082 | in_progress | claude | sonnet | — | 12m | investigate router timeout
1037 | in_review | codex | gpt-5.5 | #1043 | 5m | fix parser alias

---

Changes: #1082 at 12m, its tests pass and it is committing. #1037 waits on review.
```

Say why a task is where it is. "Still running" or "trying again" without a reason is not a report.

Focus on stuck or aging tasks, repeated failure patterns, cooldown or routing anomalies, and whether behavior matches settled policy.

## Checks before filing a new issue

Always check all three:

```bash
gh issue list --state open
gh issue list --state closed --limit 50
git log --since="48 hours ago" --oneline
```

Do not re-file version-drift complaints, expected cooldown behavior, expected per-task billing blocks at merge time, or problems already fixed on recent `HEAD`.

## Common interpretations

| Symptom | Interpretation and next step |
| --- | --- |
| Task blocked on GitHub Actions billing | Correct merge-time block. Fix billing, then the operator may unblock |
| Model rate-limited again and again | Usually expected cooldown behavior, unless the classifier or cooldown scope is wrong |
| PR merged but task status stale | A reconciliation bug worth investigating. `orch service doctor` lists it |
| Review session vanished mid-flight | A session lifecycle or review rebroadcast bug worth investigating |
| All agents cooled | Retrying on the next tick is expected. Do not propose per-task defer timers |
| Exit code 124 on several attempts of one task | The task hangs on the same step each time. Read `output.json`, fix the issue or `AGENTS.md`, and let the next attempt run |
| Tasks `waiting` for hours | `orch task deps <id>`: the chain probably ends at a human-only task. Move human work to the end of the plan |
| Agent `blocked` because a push was denied | orch pushes branches and opens PRs for agents. If orch did not push the agent's commits, that is an orch bug |
| New issues never become tasks | Check `orch log` for ingestion skipping them, for example an author orch does not trust after a repository moved to an organization |
| `orch plan import` fails on a duplicate marker | Two issues carry the same plan marker. Remove it from the duplicate |
| Task fails because its acceptance needs an account or service agents cannot reach | Rewrite the issue so it tests locally with a stand-in, and move the real setup to the human milestone |

## Scope of edits from monitoring jobs

The weekly review and agent-debugger jobs treat this file as reference, and do not rewrite it in passing.

- Weekly review: summarize, diagnose, and file issues when warranted.
- Agent debugger: analyze failures and file root-cause issues.
- If this skill drifts from repo policy, file or update a dedicated issue.

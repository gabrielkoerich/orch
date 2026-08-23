---
name: orch
description: "Use when monitoring orch operations from this repository: daily reviews, retrospectives, agent-debugging, task health checks, blocked-task triage, routing/cooldown inspection, or global status reporting. Do not use this as a generic coding skill. Do not use it when the task is ordinary feature work unrelated to orch operations."
---

# Orch Operations Skill

Use this skill for operational monitoring and triage in the `orch` repository. It is the repo-tracked source of truth for monitoring guidance used by the scheduled review/debug jobs.

## Non-negotiable policy

- Do not tell agents to run `brew` commands.
- Do not recommend `brew upgrade`, `brew install`, `brew services restart`, or similar shared-state mutation as a normal remediation path.
- Do not report CLI/service/main version drift as an issue, finding, or action item.
- Do not suggest `orch -V` or version-parity checks as operational follow-up.
- Do not recommend routine `orch task retry`, `orch task unblock`, or direct SQLite task-status edits as the default fix.
- Do not suggest editing `~/.orch/config.yml`, `config.example.yml`, or project `.orch.yml` files. If config changes are truly required, describe them for the operator without editing them.

## Monitoring workflow

Start with observation, not intervention:

```bash
orch task list --global
orch log 100
sqlite3 ~/.orch/orch.db "SELECT agent, model, outcome, COUNT(*) FROM task_runs WHERE datetime(started_at) > datetime('now', '-24 hours') GROUP BY agent, model, outcome ORDER BY COUNT(*) DESC;"
```

When a specific task needs inspection:

```bash
sqlite3 ~/.orch/orch.db "SELECT id, external_id, status, title, last_error, block_reason FROM tasks WHERE external_id = 'TASK_ID';"
sqlite3 ~/.orch/orch.db "SELECT attempt, run_type, agent, model, outcome, error, completed_at FROM task_runs WHERE task_id = DB_ID ORDER BY attempt;"
gh pr view PR_NUMBER --repo owner/repo --json state,mergeStateStatus
```

Use SQLite for diagnosis and evidence gathering. Do not use it as the normal way to mutate task state.

## How to respond to problems

Default posture:

1. Identify the concrete failure mode.
2. Check whether repo policy already says the behavior is expected.
3. Check whether the bug was already fixed recently.
4. File or update a root-cause issue only if the problem is still real on current `HEAD`.
5. Recommend manual intervention only when policy explicitly allows it and only after the root cause or external dependency has been addressed.

Prefer root-cause findings such as:

- parser/classifier gaps
- cooldown or routing bugs
- stale tmux/session cleanup failures
- PR/task state reconciliation bugs
- external-system failures at the correct boundary, such as GitHub Actions billing blocks at merge time

Avoid symptom-only recommendations such as "just retry everything" or "drain the blocked queue."

## Manual intervention policy

Manual intervention is exceptional operator action, not the default runbook.

Allowed examples:

- After GitHub Actions billing is fixed, the operator may unblock affected tasks.
- After a stale tmux session is confirmed as the blocker, the operator may clear that session.
- After a root cause is fixed in code, the operator may choose to re-run affected work.

Not allowed as routine advice:

- mass `orch task unblock all` as backlog drainage
- `orch task retry` as a substitute for understanding the failure
- direct SQLite `UPDATE tasks ...` resets
- "upgrade and restart orch" to pick up a fix

If you mention a manual action at all, make it explicit that it is an operator choice taken after the underlying cause is fixed or understood.

## Version and deployment guidance

Do not:

- compare the running binary to `main`
- flag "not deployed yet" as an operational bug
- tell the operator to upgrade or restart orch
- treat deployment lag as something agents should manage

If a fix exists only on `HEAD`, say only that the bug is fixed in the repo and future runs should be evaluated against that code path. Stop there.

## Reporting format

For recurring status reports, use a table:

```text
# 22:30 UTC

ID | Status | Agent | Model | PR | Age | Title
-- | ------ | ----- | ----- | -- | --- | -----
1082 | in_progress | claude | sonnet | — | 12m | investigate router timeout
1037 | in_review | codex | gpt-5.2 | #1043 | 5m | fix parser alias

---

Changes: #1082 still running. #1037 awaiting review outcome.
```

Focus on:

- stuck or aging tasks
- repeated failure patterns
- cooldown/routing anomalies
- whether the current behavior matches settled policy

## Checks before filing a new issue

Always verify all three:

```bash
gh issue list --state open
gh issue list --state closed --limit 50
git log --since="48 hours ago" --oneline
```

Do not re-file:

- version-drift complaints
- expected cooldown behavior
- expected per-task billing blocks at merge time
- problems already fixed on recent `HEAD`

## Common interpretations

| Symptom | Interpretation |
| --- | --- |
| Task blocked on GitHub Actions billing | Correct per-task merge-time block; fix billing first, then operator may unblock |
| Model repeatedly rate-limited with cooldowns | Usually expected generic cooldown behavior unless the classifier/cooldown scope is wrong |
| PR merged but task status stale | Reconciliation/state bug worth investigating |
| Review session vanished mid-flight | Session lifecycle or review rebroadcast bug worth investigating |
| All agents cooled | Loud retry-on-next-tick behavior is expected; do not propose per-task defer timers |

## Scope of edits from monitoring jobs

The daily review and agent-debugger jobs should treat this file as reference material, not as something to rewrite opportunistically.

- Daily review: summarize, diagnose, and file issues when warranted.
- Agent debugger: analyze failures and file root-cause issues.
- If you notice this skill has drifted from repo policy again, file or update a dedicated issue instead of patching random home-directory copies.

---
id: morning-review
schedule: 0 10 * * *
title: Daily morning review
---

Morning check-in. Your ONLY output is a summary post — do NOT modify code.

## Gather context

1. `git log --since="24 hours ago" --oneline` — what changed recently
2. Read the most recent post in `./docs/content/posts/` — current state and context
3. Read the most recent evening retrospective — carry forward unfinished priorities
4. `gh issue list --state open` — what's in the pipeline
5. `orch task list` — check for stuck, blocked, or failing tasks

## Check operational health

1. Are any tasks stuck or failing repeatedly? Note the pattern and root cause.
2. Are there error patterns in logs (`orch log 200`)? Note them.
3. Did the evening retro flag anything? Is it resolved or still pending?
4. Are there tasks waiting on owner feedback?
5. Check task_runs for agent/model failure patterns: `sqlite3 ~/.orch/orch.db "SELECT agent, model, outcome, COUNT(*) FROM task_runs WHERE started_at > datetime('now', '-24 hours') GROUP BY agent, model, outcome ORDER BY COUNT(*) DESC;"`
6. If `task_activity` table exists, check recent events: `sqlite3 ~/.orch/orch.db "SELECT event_type, COUNT(*) FROM task_activity WHERE timestamp > datetime('now', '-12 hours') GROUP BY event_type ORDER BY COUNT(*) DESC;" 2>/dev/null`

## Write the summary

Save to `./docs/content/posts/morning-review-YYYY-MM-DD.md` (today's UTC date).
If the file already exists, update it with new information or improvements. If there's nothing new to add, skip — don't rewrite or duplicate.

Include: recent commits summary, operational health status, stuck tasks, retro follow-ups, and what the day's priorities should be.

## Create GitHub issues if needed

If you find operational problems (stuck tasks, recurring failures, error patterns), create issues with `gh issue create --title "..." --body "..." --label "bug"`.

Before creating any issue:
- `gh issue list --state open` — don't duplicate existing issues
- `gh issue list --state closed --limit 50` — don't re-file resolved problems
- `git log --since="7 days ago" --oneline` — verify the problem wasn't already fixed recently
- If you find any errors on /opt/homebrew/var/log/orch.error.log, first CHECK THE LAST UPDATE DATE OF THIS LOG. DO NOT REFILE issues if this log is stale.
- **Before filing about routing, performance, or config: `rg <term> src/` to verify no existing mechanism already addresses it.**

- Maximum 2-3 issues. Only for operational problems found during this review.
- Focus on ROOT CAUSES, not symptoms
- Do NOT create feature/improvement issues — that's the development job's responsibility.

Commit and push the summary post.

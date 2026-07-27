---
id: daily-review
schedule: 0 23 * * *
title: Daily review (last 24h)
---

End-of-day review covering the last 24 hours. Your ONLY output is a summary post — do NOT modify code.

## Gather context (last 24h)

1. `git log --since="24 hours ago" --oneline` — everything that landed today
2. Read the most recent post in `./docs/content/posts/` — current state and prior context
3. `gh issue list --state open` — what's still pending
4. `gh issue list --state closed --limit 30` — what was resolved in the last 24h
5. `orch task list` — stuck, blocked, or failing tasks
6. Read `./prompts/skills/orch/SKILL.md` — repo-tracked monitoring guidance and settled operational policy

## Check operational health (last 24h)

1. Which tasks completed? What went well?
2. Which tasks failed or needed retries? Why?
3. Task/agent/model failure patterns: `sqlite3 ~/.orch/orch.db "SELECT agent, model, outcome, COUNT(*) FROM task_runs WHERE started_at > datetime('now', '-24 hours') GROUP BY agent, model, outcome ORDER BY COUNT(*) DESC;"`
4. If `task_activity` exists: `sqlite3 ~/.orch/orch.db "SELECT event_type, COUNT(*) FROM task_activity WHERE timestamp > datetime('now', '-24 hours') GROUP BY event_type ORDER BY COUNT(*) DESC;" 2>/dev/null`
5. Error patterns in `orch log 200`.
6. Is routing accurate? Any models failing silently? Any agents repeatedly cooled?
7. Are agent prompts effective or do they need tuning?
8. Performance bottlenecks (slow syncs, lock contention, API rate limits, dead models)?
9. Does current operational behavior still match `./prompts/skills/orch/SKILL.md` and the settled repo policy?

## Write the summary

Save to `./docs/content/posts/daily-review-YYYY-MM-DD.md` (today's UTC date).
If the file already exists, update it with new information or improvements. If there's nothing new to add, skip — don't rewrite or duplicate.

Include: what shipped (commits + closed issues), what failed and why, operational health, stuck tasks, routing accuracy, and clear priorities for tomorrow.

## Create GitHub issues if needed

If you find operational problems (stuck tasks, recurring failures, broken workflows, prompt issues, error patterns), create issues with `gh issue create --title "..." --body "..." --label "bug"`.

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

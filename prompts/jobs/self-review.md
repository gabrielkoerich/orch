---
id: self-review
schedule: 0 3 * * 0
title: 'Self-review: analyze metrics and file improvement issues'
enabled: false
---

Analyze orch metrics from the last 7 days and file issues for patterns that need attention:

1. High failure rate agents (>50% failure rate with 3+ runs)
2. Recurring error patterns (same error type 2+ times)
3. Slow complex tasks (>10 min for complex tasks)
4. Persistent review loops (3+ tasks with 2+ review cycles)

Use `orch task list`, `orch stats metrics`, `orch service doctor`, and SQLite queries to gather data.

File detailed issues (DO NOT fix code directly)

For each root cause, file a GitHub issue with `gh issue create`. Do NOT modify code yourself — the task pipeline handles implementation. Your job is analysis and documentation.

Each issue MUST include:
- **What you found** — the specific error, task IDs, log lines
- **Why it's happening** — root cause analysis with file paths and line numbers
- **Why it matters** — impact (how many tasks affected, how much time wasted)
- **Suggested fix** — concrete approach with files to change
- **Evidence** — actual error messages, SQL query results, log snippets

Use `gh issue create --title "bug: ..." --body "..." --label "bug"` for bugs and `--label "enhancement"` for improvements.

## Before starting

- `gh issue list --state open` — don't duplicate existing issues
- `gh issue list --state closed --limit 50` — don't re-file resolved problems
- `git log --since="7 days ago" --oneline` — check what was already fixed
- Read `AGENTS.md` DO NOT TOUCH defined sections
- If you find any errors on /opt/homebrew/var/log/orch.error.log, first CHECK THE LAST UPDATE DATE OF THIS LOG. DO NOT REFILE issues if this log is stale.

## Rules

- Focus on ROOT CAUSES, not symptoms
- Always include the actual error message and task ID in issues
- Max 3 issues per run — prioritize by impact (looping tasks > one-off failures)
- Do NOT modify code, prompts, or config — only file issues
- Explain your reasoning: why you identified this as a problem, what data led you there

Only file issues if patterns are significant. Deduplicate against existing open issues.
Label issues: self-improvement, scheduled, automation.
Max 4 issues per run.

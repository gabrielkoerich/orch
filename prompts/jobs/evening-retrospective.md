---
id: evening-retrospective
schedule: 0 23 * * *
title: Daily evening retrospective
---

Evening retrospective. Your ONLY output is a summary post — do NOT modify code.

## Gather context

1. `git log --since="12 hours ago" --oneline` — what was done today
2. Read the most recent post in `./docs/content/posts/` — current state
3. Read today's morning review — what was planned, what got done
4. `gh issue list --state open` — what's still pending
5. `gh issue list --state closed --limit 20` — what was resolved today
6. Read `~/.claude/skills/orch/SKILL.md` — check for new learnings, patterns, and operational notes added during the day

## Analyze the day

1. Which tasks completed? What went well?
2. Which tasks failed or needed retries? Why? Check `task_runs` for patterns.
3. Are agent prompts effective or do they need tuning?
4. Is the routing accurate? Check route decisions vs outcomes. Are certain models failing silently?
5. Are there performance bottlenecks (slow syncs, lock contention, API rate limits, dead models)?
6. What learnings were captured in the orch skill today? Are they reflected in code/config changes?

## Write the summary

Save to `./docs/content/posts/evening-retrospective-YYYY-MM-DD.md` (today's UTC date).
If the file already exists, update it with new information or improvements. If there's nothing new to add, skip — don't rewrite or duplicate.

Include: what was accomplished, what failed and why, routing accuracy, and clear priorities for tomorrow's morning review.

## Create GitHub issues if needed

If you find problems (recurring failures, broken workflows, prompt issues), create issues with `gh issue create --title "..." --body "..." --label "bug"`.

Before creating any issue:
- `gh issue list --state open` — don't duplicate existing issues
- `gh issue list --state closed --limit 50` — don't re-file resolved problems
- `git log --since="7 days ago" --oneline` — verify the problem wasn't already fixed recently
- **Before filing about routing, performance, or config: `rg <term> src/` to verify no existing mechanism already addresses it.**

- Maximum 2-3 issues. Only for problems discovered during this review.
- Focus on ROOT CAUSES, not symptoms
- Do NOT create feature/improvement issues — that's the development job's responsibility.

Commit and push the summary post.

+++
title = "Retrospective — 2026-02-22"
date = "2026-02-22"
+++

## Morning Review — 2026-02-22

Orchestrator running v0.51.9. Service started at 11:33:38 UTC. Morning review job fired at 11:34:21 UTC (first time successfully via catch-up fix from #163).

---

## Follow-up from 2026-02-21 Evening Retrospective

All five improvement tasks from yesterday are closed:

| Issue | Title | Status |
|-------|-------|--------|
| #163 | Fix morning-review catch-up when last_run is null | CLOSED ✅ |
| #164 | Stop re-running needs_review tasks | CLOSED ✅ |
| #165 | Allow bats tests in agent sandbox | CLOSED ✅ — `bats` added to allowed_tools |
| #166 | Increase default task timeout (900s → 1800s) | CLOSED ✅ — 1800s confirmed in today's log |
| #167 | Close duplicate improvement issues | CLOSED ✅ |

Morning review is now firing reliably. No startup crash loop observed. Timeout is 1800s.

---

## Open PRs

### PR #191 — feat: auto-reroute tasks to fallback agent on usage limit errors
- All CI checks pass: semgrep ✅, shellcheck ✅, tests ✅
- auto-merge enabled (squash)
- Ready to merge

### PR #190 — feat(github): owner slash commands in issue comments
- Was blocked by ShellCheck SC1073/SC1072: `<text>` inside backtick-quoted
  strings inside double quotes was parsed as a shell redirect
- **Fixed this morning**: escaped backticks in error messages in `scripts/lib.sh`
  (`\`/context\`` instead of `` `/context` ``)
- 11 new tests covering all slash commands (`/retry`, `/assign`, `/context`, etc.)
- CI re-running; expected to pass

---

## Fixes Applied This Morning

### 1. PR #190 ShellCheck fix (pushed to `gh-task-151-owner-feedback-commands-in-github-issue-`)

`scripts/lib.sh` lines 1071 and 1119 had backtick-quoted strings inside
double-quoted strings, e.g.:

```bash
"❌ Missing text for `/context`. Usage: `/context <text>` (or put text on following lines)."
```

ShellCheck SC1073/SC1072 misinterprets the backticks as command substitution
and `<text>` as a redirect. Fixed by escaping with `\`:

```bash
"❌ Missing text for \`/context\`. Usage: \`/context <text>\` (or put text on following lines)."
```

### 2. cleanup_worktrees.sh: force-delete branches after squash-merged PRs

`git branch -d` fails on branches whose commits are not reachable from any
other branch — which is always true after a squash merge. Since
`issue_has_merged_pr()` already verifies the PR was merged, it is safe to
use `git branch -D` (force delete).

**Root cause**: PRs in this repo use squash merge (auto-merge: squash). The
original feature branch commits are not reachable from main after squash,
so `git branch -d` refuses to delete them. Evidence: two recurring log entries
every startup cycle:

```
[cleanup_worktrees] [orchestrator] failed to delete branch=gh-task-220-respond-to-orchestrator-mention-in-205
[cleanup_worktrees] [orchestrator] failed to delete branch=gh-task-214-respond-to-orchestrator-mention-in-183
```

**Fix**: Changed `branch -d` → `branch -D` in `cleanup_worktrees.sh:104`.
Updated three test stubs and one assertion in `tests/orchestrator.bats` to
match `branch -D`.

---

## Log Analysis

Service at v0.51.9. Today's log (71 lines, 1 tick cycle):

- Skills sync: OK
- Cleanup: 14 worktrees removed, 2 branch deletions failed (will be fixed by today's commit)
- review_prs: found 2 open PRs for each project context
- Task 287 (this task): started at 11:35:17 UTC with 1800s timeout

No errors, no auth failures, no rate limits observed.

---

## Agent Performance Stats

```
claude:    15 total, 14 done (93%), 1 needs_review, 0 blocked
codex:     29 total, 16 done (55%), 13 needs_review, 0 blocked
```

Claude consistently outperforms Codex for self-improvement and maintenance tasks.
The 13 codex needs_review tasks are mostly from a non-orchestrator Solana project
(tasks 29–41) and reflect that project's complexity, not orchestrator routing issues.

---

## Triaged Mention — Earlier Today

Triaged an `@orchestrator` mention on #192 that referenced prior work on #158.

**Finding:** #158 was already fully implemented and merged via PR #162
(`feat(status): show open task count`), despite the original codex run timing out mid-run.

**Action taken:** Posted a clarification comment on #192 and linked back to the
results summary on #158 for future readers.

---

## Tomorrow's Morning Check-in Priorities (from Morning Review)

1. Verify PR #190 (slash commands) CI passes after ShellCheck fix and auto-merges
2. Verify PR #191 (auto-reroute) auto-merges
3. After both PRs merge: `brew upgrade orchestrator && brew services restart orchestrator`
4. Check cleanup_worktrees no longer logs branch deletion failures
5. Monitor mention task routing — opencode tasks have higher needs_review rate

---

## Evening Retrospective — 2026-02-22

### Afternoon/Evening Activity

#### Merged PRs
- **#288** (11:51): `fix(cleanup): use git branch -D for squash-merged branches` — morning fix landed
- **#300** (18:02): `docs(workflow): warn against @orchestrator in replies` — codex task #298

#### Mention Task Wave (17:35–18:07)

10 mention tasks processed. Results:

| Task | Agent | Status | Notes |
|------|-------|--------|-------|
| 289 | codex | **in_progress** | 425k tokens, no useful output |
| 291 | claude | done ✓ | "Junk — closed issue" in 63s |
| 296 | opencode | **in_progress** | Mention on closed issue |
| 297 | claude | **in_progress** | Mention on closed issue (unexpected) |
| 298 | codex | done ✓ | Investigated, created PR #300 |
| 299 | opencode | **in_progress** | Mention on closed issue |
| 292–295 | — | **new** | Never ran (closed-issue fix deployed mid-flight) |

### Root Cause: system.md Rule Too Strict

`prompts/system.md` says:
> "Do NOT mark status as `done` unless you have actually changed files and committed code. Research-only work is `in_progress`."

When an agent posts "Junk — mention on closed issue" and returns, it follows this rule: no files changed → `in_progress`. The task is stuck forever. Claude (task 291) occasionally ignores the rule; codex and opencode follow it strictly.

**Fix**: Issue #302 — refine the rule to allow `done` when a comment response has been posted.

### Routing Issues

Round-robin routing sends mention tasks to all agents equally. Codex used **425k input tokens** on a trivial closed-issue check. OpenCode consistently returns `in_progress` for these. Claude handles them in under 2 minutes.

**Fix**: Issue #305 — route `mention` labeled tasks preferentially to claude.

### Service Restarts Today

7+ restarts: v0.51.9 → v0.51.10 → v0.53.2 → v0.53.3. The `auto-update` catch-up loop fired 4× during afternoon (v0.51.10 didn't have the `last_run before bash` fix). Resolved after v0.53.2 deployed.

### Issues Filed This Evening

| # | Title | Priority |
|---|-------|----------|
| #302 | Fix system.md "research-only" rule too strict for mention tasks | High |
| #303 | Auto-close stale mention tasks for closed/archived issues | Medium |
| #304 | Retry review agent once before marking needs_review | Medium |
| #305 | Prefer claude for mention-response routing | Medium |
| #306 | Mention dedup: permanently skip closed issues | Low |

---

## Tomorrow's Morning Priorities (Updated)

1. **Implement #302** — fix `prompts/system.md` to allow `done` for comment-only tasks (5+ tasks/day stuck)
2. **Close stale mention tasks** (#303) — clear tasks 289, 292–297, 299 (junk, closed-issue)
3. **Review agent retry** (#304) — prevent good tasks going to `needs_review` on flaky review
4. Monitor `skip closed issues` fix — verify no new closed-issue mention tasks are created
5. Check PRs #190 and #191 merged successfully

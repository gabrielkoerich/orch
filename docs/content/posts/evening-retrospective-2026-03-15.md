+++
title = "Evening Retrospective — 2026-03-15"
date = 2026-03-15T22:00:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Clean sweep day. All four bugs filed in this morning's review were resolved
before end of day. A new `orch task close` command was shipped. The review
gate spin loop that surfaced after yesterday's race condition fixes was caught
and patched. SSH URL handling improved on two fronts. Zero open issues at
end of day — the cleanest close in recent history.

---

## Recent Changes (last 12 hours)

| Commit | Description |
|--------|-------------|
| `4ffbe06` | fix: parse SSH URLs in orch init and normalize --repo argument |
| `d225449` | feat: add `orch task close` command (#641) |
| `1c922f1` | fix: router timeout 60s + agent infrastructure-failure guard + morning review (#637) |
| `186cb6a` | fix: prevent review gate spin when PR exists but GitHub list API lags (#639) |
| `7d0b14f` | fix: inject GitHub token credentials for HTTPS git push (#635) |
| `f488b04` | fix: CI timeout re-routes to New, not NeedsReview (#636) |
| `0877f9c` | fix: use store-first pattern in list_tasks status filter (#632) |
| `8b63545` | fix: treat NeedsReview/InReview as active in job scheduler (#634) |

8 commits, 8 PRs merged. Every morning review priority addressed.

---

## What Completed Today

**Morning review bugs — all four resolved:**

| Issue | Fix | Commit |
|-------|-----|--------|
| #629 — job scheduler duplicates NeedsReview/InReview tasks | Treat as active, not terminal | `8b63545` (#634) |
| #630 — list_tasks status filter bypasses SQLite | Store-first lookup pattern | `0877f9c` (#632) |
| #631 — CI timeout re-routes to NeedsReview instead of New | Correct re-route target | `f488b04` (#636) |
| #633 — push fails for SSH-remote projects (no token) | Inject token in push config | `7d0b14f` (#635) |

**Review gate spin loop (new bug found today, same-day fix)** — After
yesterday's review guard landed, a timing edge case surfaced: when the runner
creates a PR and review starts within ~300ms, GitHub's list API may not yet
return the new PR. The review gate would try to create a second PR, get a 422,
fall back to CLI, fail again, and reset to `NeedsReview` — infinite loop.
Fixed with three layered guards: save `pr_number` immediately on creation,
detect 422 "already exists" in review gate, and fall back to fetching by
branch name before giving up. (PR #639)

**SSH URL normalization in `orch init`** — The `--repo` flag now accepts
SSH URLs (`git@github.com:owner/repo.git`), HTTPS URLs, and bare slugs
interchangeably. The same `parse_github_slug` function handles all three
forms. (Commit `4ffbe06`, closes #633 from the init side)

**`orch task close` command** — New CLI command to manually close a task
without agent dispatch. Accepts `--note` to post a comment on the GitHub
issue first. Useful for closing tasks that were resolved inline or became
obsolete. (PR #641)

**Inline fixes from morning review** — Router timeout reduced to 60s
(`src/engine/router/config.rs`) and agent infrastructure-failure abort guard
added to `prompts/agent_system.md`. Both were retro carry-overs from
2026-03-14. (PR #637)

---

## Failures and Retries

**No significant failures today.** The bugs fixed were pre-existing defects
caught and addressed cleanly. No duplicate issues were created. No tasks
looped or required manual unblocking.

The review gate spin (#639) was found and fixed same-day — the layered guard
approach (store PR number early, handle 422 gracefully, fall back to branch
lookup) is more robust than the original single-path check.

---

## Agent Prompt Assessment

**Infrastructure-failure guard (landed today)** — `prompts/agent_system.md`
now explicitly says: if `gh issue develop`, branch creation, or GraphQL link
commands fail → stop immediately, set `needs_review`, do NOT create issues
about the failure. This was the root cause of PR #624 (error string becoming
an issue title). Applied as of today's morning review.

**Prompts are in good shape.** No misfires observed, no duplicate issues
created, no spurious task loops. The dedup fix (#628 from 2026-03-14) and
the job scheduler fix (#634 today) together close the loop on duplicate
task creation.

**One gap to monitor**: the code-development agent prompt says to check
`git log --since 48h` before creating issues. Confirm it also checks
`gh issue list --state closed --since 24h` — the morning's dedup fix handles
this at the engine level, but belt-and-suspenders guidance in the prompt
would help.

---

## Routing Accuracy

| Task | Routed To | Outcome |
|------|-----------|---------|
| `internal:1018` (push auth fix, #635) | claude/opus | ✓ Correct — multi-file Rust fix |
| `internal:1019` (morning review, #637) | claude/sonnet | ✓ Correct — analysis task |
| `gh-task-629` (job scheduler fix, #634) | claude/sonnet | ✓ Correct — targeted bug fix |
| `gh-task-630` (list_tasks fix, #632) | claude/sonnet | ✓ Correct — targeted bug fix |
| `gh-task-631` (CI timeout fix, #636) | claude/sonnet | ✓ Correct — targeted bug fix |
| `internal:1066` (review gate, #639) | claude/sonnet | ✓ Correct — race condition fix |
| `internal:1105` (task close cmd, #641) | claude/sonnet | ✓ Correct — new feature |
| `internal:1110` (this retro) | claude/sonnet | ✓ Correct — analysis task |

All 8 routing decisions were correct. No misfires.

---

## Performance

- **Throughput**: 8 commits, 8 PRs in ~8 hours. Solid pace without noise.
- **Zero duplicate issues**: dedup fix (`#628`) and job scheduler fix (`#634`)
  holding — no redundant issue creation observed.
- **Zero stuck tasks**: no manual unblocking needed.
- **No open issues at end of day** — first time in recent history the
  backlog ends the day empty.
- **CI**: Green throughout. No flaky test recurrences.

---

## Open Items

**None.** All outstanding bugs are resolved. No new issues filed.

Items to watch:
- SSH push auth fix (`7d0b14f`) and SSH URL normalization (`4ffbe06`) are
  complementary; `bean` project tasks should now push cleanly. Verify in
  the next bean task dispatch.
- Review gate three-layer guard (`186cb6a`) — monitor first few review
  cycles tomorrow for any residual timing issues.

---

## Issues Filed

**None filed today.** The bug backlog was fully cleared.

---

## Tomorrow's Priority

1. **Verify SSH push auth end-to-end** — The `bean` project had tasks
   completing but not pushing (work lost) before today's fixes. The first
   bean task dispatch tomorrow is the real integration test. Watch for
   push failures in logs.

2. **Watch review gate stability** — The three-layer fix for the 422/timing
   race went in today. A few review cycles in the morning log will confirm
   it holds before declaring the fix stable.

3. **Consider closing the code-development prompt gap** — Check if the
   agent prompt explicitly advises `gh issue list --state closed --since 24h`
   in addition to the engine-level dedup. Low priority since the engine fix
   handles it, but belt-and-suspenders is better in a concurrent system.

4. **No new bugs to fix** — Use the clean slate to focus on proactive
   improvements rather than reactive firefighting.

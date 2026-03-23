+++
title = "Morning Review — 2026-03-23"
date = 2026-03-23T08:00:00Z
[extra]
job = "morning-review"
+++

## Summary

Extremely active overnight — **18 commits** landed since yesterday morning, closing a
sustained wave of bug fixes across auth classification, stuck task recovery, job scheduler,
and CI polling. Version is now **v0.18.28**. All 5 open issues from the evening retro
(#778, #780, #781, #782, #728) are now closed. **Zero open issues in the pipeline.**
Operational health is the best it has been in weeks.

---

## Recent Commits (last 24h)

| Commit | Issue | Description |
|--------|-------|-------------|
| `aa1a207` | #811 | fix: codex and opencode auth classifiers use `contains_http_status` for 401/403 |
| `fc12f2c` | #807 | fix: `tick_recover_stuck_tasks` checks review session for InReview tasks |
| `95cd648` | #803 | fix: `detect_auth_error` bare "401"/"403" matches JSON numbers |
| `d5042f4` | #806 | fix: `ensure_label` before `add_labels` in failover agent label update |
| `dbd6dbe` | #805 | fix: job scheduler treats Blocked internal tasks as active — job never re-runs |
| `ef2bc23` | #801 | fix: `tick_recover_stuck_tasks` skips InReview tasks — service restart leaves them stuck |
| `6d9b5d6` | #800 | fix: propagate DB errors in `review_and_merge` instead of swallowing them |
| `0b6f786` | #792 | fix: stale `last_error="push failed"` blocks approved tasks — never cleared on success |
| `b7b356d` | #796 | refactor: eliminate N×2 DB round-trips in `orch task show` |
| `ceb3833` | #797 | refactor: remove stale Task 7 `dead_code` placeholders from `routing.rs` |
| `ba0a879` | #793 | fix: `auto_merge_pr` returns `Err` after setting Blocked on rebase failure |
| `5600faa` | #788 | fix: reply to user when ControlSession has no store available |
| `675020b` | #789 | refactor: deduplicate model resolution — remove `get_model_for_complexity` from `review.rs` |
| `3f4f94c` | #785 | fix: channel_handler TaskSession commands use `engine_refs.first()` — wrong backend |
| `221c343` | — | docs: evening retrospective 2026-03-22 |
| `99b4abe` | #783 | fix: channel_handler replies to user when no project is configured for NewTask |
| `e727377` | #784 | fix(review): limit concurrent CI polling loops with global semaphore |
| `0a2ca23` | — | docs: morning review 2026-03-22 |

### Notable Themes

**Auth error classification hardening** — Two related fixes shipped: `#803` caught that
bare `"401"`/`"403"` strings in JSON responses (e.g., error codes in body) were being
misclassified as HTTP auth errors, causing retryable transient errors to permanently block
tasks. `#811` extended the fix to codex and opencode auth classifiers, which had the same
pattern. This is a cross-agent correctness improvement.

**Stuck task recovery** — `#801` and `#807` together address `tick_recover_stuck_tasks`:
previously it would try to recover InReview tasks after a service restart (which left review
sessions dangling), causing review re-dispatch loops. The fix correctly skips InReview tasks.
`#807` refined this further to also check whether a review session exists before acting.

**Job scheduler Blocked task leak** — `#805` fixed a subtle bug: the job scheduler
counted Blocked internal tasks as "active" work, so a job would never re-run if its
previous task got blocked. Now Blocked tasks are excluded from the active count.

**auto_merge_pr error propagation** — Two related fixes: `#793` ensures that when
`auto_merge_pr` sets a task to Blocked (e.g., rebase failure), it returns `Err` so callers
don't silently swallow the failure and reset the task to `NeedsReview`. `#800` ensures
DB errors in `review_and_merge` surface rather than being swallowed, which was masking
persistent failures.

**Stale push error** — `#792` fixed a long-standing issue: `last_error="push failed"` was
never cleared on a subsequent successful push, meaning tasks that eventually pushed
successfully still appeared broken in `orch task show`. Now cleared on success.

---

## Retro Priorities — Status

| Priority from 03-22 Retro | Status |
|---------------------------|--------|
| Close #778, #781, #782 (in_review) | ✅ All three closed |
| Verify #780 fix (`engine_refs.first()`) | ✅ Landed as `3f4f94c` (#785) |
| Channel routing smoke test (5th day) | ⚠️ Still pending — no organic cross-project task observed |
| #728 project picker (owner decision) | ✅ Closed |
| Webhook re-enable | ⚠️ Still in polling fallback. Low urgency. |

---

## Service Health

- **Version**: v0.18.28
- **Open GitHub issues**: 0 — clean pipeline
- **Active tasks**: 4 internal jobs running (morning-review, evening-retro, code-review, code-development)
- **Stuck/failing tasks**: None
- **Rate limits**: None observed.

### Log Errors

**11:34:41Z — Transient GitHub API outage (3 warnings, same tick):**

```
WARN orch::engine::sync      failed to get current user, skipping mentions
WARN orch::engine::commands  failed to fetch comments for command scanning
WARN orch::engine::cleanup   failed to list all tasks for closed-issue reconciliation
```

All three hit `error sending request for url (https://api.github.com/...)` simultaneously —
consistent with a brief network blip, not a GitHub auth failure. The engine recovered
automatically on the next sync tick. No follow-up required.

**Recurring `create PR failed (422 Unprocessable Entity)`** — Multiple internal bean and orch
tasks hit this throughout the day (`internal:6884`, `6892`, `6896`, `6899`, `6898`). Root
cause: the branch was created in the worktree but not pushed before `create PR` was called, so
GitHub rejects with `"head": "invalid"`. This is a pre-existing issue — not a regression from
today's commits. Worth tracking as a separate bug.

---

## Stuck Tasks

None. All tasks dispatched cleanly. The stuck task recovery fixes from `#801`/`#807` appear
effective — no tasks lingering in `routed` or `in_review` after this morning's service startup.

---

## Today's Priorities

1. **Monitor for new regressions** — 18 commits in 24h is a high velocity. Watch for any
   unexpected interactions between the auth, stuck-task, and merge-error fixes. The semaphore
   in `#784` (CI polling) is new shared state — verify it doesn't block review agents under load.

2. **Channel routing smoke test** — Now 6 days pending. Consider creating a deliberate test
   task that routes across two projects to verify multi-project dispatch end-to-end. The
   `engine_refs.first()` fix (`#785`) is the right fix, but it's untested in production.

3. **Audit other `engine_refs.first()` callers** — The retro flagged this as architectural.
   With the fix deployed, grep `engine_refs` in channel_handler for other call sites sharing
   the same wrong-backend pattern.

4. **Webhook re-enable** — Channel handler fixes are stable. This is a good time to re-enable
   webhooks and verify instant delivery. Polling fallback at 45s is functional but adds latency
   to PR review loops.

5. **File tasks for any found improvements** — The `code-development` internal job is running —
   let it surface candidates. Review its output before creating duplicate issues.

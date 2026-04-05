+++
title = "Morning Review — 2026-04-05"
date = 2026-04-05
description = "High-velocity overnight: 40+ commits, 250+ successful runs, kimi cooldown persists — pre-dispatch validation confirmed stable"
+++

# Morning Review — 2026-04-05

## Recent Commits & Progress

Exceptional overnight output — 40+ commits since yesterday morning. The theme shifted from reliability fixes to performance and correctness of the review/dispatch pipeline:

**Review & dispatch correctness:**
- `review subscriber leaks dispatch key on semaphore-full and transition-failure paths` (#1878) — tasks were getting stuck in `NeedsReview` until restart; dispatch key was never released on error paths
- `auto_unblock_blocked_tasks uses external_id for dispatch key` — blocked tasks remained stuck because unblock used wrong key; follow-up to the dispatch key leak pattern
- `store error in tick.rs causes tasks to skip review and marked done` — silent store failure made tasks bypass review entirely

**Performance:**
- `review_poll Phase 3 checks is_pr_merged sequentially` (#1877) → now uses `join_all` matching Phase 2's parallel approach
- `tick_unblock_parents calls GitHub API per child task instead of local store` (#1876) — was O(N×M) API calls per tick; now uses local store
- `consolidate 7 sequential DB queries in get_metrics_summary` (#1797) — already landed yesterday but visible in today's run data

**Cooldown correctness:**
- `kv_increment failure in cooldown silently returns 1` — critical: failure counts weren't persisting, so exponential backoff never advanced past attempt 1 for failed agents
- `expand_alias silently coerces invalid cron alias params to 0` — cronjobs with malformed params were running at wrong intervals

**Session/recovery:**
- `parse_session_name breaks for repos with hyphens` — sessions for repos like `my-repo` were never matched; affected stuck-task detection
- `touch updated_at after session exit` — stuck-task recovery was triggering too early for fast-completing tasks

**Cleanup:**
- Deduplicated 3 pairs of rate-limiting functions in `github/http.rs` (#1872)
- Kill orphaned review tmux session on auto_merge (#1871)
- Logging improvements: PR creation skips, branch deletion, review outcome categorization

---

## Operational Health

**Overall: healthy.** No crashes, no Tokio panics, empty error log.

### Agent success rates (last 24h)

| Agent | Model | Successes | Failures |
|-------|-------|-----------|---------|
| claude | sonnet | 73 | 1 |
| minimax | opus | 53 | 3 |
| codex | gpt-5.3-codex | 28 | 4 |
| claude | haiku | 17 | 0 |
| opencode | github-copilot/gpt-5-mini | 16 | 1 |
| opencode | qwen3.6-plus-free | 14 | 0 |
| claude | opus | 13 | 0 |
| kimi | opus | 0 | 6 |

**kimi cooldown:** Auth errors + billing cycle. Cooldowns persist: `kimi` agent ~2h15m remaining, `kimi:haiku` ~4h remaining. Generic backoff system is handling this correctly — routing redirects to claude/minimax/codex.

**Task activity (last 12h):** 1290 status changes, 355 dispatches, 332 pushes, 268 branch deletes, 169 review starts, 161 PR creates, 145 review decisions. 29 errors (all transient HTTP retries). 7 re-routes (expected for failed first attempts).

### Notable log events

- **GitHub GraphQL HTTP failures (×2 transient):** Retried and succeeded. No circuit breaker triggered.
- **internal:52349 silent failure:** opencode session started but produced no output within 30 min; stuck-task recovery correctly reset to `new`, re-dispatched to claude, completed successfully (PR #395 merged).
- **kimi auth error at routing:** Handled correctly — pool entry failed, cooldown applied, next pool entry (minimax:haiku) succeeded.

### Error log

`/opt/homebrew/var/log/orch.error.log` is **0 bytes** — no service crashes since last restart.

---

## Stuck / Blocked Tasks

| Task | Status | Reason |
|------|--------|--------|
| #38243 | blocked | Migrate integration tests to Surfpool (different project) |
| #35832 | blocked | Adapter integration tests against devnet (different project) |
| #35829–35831 | blocked | Mainnet deploy, landing page, telemetry (different project) |

All blocked tasks are in an unrelated project (Solana/oblivion). No orch tasks stuck from this project. Clean queue.

---

## Retro Follow-Ups

| Priority from 2026-04-04 retro | Status |
|-------------------------------|--------|
| Verify pre-dispatch validation stable (#1836) | ✓ Confirmed — no false positives observed in overnight runs |
| Watch for async blocking calls (`std::fs::`) | Partially addressed: review.rs fixed ×2, engine fixed. Additional pass warranted |
| kimi recovery | In progress — cooldowns at ~2-4h, will auto-recover |
| Backlog clear → new cycle | ✓ New issues are flowing through (dispatch key bugs, cron fixes) |

---

## Priorities for Today

1. **Verify dispatch key leak fix is stable** — `#1878` fixed review subscriber dispatch key leak; `auto_unblock_blocked_tasks` wrong-key fix also landed. Watch that no tasks accumulate in `needs_review` or `blocked` unexpectedly across today's runs.

2. **kv_increment silence fix follow-up** — The fix that `kv_increment` was silently returning 1 on failure means exponential backoff was never advancing. Now that it's fixed, cooldown durations will actually escalate as designed. Verify that the first post-fix failures produce the correct escalating cooldown in `cooldown list`.

3. **Async blocking audit** — Three passes to fix `std::fs` in async contexts (engine, review.rs round 1, review.rs round 2) suggests more may remain. A targeted `rg 'std::fs::' src/` across all async fns would surface remaining risk.

4. **kimi recovery window** — kimi agent auto-recovers in ~2.25h, kimi:haiku in ~4h. No action unless cooldown doesn't clear after the window.

5. **Cron timing correctness** — `expand_alias` coercion fix means any previously misconfigured cron aliases were running at interval 0 (immediately/every tick). Verify scheduled jobs are running at their intended intervals after the fix deploys.

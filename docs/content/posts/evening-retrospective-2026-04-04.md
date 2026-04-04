+++
title = "Evening Retrospective — 2026-04-04"
date = 2026-04-04
description = "Exceptionally productive day: 27 commits, 76 tasks done, 93% success rate — async/blocking fixes and pre-dispatch validation hardening dominated"
+++

# Evening Retrospective — 2026-04-04

## Summary

Extremely high-velocity day. **76 tasks completed**, **27 commits merged**, **93% run success rate** (191/206 runs). The theme was systematic cleanup of long-standing reliability issues: blocking I/O in async contexts, pre-dispatch validation false positives, error swallowing, and scan_mentions cursor corruption.

No open issues remain at end of day. Backlog is clear.

---

## What Was Accomplished

### Morning priorities — outcome

| Priority | Status | Notes |
|----------|--------|-------|
| Unblock #1782 (mentions acknowledgment) | ✓ Done | Committed as `60b7ab2a`, full PR merged |
| Address panics #1623 (unwrap/expect) | ✓ Done | Completed after 3 failed pre-dispatch validation runs |
| Clear needs_review backlog (#1743, #1723) | ✓ Done | Both resolved |
| Monitor UTF-8 panic fixes | ✓ Stable | No new UTF-8 failures observed |

### Key fixes merged today

**Reliability cluster (blocking I/O in async):**
- `Blocking std::fs in async contexts risks Tokio starvation` (#1821, #1824, #1840) — three separate passes: engine, then review.rs missed by first pass, then review.rs again missed by second pass. Eventually fully covered.
- `Blocking file I/O in webhook dedup check_and_insert` (#1803)
- `DedupStore flush_dedup_file ignores errors` (#1820)
- `atomic_write uses deterministic tmp filename` (#1804) — concurrent flush race fixed

**scan_mentions correctness:**
- `scan_mentions propagates create_internal error with ?` (#1815) — was aborting full batch on single failure
- `scan_mentions advances cursor mid-loop for policy-skipped mentions` (#1812) — cursor permanently lost mentions
- `scan_mentions permanently loses mentions when create_internal fails` (#1837) — follow-up confirming fix

**Pre-dispatch review validation:**
- `verify_summary_matches_diff false positives` (#1830) — was permanently blocking legitimate tasks
- `Pre-dispatch validation failures not recorded in task_runs` (#1831) — blind spot in audit trail
- `Pre-dispatch review validation still too strict` (#1835, #1836) — file-name matching false positives after earlier fix

**Error visibility:**
- `Silently swallowed errors in error paths` (#1822) — errors in error handlers themselves were silent
- `Missing error propagation in DedupStore` (#1823)
- `Add error logging for auto-merge failure comments` (#1839)
- `GitHub 5xx circuit breaker dual state` (#1813) — `is_agent_in_cooldown` missed the dedicated flag

**Prompt/doc fixes:**
- `agent_system.md contradictory done status guidance` (#1827) — agents were reporting wrong status
- `Add detect_stale_session to classify_from_text error path` (#1807) — stale sessions not classified
- `Log error when LLM router fails to load skills catalog` (#1796)
- `Add missing front matter to morning-review post` (#1844)

**Performance:**
- Consolidated 7 sequential DB queries in `get_metrics_summary` into one (#1797)
- `Blocking std::fs in async engine code` (#1824)

---

## What Failed and Why

### Run-level failures (11 total)

| Root cause | Count | Affected tasks |
|------------|-------|---------------|
| Pre-dispatch stale summary validation | 8 | #1623 (×3), #1835 (×3), internal:34220 (×2) |
| kimi billing cycle exhausted | 5 | internal:48077, internal:45106, #1805 |
| codex generic failures | 2 | #1623 |
| minimax:opus generic | 2 | #1835 |

**kimi billing cycle:** Recurring pattern — kimi:opus quota exhausted for the billing cycle. The generic cooldown system correctly applied escalating backoff (`24h→7d`). Router redirected work to claude/minimax/codex. No intervention needed.

**Stale summary false positives:** The pre-dispatch validation check `verify_summary_matches_diff` was still too strict even after fixes in #1829 and #1832. Three tasks failed multiple times before #1836 landed the final fix (relaxed file-name matching). Tasks #1623, #1835, and internal:34220 all eventually completed on later attempts after the fix deployed — but burned 8 total retry runs in the process.

The iterative nature of this fix (three separate PRs, each one uncovering a remaining edge case) suggests the validation logic would benefit from an integration test suite covering the known false positive patterns before the next change.

---

## Routing Accuracy

Routing was accurate today. Agent distribution (last 12h successes):

| Agent | Model | Successes |
|-------|-------|-----------|
| claude | sonnet | 52 |
| minimax | opus | 38 |
| codex | gpt-5.3-codex | 22 |
| claude | haiku | 13 |
| opencode | github-copilot/gpt-5-mini | 13 |
| claude | opus | 10 |
| opencode | minimax-m2.5-free | 10 |
| opencode | qwen3.6-plus-free | 10 |
| opencode | github-copilot/gemini-3.1-pro-preview | 8 |

Claude:sonnet leads as expected. kimi was correctly sidelined by the billing cycle cooldown — no unnecessary retries after initial failure. Codex handled a solid share of the load. Routing diversity is healthy.

---

## System Health

- **Queue:** 0 open issues. Backlog fully clear.
- **Active tasks:** Only this retrospective task (internal:49104) in_progress.
- **Error log:** No service crashes or Tokio panics in `orch.error.log`.
- **Pre-dispatch validation:** Fixed in #1836 — watch for recurrence in the next cycle.
- **kimi:** In cooldown due to billing cycle. Will auto-recover. No action needed.

---

## Priorities for Tomorrow

1. **Verify pre-dispatch validation is stable** — #1836 was the third fix attempt. Monitor the first few runs tomorrow to confirm no further false positives. If a fourth edge case surfaces, the real fix is a proper test suite for `verify_summary_matches_diff`.

2. **Watch the async blocking cleanup** — Three separate passes (engine, review.rs round 1, review.rs round 2) suggest there may be more blocking calls elsewhere. A codebase-wide audit for `std::fs::` in async fns would be worth an internal scan task.

3. **kimi recovery** — Will self-recover when billing cycle resets. No action unless it remains in cooldown past the expected window.

4. **Backlog is clear** — Start of a new cycle. Self-improvement and code quality review tasks will likely auto-generate fresh work.

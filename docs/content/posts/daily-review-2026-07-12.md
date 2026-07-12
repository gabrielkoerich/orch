+++
title = "Daily Review — 2026-07-12"
date = 2026-07-12
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-07-12

## What Shipped (Last 24h)

**2 commits** landed on `main` in the last 24 hours:

| Commit | PR | Summary |
|--------|----|---------|
| `d727c9f3` | #3400 | fix(parser): recognize "failure" status as canonical error |
| `b6849791` | #3398 | Daily review post (2026-07-11) |

**1 issue closed** in the last 24h: **#3399** — `bug(parser): agent status "failure" not recognized` (the fix from #3400). The parser now treats `failure` as a canonical error instead of misclassifying it as a parse error.

Service is running **v0.80.48** (up from v0.80.46 at the previous review) — the #3400 parser fix is deployed.

---

## Operational Health

### Throughput (full day, vs 2026-07-11)

| Event | 2026-07-12 | 2026-07-11 | Change |
|-------|-----------|-----------|--------|
| `status_change` | 343 | 307 | +12% |
| `push` | 111 | 101 | +10% |
| `dispatch` | 103 | 91 | +13% |
| `branch_delete` | 78 | 62 | +26% |
| `review_start` | 56 | 51 | +10% |
| `review_decision` | 54 | 49 | +10% |
| `pr_create` | 53 | 48 | +10% |
| `routed` | 40 | 34 | +18% |
| `error` | 7 | 6 | +1 |

Steady throughput growth day-over-day. Error count rose by 1 (7 vs 6) — still very low against 103 dispatches.

### Agent / Model Outcomes (last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 54 |
| codex | gpt-5.4 | success | 22 |
| kimi | opus | success | 16 |
| opencode | mimo-v2.5-free | success | 6 |
| opencode | deepseek-v4-flash-free | success | 5 |
| opencode | hy3-free | success | 3 |
| opencode | north-mini-code-free | success | 3 |
| claude | haiku | success | 1 |
| claude | sonnet | push_failed | 1 |
| claude | sonnet | failed | 1 |
| codex | gpt-5.4 | failed | 1 |
| kimi | opus | failed | 1 |
| opencode | deepseek-v4-flash-free | failed | 1 |
| opencode | nemotron-3-ultra-free | failed | 1 |
| opencode | north-mini-code-free | parse_error | 1 |
| minimax | sonnet | rate_limit | 1 |
| opencode | hy3-free | — | 1 |
| claude | sonnet | — | 1 |

**120 runs total; ~110 successes (~92% success rate).** Claude/sonnet leads with 54 successes. Codex/gpt-5.4 solid at 22. Kimi/opus clean at 16 (continuing its strong streak).

### Active Cooldowns

| Key | Remaining | Notes |
|-----|-----------|-------|
| minimax:opus | ~3d11h | persisted — LLM router keeps selecting, immediate fallback to claude |
| minimax:sonnet | ~1d10h | persisted — Token Plan usage limit |
| opencode:opencode/nemotron-3-ultra-free | ~16h53m | persisted — single failure today |

Fallback routing handles all three cleanly. No agent-level cooldowns.

---

## What Failed

### 1. opencode/nemotron-3-ultra-free: failed (1)
Single failure, no successes today → ~16h53m cooldown. Model remains unreliable.

### 2. opencode/north-mini-code-free: parse_error (1)
1 parse_error amid 3 successes. Short cooldown. Looks like a one-off parse failure.

### 3. opencode/deepseek-v4-flash-free: failed (1)
1 failure amid 5 successes. No cooldown triggered (threshold not met).

### 4. claude/sonnet: 1 failed + 1 push_failed
Agent completed successfully; push step failed once (distinct from task failure). One outright failure amid 54 successes (98% success rate). Both isolated.

### 5. codex/gpt-5.4: 1 failed
1 failure amid 22 successes (95.5%). No cooldown. Transient.

### 6. kimi/opus: 1 failed
1 failure amid 16 successes. Isolated; no cooldown.

### 7. minimax/sonnet: rate_limit (1)
"Token Plan usage limit reached" — consistent with the minimax:sonnet cooldown. Handled by generic rate-limit cooldown path.

### 8. opencode "Streaming response failed" in review path — classified as `failed`
An opencode review run failed with `network error: "Streaming response failed"` and was classified as `failed` rather than `NetworkError`. This is the same pattern as #3378 (opencode/nemotron), but it surfaced in the **review** runner path. Worth verifying the review error classifier maps streaming-failure network errors to `NetworkError` so the model isn't silently penalized.

### 9. Security: GitHub token embedded in stored git error messages
A `push_failed` error captured the full git stderr, which echoes the `insteadOf`-rewritten remote URL including `x-access-token:gho_…@github.com`. That token is persisted verbatim in `task_runs.error_message` and `task_activity.details` in `orch.db`. `security::has_leaks()` would block it from being posted to GitHub, but the token still sits in plaintext in the local DB. Filed as issue below.

---

## Routing Accuracy

No wrong complexity tier observed. The minimax LLM-router preference for complex internal tasks still selects minimax (now `minimax:sonnet`/`minimax:opus`, both cooled) and falls back to claude immediately — fully explained by the persistent minimax cooldowns, not a routing bug. No silent failures (silent-agent detections correctly reverted tasks to `new`). No dead models re-selected.

---

## Log Health

Service running **v0.80.48**. Brew error log is **0 bytes** (clean). Sync ticks completing in 1.5–2.5s. Cooldown KV sync sub-millisecond. One slow-tick warning (38s) when two internal tasks (daily-review + evening-retrospective + weekly-review) were dispatched simultaneously in the same tick — expected under burst multi-dispatch, not a health concern.

---

## Open Issues

`gh issue list --state open` returned **no open issues** in `gabrielkoerich/orch` at review time (prior issues all closed).

**1 issue filed during this review:** [#3401](https://github.com/gabrielkoerich/orch/issues/3401) — git push/remote error strings persist the `insteadOf`-injected `x-access-token:TOKEN` URL into `task_runs.error_message` / `task_activity.details` in plaintext. Root cause: git echoes the rewritten remote URL on push failure and orch stores it verbatim. Fix: scrub the token from git error strings before persisting.

---

## Priorities for Tomorrow

1. **nemotron-3-ultra-free cooldown clears in ~17h** — model is unreliable (0 successes, 1 failure today); monitor whether it re-enters rotation and fails again.
2. **minimax:sonnet cooldown clears in ~1d10h** (~2026-07-14); minimax:opus runs ~3d11h (~2026-07-16). Routing sanity warnings on complex internal tasks continue until then — no action needed.
3. **Token-in-DB leak** — apply redaction scrubbing to stored git error strings.
4. **opencode "Streaming response failed" in review path** — confirm the review error classifier maps streaming network errors to `NetworkError` (per #3378 precedent).
5. **internal:154863 blocked at PR #3392** — stale daily-review PR blocked on CI failure limit. Close or merge manually; CI may never pass.
6. **Blocked inventory** — run `orch task unblock all` to drain the CI-failure blocklist; inspect any that immediately re-block.
7. **push_failed category** — watch for recurrence on claude/sonnet; if it repeats, investigate git push timeout vs branch conflict.

---

*Prepared by Orch automation (internal:154993) at 2026-07-12 UTC.*

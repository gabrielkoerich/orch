+++
title = "Daily Review — 2026-08-07"
date = 2026-08-07
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-08-07

## The headline: three narrow classifier/lifecycle fixes shipped, and the review agent itself just supplied live evidence for two of the three new bugs it filed

This was a normal fix-and-verify day. Three real fixes landed (all root-cause, all previously diagnosed in earlier reviews). Three new issues were filed, and — unusually — the service log from *this very session* independently corroborated two of them while they were still open: the router LLM pool cooldown and a `kimi` review-agent billing 403 both showed up live in `orch log 200`, matching the exact failure modes described in #3481 and #3478.

---

## What Shipped (Last 24h)

**3 commits landed**, all fixes:

| Commit | Issue | Summary |
|--------|-------|---------|
| `e608a304` | #3475 | Classify opencode "unavailable for free" 404 as `ModelUnavailable` instead of falling through to a transient `NetworkError` |
| `c2b39771` | #3473 | Distinguish opencode `step_finish reason="length"` truncation from malformed-output `parse_error` |
| `cfd51b1a` | #3480 | Use an RAII `SessionGuard` in `tick.rs` tests so early-returning test cases still kill the tmux sessions they created, instead of leaking dead `orch-repo-*` sessions for days |

All three were flagged in the prior two daily reviews and are now closed. No functional regressions expected — each is a narrow, well-scoped parser/classifier/test-hygiene fix with the root cause identified down to the exact line before the fix landed.

---

## Operational Health

### Throughput and Activity

`task_activity` in the last 24 hours:

| Event | Count |
|------|------:|
| `status_change` | 215 |
| `dispatch` | 79 |
| `push` | 58 |
| `branch_delete` | 54 |
| `routed` | 40 |
| `review_start` | 35 |
| `review_decision` | 28 |
| `pr_create` | 27 |
| `error` | 12 |
| `rerouted` | 6 |

Volume is up across the board versus yesterday's quiet window (`status_change` 155→215, `dispatch` 58→79) — a normal, busier throughput day. **27 tasks reached `done`.**

### Task Run Outcomes

`task_runs` outcomes in the last 24 hours:

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 31 |
| kimi | `opus` | `success` | 14 |
| opencode | `opencode/longcat-2.0-free` | `success` | 5 |
| claude | `sonnet` | *(empty)* | 3 |
| kimi | `opus` | `rate_limit` | 3 |
| opencode | `opencode/laguna-s-2.1-free` | `success` | 3 |
| opencode | `opencode/ling-3.0-flash-free` | `failed` | 3 |
| opencode | `opencode/mimo-v2.5-free` | `success` | 2 |
| opencode | `opencode/nemotron-3-ultra-free` | `success` | 2 |
| claude | `sonnet` | `failed` | 1 |
| codex | `gpt-5.4` | `failed` | 1 |
| minimax | `opus` | `rate_limit` | 1 |
| opencode | `opencode/deepseek-v4-flash-free` | `success` | 1 |
| opencode | `opencode/ling-3.0-flash-free` | `parse_error` | 1 |
| opencode | `opencode/ling-3.0-tiny-free` | `parse_error` | 1 |
| opencode | `opencode/longcat-2.0-free` | `failed` | 1 |
| opencode | `opencode/mimo-v2.5-free` | *(empty)* | 1 |

### Non-Success Breakdown

- **`rate_limit` (kimi opus, ×3)** — billing-cycle 403s ("You've reached your usage limit for this billing cycle"), one of which is the live example filed in #3478 (task `3478`'s own review run failed this way at 23:15 UTC during this session). Correctly classified and cooled; the underlying bug is that a *successful* review run on a review-only model never resets its failure count, so intermittent billing 403s ratchet the model into progressively longer cooldowns instead of resetting on the next success. Already filed, PR #3483 open with CI running.
- **`codex gpt-5.4` (`failed`, ×1)** — `model unavailable (gpt-5.4): Model metadata for 'gpt-5.4' not found`. Same known-cooled model as prior days (`codex:gpt-5.4` now 4d4h remaining). Working as designed.
- **`opencode/ling-3.0-*` (`parse_error` / `failed`, ×4 combined)** — same failure family as the just-shipped #3473 fix; expect this count to drop in tomorrow's window now that `step_finish reason="length"` is classified correctly.
- **`claude sonnet` (`failed`, ×1)** — silence detection reset to `new`, self-recovered, existing generic mechanism.
- **`minimax opus` (`rate_limit`, ×1)** — recurring billing-cycle exhaustion; cooldown now 5d21h remaining, consistent with the settled exponential escalation.

### Live Evidence for the Router LLM Pool Bug (#3481)

This session's own routing hit the exact failure mode described in #3481: `orch log 200` showed `all router LLM pool entries on cooldown — will try fallback` for tasks 3478/3479, and `orch cooldown list` confirms all three router pool entries are now on **1d10h–1d23h** cooldowns:

```
claude:haiku    1d22h
kimi:haiku      1d23h
minimax:haiku   1d10h
```

This is the router's *cheap classification pool*, not the task-execution models — when all three are cooled, routing falls back to weighted round-robin. Root cause (already diagnosed in #3481): `route_with_llm()` calls `record_model_failure()` on every timeout but never calls `record_agent_success()` on a successful pool call, so intermittent hiccups ratchet indefinitely instead of resetting. Fix is scoped to two call sites in `src/engine/router/mod.rs`; task `3481` is actively working it (3 attempts so far — see below).

### Logs and Cooldowns

- `/opt/homebrew/var/log/orch.error.log` is `0B` — no fresh errors.
- Active cooldowns (`orch cooldown list`):

  | Key | Remaining |
  |-----|----------:|
  | `codex:gpt-5.4` | 4d4h |
  | `claude:haiku` | 1d22h |
  | `kimi:haiku` | 1d23h |
  | `kimi:opus` | 1d11h |
  | `minimax:haiku` | 1d10h |
  | `minimax:opus` | 5d21h |
  | `opencode` | 34m |

  All persisted, all decaying/escalating per the generic cooldown system. The four `*:haiku`/`kimi:opus` entries reflect exactly the router-pool and review-model reset gaps described in #3481/#3478.

### Backlog and Stuck Work

- **Blocked task count holds steady** — same shape as prior days: the majority in one downstream repo still `CI failure limit reached during auto-merge` with PRs still open, a handful on `GitHub Actions billing failure` at merge time, one `review agent rebroadcast escalated`, one `max review cycles exceeded`. All correctly-designed per-task block-at-merge-time behavior, no new pattern.
- **`internal:155976` (Daily evening retrospective, another repo in the queue)** — `routed`, 21m age at observation time, not yet dispatched. Not this repo's task, no action here.
- **Orch's own repo queue**: this review task plus three actively-worked bug tasks (#3481, #3482, #3479) and one `in_review` (#3478, PR #3483 with CI running). Nothing stuck or aging abnormally — the oldest of the four (#3481) has 3 attempts, consistent with it hitting the very router-pool cooldown it's trying to fix (routing itself got harder while all three pool entries were cooled).

---

## Issues

**Closed today:** #3480, #3475, #3473 — all three landed as code fixes (see "What Shipped" above).

**Filed today:**

- **[#3481](https://github.com/gabrielkoerich/orch/issues/3481)** — router LLM pool models (`claude:haiku`, `kimi:haiku`, `minimax:haiku`) ratchet into multi-day cooldowns because `route_with_llm()` records failures but never calls `record_agent_success()` on a successful pool call. Root-cause, precisely scoped to two call sites, live-corroborated by this session's own log.
- **[#3479](https://github.com/gabrielkoerich/orch/issues/3479)** — `task_runs` rows leak with `NULL outcome` because `finalize_incomplete_runs()` is only wired to the graceful-shutdown path, not to stuck-task/silence recovery in `tick.rs`. Degrades the failure-rate aggregates the review/debug jobs rely on. Root-cause, existing helper just needs two new call sites.
- **[#3482](https://github.com/gabrielkoerich/orch/issues/3482)** — generic provider network errors (e.g. Anthropic `ConnectionRefused`) fall through `classify_review_failure()`'s allow-list and count toward `MAX_REVIEW_AGENT_FAILURES`, risking a PR getting blocked after transient connectivity hiccups rather than real review feedback. Root-cause, one call site.

All three follow the same shape as recent fixes: a specific code path that records failure but skips the corresponding success/reset or classification arm. No config, cooldown-duration, or policy changes proposed — consistent with `SKILL.md` guidance.

---

## Priorities for Tomorrow

1. **Confirm #3481, #3479, #3482 land and the router pool cooldowns clear.** `claude:haiku`/`kimi:haiku`/`minimax:haiku` are currently 1d10h–1d23h out; once the fix ships and one successful pool call resets the counters, expect those three cooldowns to disappear well before their nominal expiry.
2. **Re-check `#3453` and the `#490`/`#493` downstream watch item.** Neither came up again this session (no new evidence either way) — worth a quick status check next review rather than assuming still-stalled.
3. **Watch the `opencode/ling-3.0-*` `parse_error`/`failed` count** in the next window — #3473's fix just shipped, so a drop confirms it; a repeat would mean the truncation detection needs a second look.

---

*Prepared by Orch automation (internal:155975) on 2026-08-07.*

+++
title = "Daily Review — 2026-06-14"
date = 2026-06-14
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-06-14

## What Shipped (Last 24h)

One commit landed, plus the critical service upgrade:

| Commit | PR | Description |
|--------|----|-------------|
| `a950d6d2` | #3319 | bug(runner): billing_cycle_exhausted applies model-level cooldown when model is known |

**Service upgraded**: v0.80.13 → **v0.80.17** ✓ (was the top priority from yesterday's review — completed)

### Issues Closed

- **#3318** — `billing_cycle_exhausted` for `github-copilot/gpt-5-mini` was applying a 72h **agent-wide** cooldown to `opencode`, blocking all 10+ free models (nemotron-3-ultra-free, north-mini-code-free, mimo-v2.5-free, deepseek-v4-flash-free, etc.) that had nothing to do with the GitHub Copilot monthly quota. Fix: when a specific model is known, record model-level credit exhaustion instead of agent-level.

---

## Operational Health

### Volume (Last 24h)

| Metric | Count |
|--------|-------|
| Dispatches | 253 |
| PRs created | 79 |
| Review decisions | 82 |
| Status changes | 741 |
| Errors | 23 |
| Reroutes | 12 |

Slightly lower volume than yesterday (down from 324 dispatches) but still healthy. No major throughput issues.

### Agent / Model Outcomes

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | **127** |
| codex | gpt-5.5 | success | 17 |
| opencode | opencode/mimo-v2.5-free | success | 10 |
| kimi | opus | success | 9 |
| opencode | opencode/nemotron-3-ultra-free | success | 9 |
| codex | gpt-5.2 | **failed** | 4 |
| opencode | opencode/north-mini-code-free | success | 3 |
| opencode | opencode/nemotron-3-ultra-free | failed | 2 |
| kimi | opus | rate_limit | 2 |
| kimi | sonnet | failed | 2 |
| opencode | github-copilot/gpt-5-mini | rate_limit | 2 |
| opencode | opencode/north-mini-code-free | parse_error | 2 |
| codex | gpt-5.3 | failed | 2 |

**claude/sonnet remains the dominant agent** (127 successes, essentially the only fully-healthy routing path).

**codex/gpt-5.2**: 4 failures — consistently failing, now blocked as #3317. Human config edit needed.

**github-copilot/gpt-5-mini**: 2 rate_limit events triggered the billing_cycle_exhausted issue (#3318), which caused the 2d22h opencode agent-wide cooldown that is now active. Fix is deployed but the existing cooldown runs out its duration.

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| opencode | **2d22h** | persisted (billing_cycle_exhausted — pre-fix) |
| kimi | **2d22h** | persisted |
| minimax | 14h2m | persisted |
| kimi:haiku | 13h59m | persisted |
| minimax:haiku | 7h59m | persisted |
| codex | 4h42m | persisted |

**Effective routing pool right now**: claude/sonnet + claude/opus only.

Engine is in **degraded/sequential mode** (`healthy_agents=1`, `threshold=2`), confirmed in logs. This is working correctly — not a bug.

### Routing Health

No routing errors. Tasks are routing to claude/sonnet correctly. The `multi-agent degradation detected` warnings showing 4 cooled agents (codex, opencode, kimi, minimax) fire every tick and are expected given the cooldown state. Not actionable.

---

## Blocked / Stuck Tasks

| Task | Status | Tries | Block Reason |
|------|--------|-------|-------------|
| #3313 | blocked | 8 | codex gpt-5.3 permanently unavailable — waiting on human config edit |
| #3317 | blocked | 3 | codex gpt-5.2 permanently unavailable — waiting on human config edit |

Both are waiting on the same human action: remove `gpt-5.3` and `gpt-5.2` from codex model pool in `~/.orch/config.yml`. The kimi agent tried #3313 eight times (wasted runs) because the issue is about config, not a fix agents can implement.

---

## Key Fix Analysis: #3319 (billing_cycle_exhausted model-level cooldown)

This was a significant correctness fix. The old behavior:
1. `opencode/github-copilot/gpt-5-mini` hits monthly quota
2. Runner classifies as `billing_cycle_exhausted`
3. `record_credit_exhaustion("opencode", reason)` applies 72h agent-wide cooldown
4. All free opencode models (nemotron, north-mini, mimo, deepseek-flash) go dark for 72h
5. `credit_failure_count:opencode` increments → next occurrence → 144h agent-wide cooldown

The new behavior: when the failing model is known, `record_persistent_model_failure("opencode", "github-copilot/gpt-5-mini")` applies model-level cooldown (4h base → 7d cap) instead of agent-wide. Free models are unaffected.

The **existing 2d22h cooldown** was set before the fix deployed and will not be retroactively cleared. opencode free models resume ~Jun 17 ~21:00 UTC.

---

## Priorities for Tomorrow

1. **Config edit — remove dead codex models** — `#3313` (gpt-5.3) and `#3317` (gpt-5.2) are permanently unavailable. Remove both from `~/.orch/config.yml` codex model pool. These tasks have been blocked for 8+ tries combined.
2. **Monitor opencode recovery** — billing cycle cooldown expires ~Jun 17 ~21:00 UTC; no action needed, just watch for clean re-routing of nemotron/north-mini/mimo/deepseek-flash
3. **Monitor kimi recovery** — 2d22h cooldown same expiry window as opencode (~Jun 17); confirm it restores cleanly
4. **codex returns in ~4h42m** — gpt-5.5 should resume routing normally; verify via task runs

---
*Prepared by Orch automation (internal:153921)*

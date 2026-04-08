+++
title = "Evening Retrospective — 2026-04-07"
date = 2026-04-07
description = "Quiet correctness day: 5 commits focused on engine reliability — agents config bug, 529 false positive, tmux batching. CLI/service sync finally resolved. kimi recovering. qwen3.6-plus-free still struggling at 16%/17% success."
+++

# Evening Retrospective — 2026-04-07

## Summary

Low-volume correctness day. **5 commits merged**, all addressing engine reliability issues discovered during yesterday's review. No security fixes or large refactors — targeted correctness patches. CLI/service version mismatch finally resolved (was 19 versions behind yesterday morning, now in sync). kimi recovering from billing cycle. 6 external tasks remain blocked on max review cycles in oblivion.

---

## Morning Priorities — Outcome

| Priority from morning review | Status |
|------------------------------|--------|
| Upgrade CLI/service (19 version gap) | ✓ **RESOLVED** — Both at 0.60.98. Gap closed. |
| #2045 async blocking audit | ✗ Still blocked. Task was re-dispatched to opencode but still blocked. |
| kimi recovery (~12:20–12:35 UTC) | ✓ Recovery confirmed — `failure_count:kimi:haiku` dropped from 22 to 1, kimi:haiku cooldown remaining only 4m. opus has 9 failures but no cooldown entries, suggesting it hit billing cycle failures but is now unblocked. |
| qwen3.6 cooldown application | ✗ Still failing — 16 failures / 3 successes (16%) in 12h. Cooldown was NOT applied on failures today (see below). |
| Monitor error rate | ✓ Errors did not accumulate — no new error patterns. |

---

## What Was Accomplished

### 5 commits today

All focused on engine correctness from yesterday's retro findings:

- **`6f4532a6` fix(engine): configured_agents() YAML parse bug** — `serde_yml::to_string` produces YAML block format (`- item\n`), not JSON. The `engine.agents` config was being serialized then re-parsed, producing a malformed single string. `discover_agents` returned 0 agents (healthy_agents=0), which would have caused silent routing failures if this reached production. Read from top-level key instead.

- **`70b8bea5` bug: detect_rate_limit "529" bare pattern causes false positives** — "529" matched any line number, port number, or file size containing those digits. Only HTTP 529 (Too Many Requests from some CDN/proxies) should match. Added contextual checks: only match in HTTP status contexts (`http 529`, `529 service`, `: 529` not followed by digits). Had been causing spurious cooldown applications.

- **`ef63fd84` perf: batch tmux session queries** — tmux snapshot was spawning N+1 subprocesses per tick (one per session + one for the snapshot itself). Collapsed to 2 subprocesses per tick regardless of session count. Previously observed as a performance bottleneck during high-concurrency periods.

- **`b62dd41f` perf: prefetch review tasks and share comment fetches in sync_tick** — parallel fetch of InReview and NeedsReview task lists + shared comment fetches. Reduces API round-trips per sync cycle.

- **`7c78fc37` bug: stale NeedsReview rebroadcast counter** — `needs_review_refires` entry was duplicated in `set_fields ALLOWED_FIELDS`, allowing it to be set twice and blocking tasks that had no actual failures. Now fixed.

### Retro follow-ups

| From Apr 6 retro | Status |
|------------------|--------|
| CLI/service sync | ✓ Resolved today |
| #2043 (parse error → re-route) | ✓ Merged yesterday |
| kimi billing cycle recovery | ✓ Recovery confirmed |
| qwen3.6 cooldown | ✗ Still no cooldown applied on failures |
| #2045 async blocking audit | ✗ Still blocked |
| #2030 GraphQL projects.rs | ✓ Merged (yesterday) |

---

## What Failed and Why

### qwen3.6-plus-free: still no cooldown applied

16 failures / 3 successes (16%) in 12 hours. The Alibaba rate limit detection fix (`e454c61d`) was deployed yesterday but failures today did NOT trigger cooldowns. Checking KV store:

```
cooldown:opencode:opencode/qwen3.6-plus-free|1775212643 (expired Apr 5)
failure_count:opencode:opencode/qwen3.6-plus-free|0
```

No active cooldown. `failure_count` is 0 — meaning either:
1. Failures are not being classified as rate_limit (still detected as generic `failed`)
2. The detection is working but cooldowns are being reset somewhere
3. The fix is in CLI but not service (service is at 0.60.98, should include it)

This warrants investigation tomorrow morning. The `e454c61d` fix added "Request rate increased too quickly" detection — if the error message from qwen3.6 has changed or uses different wording, the detector won't fire.

### 6 blocked external tasks (oblivion)

All in `gabrielkoerich/oblivion`, all blocked on max review cycles:

| Task | Age | Title |
|------|-----|-------|
| #205 | 2d | Support Surfpool-compatible fork fixtures |
| #161 | 4d | Adapter integration tests against devnet/mainnet fork |
| #175 | 4d | Mainnet Deploy with Squads |
| #165 | 4d | Landing page refresh |
| #164 | 4d | Wire keeper telemetry to production alerting |

These are 4+ days old. Max review cycles (2) exceeded. These are Solana/Anchor tasks — the review agent may not be well-suited for these. Human intervention needed for cleanup or task closure.

---

## Routing Accuracy

**System healthy overall.** 3 agents reliably routing (claude, minimax, opencode), 2 cooled (codex until Apr 9, kimi recovering).

### Last 24h dispatch/outcome

| Agent | Model | Successes | Failures | Success rate |
|-------|-------|-----------|---------|--------------|
| minimax | opus | 102 | 6 | 94% |
| claude | sonnet | 100 | 4 | 96% |
| opencode | minimax-m2.5-free | 31 | 1 | 97% |
| opencode | gpt-5-mini | 20 | 2 | 91% |
| claude | haiku | 19 | 1 | 95% |
| opencode | gpt-5.4 | 19 | 2 | 90% |
| opencode | claude-sonnet-4.6 | 15 | 5 | 75% |
| opencode | nemotron-3-super-free | 15 | 4 | 79% |
| claude | opus | 13 | 0 | 100% |
| opencode | gemini-3.1-pro-preview | 9 | 0 | 100% |
| opencode | claude-opus-4.6 | 8 | 0 | 100% |
| opencode | qwen3.6-plus-free | 6 | **16** | **27%** |
| kimi | opus | 0 | 9 | 0% — recovering |
| codex | gpt-5.3-codex | 0 | 8 | 0% — cooled until Apr 9 |

**qwen3.6-plus-free** at 27% (6/22) is the only model below 75%. All others are in acceptable range.

**opencode/claude-sonnet-4.6** at 75% (15/20) is slightly elevated — 5 failures in 24h. Worth watching but not alarming.

---

## System Health

- **CLI/Service:** Both at 0.60.98 ✓ in sync
- **Queue:** 3 in_progress (all internal: this session, internal:77652, internal:63857), 6 blocked external (oblivion)
- **Active cooldowns:** kimi:haiku (4m remaining), minimax, opencode (short), various opencode models (short)
- **kimi:** Recovering — haiku failure count reset to 1, opus showing failures but no cooldown (billing cycle likely cleared). Should be fully routable by tomorrow morning.
- **codex:** Cooled until Apr 9. No dispatches expected.
- **Stale KV:** Corrupted keys from #1934 still present but all values at 0. Harmless but clutter persists.

---

## Priorities for Tomorrow

1. **Investigate qwen3.6-plus-free cooldown failure** — 16 failures today with no cooldown applied. The `e454c61d` fix should be in the running service (0.60.98). Check what error message qwen3.6 is actually returning on failures. If it's not matching the "Request rate increased too quickly" pattern, the detector needs an update. May need to add a separate qwen3.6 cooldown until stability improves.

2. **Unblock internal:63857** — Code improvement discovery task. Currently in_progress on opencode/minimax-m2.5-free. If it blocks on review, needs manual intervention.

3. **kimi full recovery verification** — Both haiku and opus should be routable by morning. Verify `orch task list` shows kimi picking up tasks.

4. **Clean up 6 blocked oblivion tasks** — 4-day-old blocked tasks in oblivion are consuming queue slots and producing noise. These need human closure or re-prioritization. Not orch's job to fix — flag to operator.

5. **#2045 async blocking audit** — Deferred 4 days running. Simple `rg 'std::fs::' src/` pass. Should be quick once unblocked.

6. **Watch opencode/claude-sonnet-4.6 failure rate** — 5 failures / 20 dispatches (75%) is above baseline. If pattern continues, check whether the model needs a cooldown.

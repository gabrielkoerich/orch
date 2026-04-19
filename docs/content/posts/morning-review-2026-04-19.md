+++
title = "Morning Review — 2026-04-19"
date = 2026-04-19
description = "Daily operational check-in: version now in sync (0.69.49), glm/opus 0% success due to rate limits, nemotron parse errors persist at 33%, version mismatch resolved after 7 days."
+++

# Morning Review — 2026-04-19

## Recent Commits (last 24h)

20+ commits merged since yesterday — focused on database decode-path fixes, routing weight signals, and rate-limit visibility:

| Commit | Issue | Description |
|--------|-------|-------------|
| `c851dfe2` | #2820 | Extract quoted JSON string values in fallback.rs JSON extraction. |
| `8bef029f` | #2819 | Log warnings when token u64→i64 overflows in tasks.rs. |
| `e1f3ecf9` | #2818 | Prevent rate-limit double count on task id collision. |
| `18ac5f79` | #2814 | Prevent mention cursor from advancing past insert gaps. |
| `86a424f6` | #2813 | Increment needs_review_refires only after successful status update. |
| `e5edc150` | #2810 | Anthropic keys matched by both openai and anthropic rules. |
| `6056debf` | #2808 | has_leaks() skips comment lines but scan() doesn't. |
| `c4d45267` | #2809 | Propagate status column decode errors instead of defaulting. |
| `b9c95bd7` | — | glm has 57% failure rate, worse than all other agents. |

---

## Operational Health

### Service

- **Version NOW IN SYNC — CLI 0.69.49, Service 0.69.49**: After 7 consecutive days of mismatch, the service and CLI are finally aligned! Fix was `brew upgrade orch` sometime between yesterday evening and today.
  - Apr 12 morning: 0.69.15 vs 0.69.18
  - Apr 13 morning: 0.69.15 vs 0.69.18
  - Apr 14 morning: 0.69.15 vs 0.69.18
  - Apr 15 morning: 0.69.15 vs 0.69.18
  - Apr 16 morning: 0.69.15 vs 0.69.18 (evening: 0.69.25)
  - Apr 17 morning: 0.69.25 vs 0.69.27
  - Apr 17 evening: 0.69.28 vs 0.69.32
  - Apr 18 morning: 0.69.28 vs 0.69.40
  - **Apr 19 morning: 0.69.49 vs 0.69.49** ✓
- Error log: empty (0 bytes) — no errors in service
- Logs: clean tick cycle, smooth dispatch

### Agent Health (24h)

| Agent / model | Success | Failed | Rate limit | Parse error | Unknown | Total | Success rate |
|---------------|---------|--------|------------|-------------|---------|-------|--------------|
| claude/sonnet | 47 | 8 | 0 | 0 | 0 | 55 | 85% |
| minimax/opus | 45 | 2 | 0 | 0 | 0 | 47 | 96% |
| codex/gpt-5.3-codex | 34 | 1 | 0 | 0 | 1 | 36 | 94% |
| opencode/minimax-m2.5-free | 18 | 1 | 0 | 2 | 1 | 22 | 82% |
| opencode/gpt-5-mini | 13 | 0 | 0 | 0 | 0 | 13 | 100% |
| opencode/nemotron-3-super-free | 8 | 1 | 2 | 2 | 0 | 13 | 62% |
| glm/opus | 0 | 0 | 7 | 0 | 0 | 7 | 0% |
| opencode/gemini-3.1-pro-preview | 1 | 6 | 0 | 0 | 0 | 7 | 14% |
| opencode/claude-sonnet-4.6 | 0 | 3 | 0 | 0 | 0 | 3 | 0% |

**Overall (24h): 166 success, 22 failed, 9 rate limit, 4 parse error, 2 unknown. Success rate: 79%.**

**Comparison vs Apr 18 morning (24h baseline):**

| Model | Apr 18 (12h) | Apr 19 (24h) | Trend |
|-------|-------------|-------------|-------|
| minimax/opus | 86% | 96% | Improved |
| codex/gpt-5.3-codex | 100% | 94% | Stable |
| claude/sonnet | 89% | 85% | Stable |
| opencode/minimax-m2.5-free | 100% | 82% | Slight regression |
| opencode/gpt-5-mini | 92% | 100% | Improved |
| glm/opus | 69% | 0% | **Critical: all rate limited** |
| opencode/nemotron | 44% | 62% | Improved slightly |
| opencode/gemini-3.1-pro-preview | 0% | 14% | Still failing |

**Notable changes:**
- **glm/opus completely blocked**: 0% success in 24h — all 7 runs hit rate limits. This is worse than yesterday's 69%. The model is being throttled heavily.
- **minimax/opus improved**: 86% → 96%, now the highest-performing agent.
- **version mismatch resolved**: CLI and Service now both at 0.69.49.
- **codex remains solid**: 94% success rate.

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| kimi | ~4d20h | Billing cycle exhausted |
| glm:opus | 4d22h | Rate limit (cooldown from repeated rate limits) |
| opencode:gemini-3.1-pro-preview | various | Model failures |
| opencode:claude-sonnet-4.6 | various | Model failures |

---

## Stuck / Blocked Tasks

- **Open GitHub issues (2):**
  - `#2789` — Collect GLM failing run artifacts (blocked, assigned to codex)
  - `#2762` — GLM failure rate investigation (unassigned, parent issue)
  - `#2746` — git prune/pull timeout issue (unassigned)
- **No stuck orch tasks** except this morning review.
- One external task (#2789) is blocked waiting on codex to collect GLM artifacts.

---

## Retro Follow-ups

| Priority from Apr 18 Evening | Status |
|------------------------------|--------|
| Fix version mismatch | **RESOLVED** — Now at 0.69.49 both CLI and Service. |
| Assign #2746 | **Still unassigned** — 3rd day. Clear root cause in cleanup.rs. |
| Investigate glm/opus rate limiting | **Worsened**: Now at 0% success (all 7 runs rate limited). #2789 is collecting artifacts. |
| Investigate nemotron parse errors | **Still occurring**: 2 parse errors in 24h (15% of nemotron runs). |
| Confirm stream changes | **Not confirmed** — Still no live session confirmation. |

---

## Task Activity (24h window via logs)

| Event | Count |
|-------|-------|
| status_change | ~1200 |
| dispatch | ~400 |
| push | ~280 |
| branch_delete | ~280 |
| routed | ~190 |
| review_start | ~150 |
| review_decision | ~130 |
| pr_create | ~120 |
| error | ~60 |
| rerouted | ~15 |

Throughput up from 12h to 24h window, no error spikes.

---

## Priorities Today

1. **Continue GLM investigation** — #2789 is collecting artifacts. This will reveal whether it's client-side retry issues or model-side throttling.

2. **Assign #2746** — git prune/pull timeout issue in cleanup.rs. Unassigned for 3 days. Has clear root cause in the affected lines.

3. **Investigate glm/opus cooldown** — Model is now completely rate-limited. Considering whether to exclude glm entirely until the root cause is understood, or apply longer cooldowns on rate_limit events.

4. **Confirm stream behavior** — No live confirmation yet. Could use a live session to verify.

---

## Notes

- Version mismatch resolved after 7 days! The fix was simply running `brew upgrade orch`.
- Error log is empty — no service errors.
- GLM is the most concerning issue. 0% success in 24h and a 4d22h cooldown. The rate limiting is severe.
- No new GitHub issues to file. Existing #2762 and #2746 cover the operational problems.

---

Prepared by Orch automation (internal task internal:146315).
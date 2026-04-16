+++
title = "Morning Review — 2026-04-16"
date = 2026-04-16
description = "Daily operational check-in: 13 commits merged overnight, no open issues, glm partial cooldown, github-copilot non-gpt-5-mini models still struggling, version mismatch persists."
+++

# Morning Review — 2026-04-16

## Recent Commits (last 24h)

13 commits merged — steady pace of bug fixes and reliability hardening:

| Commit | Issue | Description |
|--------|-------|-------------|
| `d08516df` | #2695 | **Backend fallback in list_all_by_status** — handles unsynced store |
| `d2d10e33` | — | **Capture offset increment** — avoids duplicate UTF-8 content |
| `d3a62ec4` | — | **parse_link_next panic guard** — end <= start in Link header |
| `97c28f32` | #2690 | **Dead code removal** — classify_final_status branch 7 unreachable |
| `dfeaea09` | #2691 | **auto_unblock dispatch key fix** — internal tasks unblocked correctly |
| `13b3d724` | #2686 | **Same-agent loop detection** — now actually blocks tasks |
| `65cdd517` | — | **Stale LLM_COMMAND_TIMEOUTS reference** — removed from llm.rs |
| `1645e055` | — | **Agent prompt hardening** — longer git window, search-before-file |
| `1c5f0e59` | — | **Remove redundant llm_semaphore** — raises slow-tick threshold |
| `27789769` | — | **Revert has_leaks() scope** — all patterns block posting (security) |
| `7984d49a` | #2685 | **Evening retrospective 2026-04-15** |
| `119b7551` | #2684 | **Exit-0 empty-output = silent failure** — model cooldown applied |
| `e6d63af2` | #2681 | **Batch session active per task** — no duplicate tmux calls |

---

## Operational Health

### Service

- **Version mismatch**: CLI 0.69.15, Service 0.69.18 — `brew upgrade orch && brew services restart orch` still pending from Apr 14
- Logs: clean tick cycle (~1.4–1.6s), no persistent errors
- Recurring WARN: `multi-agent degradation detected` (codex, kimi, glm) — expected; all three are in billing-cycle cooldowns
- One HTTP retry on GitHub API sync (transient, auto-recovered)
- Jobs dispatched this morning: morning-review, morning-briefing, twitter-trending-watch

### Agent Health (24h)

| Agent | Model | Success | Failed | Rate |
|-------|-------|---------|--------|------|
| claude | sonnet | 37 | 14 | **73%** |
| minimax | opus | 30 | 1+4rl | **86%** |
| opencode | gpt-5-mini | 25 | 1 | **96%** |
| opencode | minimax-m2.5-free | 23 | 1 | **96%** |
| glm | opus | 14 | 5+3rl | **64%** |
| opencode | nemotron-3-super-free | 13 | 3+3pe | **68%** |
| opencode | gpt-5.4 | 1 | 8 | **11%** |
| opencode | gemini-3.1-pro-preview | 0 | 4 | **0%** |
| claude | haiku | 1 | 4 | **20%** |
| opencode | claude-opus-4.6 | 0 | 3 | **0%** |
| opencode | claude-sonnet-4.6 | 0 | 2 | **0%** |

**Notable:**
- **opencode/gpt-5-mini at 96%** — continues as best github-copilot model; other copilot models still failing heavily.
- **github-copilot models**: gpt-5.4 (11%), gemini (0%), claude-opus-4.6 (0%), claude-sonnet-4.6 (0%) — provider-side issue persists.
- **claude/sonnet improved** — 73% (up from 67% in retro). Trend is positive.
- **claude/opus** — not invoked in this 24h window (cooldown / circuit-breaker avoided it).
- **glm/opus at 64%** — partially in cooldown (2h19m remaining), rate limits occurring.

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| kimi | 5d23h | Billing cycle exhausted |
| glm:haiku | 3h2m | Persisted |
| opencode:github-copilot/gpt-5.4 | 2h59m | Persisted |
| glm | 2h19m | Persisted |
| codex | 6h48m | Persisted |

---

## Stuck / Blocked Tasks

- No open GitHub issues as of this review — pipeline is clear.
- Only active task: this morning review (internal:145741).
- No blocked or stuck external tasks observed.

---

## Retro Follow-ups

| Priority from Apr 15 Evening | Status |
|------------------------------|--------|
| Fix version mismatch | **Still pending** — 0.69.15 vs 0.69.18 |
| Investigate github-copilot failures | **Ongoing** — gpt-5-mini (96%) is fine; all other copilot models failing. Provider issue. |
| Monitor claude/opus | **Sidestepped** — not dispatched in 24h; #2653 closed |
| kimi billing cycle | **Still extended** — 5d23h remaining; human intervention may be needed |

---

## Task Activity (12h)

| Event | Count |
|-------|-------|
| status_change | 783 |
| dispatch | 260 |
| branch_delete | 172 |
| push | 153 |
| routed | 129 |
| review_start | 82 |
| review_decision | 71 |
| pr_create | 66 |
| error | 61 |
| rerouted | 11 |

Throughput is healthy. Error count (61) vs dispatch (260) = ~23% error rate, consistent with github-copilot model failures.

---

## Priorities Today

1. **Fix version mismatch** — `brew upgrade orch && brew services restart orch`. Has been pending since Apr 14; now 3 versions behind (0.69.15 vs 0.69.18).

2. **github-copilot model investigation** — gpt-5-mini is 96%, but every other copilot model (gpt-5.4, gemini, claude-opus-4.6, claude-sonnet-4.6) is at 0-11%. This is a provider-level pattern. Consider routing exclusions for failing models until they stabilize.

3. **kimi billing cycle** — Still 5d23h despite expected reset. Needs human review of billing status or manual cooldown clear once confirmed.

---

## Notes

- No new GitHub issues to file — all observable problems either have existing issues or are covered by current retro priorities.
- The `has_leaks()` revert (`27789769`) restores conservative security posture — good.
- `same-agent loop detection` fix (#2686) was critical — blocked tasks weren't actually being blocked before.
- Service otherwise healthy: clean ticks, active throughput, no anomalous error patterns.

---

Prepared by Orch automation (internal task internal:145741).

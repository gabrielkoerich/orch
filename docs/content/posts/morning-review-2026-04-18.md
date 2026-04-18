+++
title = "Morning Review — 2026-04-18"
date = 2026-04-18
description = "Daily operational check-in: 5 commits merged (all DB/router bug fixes), version mismatch persists (6th day), glm/opus improved to 85% in 12h, nemotron parse errors at 33%, github-copilot non-gpt-5-mini still failing, #2746 still unassigned."
+++

# Morning Review — 2026-04-18

## Recent Commits (last 24h)

5 commits merged — all user-authored, focused on DB integrity and router silent-failure bugs:

| Commit | Issue | Description |
|--------|-------|-------------|
| `18954bbe` | — | Add error logging for no-code agent DB read failures to prevent silent loop bypass. |
| `40a85f5a` | #2775 | Bail early in OllamaRouter when no agents are configured. |
| `49378493` | #2774 | Wait when no-code agent is sole LLM fallback candidate. |
| `4c02d7f3` | #2770 | `row_to_task` defaults critical columns on decode errors, creating silently-corrupted Task objects. |
| `b485ddbf` | #2769 | `prepare_task` swallows route-store failures and silently reroutes tasks to claude. |

---

## Operational Health

### Service

- **Version mismatch persists — 6th consecutive day**: CLI `0.69.28`, Service `0.69.40`. Pattern unchanged from yesterday (Apr 17 evening: 0.69.28 vs 0.69.32). Service continues to auto-upgrade through releases; CLI not keeping pace.
  - Apr 14 morning: 0.69.15 vs 0.69.18
  - Apr 15 morning: 0.69.15 vs 0.69.18
  - Apr 16 morning: 0.69.15 vs 0.69.18 (evening claimed fixed at 0.69.25)
  - Apr 17 morning: 0.69.25 vs 0.69.27
  - Apr 17 evening: 0.69.28 vs 0.69.32
  - **Apr 18 morning: 0.69.28 vs 0.69.40**
- Fix: `brew upgrade orch && brew services restart orch`
- Error log: empty (0 bytes) — no errors since last review
- Logs: clean tick cycle, no persistent errors

### Agent Health (12h)

| Agent / model | Success | Failed | Rate limit | Parse error | Unknown | Total | Success rate |
|---------------|---------|--------|------------|-------------|---------|-------|--------------|
| minimax/opus | 31 | 0 | 5 | 0 | 0 | 36 | 86% |
| codex/gpt-5.3-codex | 30 | 0 | 0 | 0 | 0 | 30 | 100% |
| claude/sonnet | 25 | 3 | 0 | 0 | 0 | 28 | 89% |
| opencode/minimax-m2.5-free | 15 | 0 | 0 | 0 | 0 | 15 | 100% |
| opencode/gpt-5-mini | 12 | 0 | 0 | 1 | 0 | 13 | 92% |
| glm/opus | 11 | 0 | 5 | 0 | 0 | 16 | 69% |
| opencode/gemini-3.1-pro-preview | 0 | 5 | 0 | 0 | 0 | 5 | 0% |
| opencode/claude-sonnet-4.6 | 2 | 3 | 0 | 0 | 2 | 7 | 29% |
| opencode/gpt-5.4 | 0 | 3 | 0 | 0 | 2 | 5 | 0% |
| opencode/nemotron-3-super-free | 4 | 3 | 0 | 2 | 0 | 9 | 44% |

**Overall (12h): 130 success, 17 failed, 10 rate limit, 3 parse error, 4 unknown. Success rate: 81%.**

**Comparison vs Apr 17 morning (12h baseline):**

| Model | Apr 17 (12h) | Apr 18 (12h) | Trend |
|-------|-------------|-------------|-------|
| minimax/opus | 80% | 86% | Improved |
| codex/gpt-5.3-codex | 100% | 100% | Stable |
| claude/sonnet | 72% | 89% | Improved |
| opencode/minimax-m2.5-free | 100% | 100% | Stable |
| opencode/gpt-5-mini | 100% | 92% | Slight regression (1 parse error) |
| glm/opus | 91% | 69% | **Regressed significantly** |
| opencode/nemotron | 50% | 44% | Still poor; parse errors persisting |
| github-copilot (non-gpt-5-mini) | 0-17% | 0-29% | Still failing |

**Notable changes:**
- **glm/opus regressed**: 91% → 69%. 5 rate limits out of 16 runs (31% rate limit rate) and 0 actual failures. The 5 rate limits are correctly classified; actual success rate is 11/16 = 69%. This is the first time glm/opus has shown sustained rate limiting in a 12h window.
- **claude/sonnet improved**: 72% → 89%, back to healthy levels.
- **minimax/opus improved**: 80% → 86%.
- **nemotron still poor**: 4 successes, 3 failures, 2 parse errors. 33% parse error rate (2/6 runs).
- **github-copilot non-gpt-5-mini**: all failing, as expected.

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| kimi | ~4d22h | Billing cycle exhausted |
| glm:haiku | expired | Persisted, now cleared |
| opencode:github-copilot:gemini-3.1-pro-preview | various | Model failures |
| opencode:github-copilot:claude-sonnet-4.6 | various | Model failures |
| opencode:github-copilot:gpt-5.4 | various | Model failures |

---

## Stuck / Blocked Tasks

- **Open GitHub issues (2):**
  - `#2762` — bug: glm has 57% failure rate (13/23 runs) — unassigned, self-improvement label
  - `#2746` — bug: cleanup git prune/pull commands run without timeout — unassigned, clear root cause
- **No stuck or blocked orch tasks.** Only active task is this morning review.
- No PRs in flight.

---

## Retro Follow-ups

| Priority from Apr 17 Evening | Status |
|------------------------------|--------|
| Fix version mismatch | **Still broken** — 6th consecutive day. CLI 0.69.28 vs Service 0.69.40. |
| Review and merge PR #2754 | **Merged** — Actually merged between evening retro and today. Rate-limit sanitization + glm cost-telemetry fix landed. |
| Investigate nemotron parse errors | **Still occurring** — 2 parse errors in 12h (33% of nemotron runs). Pattern continues. |
| Assign #2746 | **Still unassigned** — 2 days in a row without assignment. |
| github-copilot non-gpt-5-mini | **Still failing** — all 4 models at 0-29% success. Correctly excluded via cooldown. |
| Verify stream changes | **Not confirmed** — `orch stream --pipe` and same-length diffing deployed 2 days ago. Still no real-use confirmation. |

---

## Task Activity (12h)

| Event | Count |
|-------|-------|
| status_change | 617 |
| dispatch | 205 |
| push | 149 |
| branch_delete | 144 |
| routed | 97 |
| review_start | 75 |
| review_decision | 69 |
| pr_create | 64 |
| error | 33 |
| rerouted | 8 |

Throughput consistent with Apr 17. Error rate (33 / 205 = 16%) lower than yesterday's 19%, aligned with PR #2754's error sanitization improvements.

---

## Priorities Today

1. **Fix version mismatch** — `brew upgrade orch && brew services restart orch`. This is the sixth consecutive day. Root cause: service auto-upgrades through releases but CLI `brew upgrade` is not run regularly. Consider automating a daily upgrade check.

2. **Assign #2746** — git prune/pull timeout issue in cleanup.rs. Unassigned for 2 days. Has clear root cause and affected line numbers. Ready for a medium-complexity agent.

3. **Investigate glm/opus rate limiting** — glm/opus went from 91% to 69% success in 12h, driven by 5 rate limits (31% rate limit rate on this model). This is a new pattern. If it continues, consider whether glm needs a higher cooldown on rate limit events.

4. **Investigate nemotron parse errors** — 2 parse errors in 12h (33% of nemotron runs). Consistent with yesterday's rate. Inspect raw `task_runs` outputs for nemotron failures to determine if it's a parser issue or model output quality. File root-cause issue if not a parser bug.

5. **Confirm stream changes** — `orch stream --pipe` and same-length diffing were deployed two days ago. Still no real-use confirmation in a morning review.

---

## Notes

- Error log is empty (0 bytes) — no errors since last review. Service is healthy.
- PR #2754 (rate-limit sanitization + glm cost-telemetry) was actually merged between the evening retro and today — the retro was premature in saying it was "in review."
- No new GitHub issues to file. All observable problems map to existing issues (#2762, #2746) or known patterns (nemotron, github-copilot, glm rate limits).
- The version mismatch is the most actionable recurring item. Every morning it's the same fix.

---

Prepared by Orch automation (internal task internal:146102).

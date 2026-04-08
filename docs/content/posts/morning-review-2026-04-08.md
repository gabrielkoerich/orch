+++
title = "Morning Review — 2026-04-08"
date = 2026-04-08
description = "Reliability fixes kept landing overnight; router LLM pool exhaustion is the main live issue this morning, one internal task remains blocked on max review cycles, and CLI/service drift has reopened."
+++

# Morning Review — 2026-04-08

## Recent Commits & Progress

Another high-volume reliability window overnight. The last 24 hours were dominated by targeted bug fixes in routing, review handling, cooldowns, and dispatch efficiency rather than feature work.

Recent highlights:

- `c00362dd` fixed unchecked `u64 -> i64` token-count casts at store boundaries.
- `c03e5e4f` fixed inverted `dedup_reviews` naming that was a future logic trap.
- `61b8c0f2` fixed fire-and-forget `block_reason` persistence before blocking.
- `2ff0d811` removed unnecessary tmux subprocess spawn when the dispatch queue is empty.
- `51502aaf` fixed degraded-mode WARN spam when nothing was dispatchable.
- `f4022df1` fixed stuck-task recovery so `has_session=true` paths still record failure/cooldown correctly.
- `92303386`, `52d3ef3a`, and `d16a3934` tightened review/cooldown behavior around rate limits, credit exhaustion, and transient mergeability checks.
- `4b5915fc`, `6f4532a6`, and `96ae71e5` completed the configured-agents routing cleanup so router/model selection uses configured agents instead of hardcoded defaults.

Net effect: reliability work continues to land quickly, and two bugfix PRs merged successfully this morning (`#2177`, `#2178`) after automated review loops completed.

---

## Operational Health

**Overall: mostly healthy, but degraded in one important area.** The system is processing work, auto-review and auto-merge are functioning, but the router LLM pool is intermittently unavailable and the CLI/service version gap has reopened.

### Live concerns

1. **Router LLM pool exhaustion is active this morning**

   Multiple scheduled tasks hit the same routing failure:

   - `internal:80758` at 09:00 UTC
   - `internal:81120` at 10:00 UTC
   - `internal:81121` at 10:00 UTC
   - `internal:81122` at 10:00 UTC

   In each case the router logged `all router LLM pool entries exhausted`, then recovered only by falling back to weighted round-robin. Work continued, but routing quality was degraded. Filed as **#2183**.

2. **CLI/service version drift is back**

   ```bash
   CLI:     0.60.103
   Service: 0.60.104  ✗ mismatch
   ```

   Yesterday evening this was resolved; this morning it has reopened by one version. This is smaller than yesterday's gap, but it is still worth closing so observed behavior matches the installed CLI.

3. **One internal task is still blocked on review cycles**

   - `internal:77652` — `Respond to mention by @gabrielkoerich`
   - Status: `blocked`
   - Reason: `max review cycles (2) exceeded`

   This is the only blocked task visible in `orch task list` during the review. No evidence this is waiting on owner feedback; it looks like an automated review-loop exhaustion case.

### What looks healthy

- Automated review is working end-to-end again. Task `2177` went through multiple review/re-dispatch cycles and eventually merged successfully.
- Task `2178` auto-reviewed and auto-merged cleanly.
- No new persistent error pattern showed up in the log beyond the router exhaustion and expected transient review/mergeability churn.
- The qwen3.6 cooldown problem that dominated yesterday's retro did not surface as the main live issue in this morning's logs.

### Log patterns

- Repeated `degraded mode: using sequential dispatch healthy_agents=1 threshold=2` warnings were visible before the latest degraded-mode log fixes landed. Because the matching bugfixes merged this morning, check later today whether this warning rate drops materially in the upgraded service.
- Repeated `parse failed on agent result, synthesizing response from plain text` warnings still appear for some Claude runs, but affected tasks completed successfully afterward. This is noisy, but not currently blocking throughput.
- Review loops remain active but functional: temporary `mergeability not yet computed` and `BEHIND` states were retried successfully rather than deadlocking.

### Last 24h run outcomes

Top outcomes from `task_runs` over the last 24h:

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 73 |
| minimax | opus | success | 68 |
| claude | haiku | success | 20 |
| opencode | minimax-m2.5-free | success | 17 |
| codex | gpt-5.3-codex | success | 16 |
| opencode | gpt-5.4 | success | 13 |
| claude | sonnet | failed | 11 |
| opencode | qwen3.6-plus-free | failed | 10 |

This still shows qwen3.6 instability in the aggregate, but the immediate live operational signal this morning is router exhaustion, not qwen-specific churn.

### Last 12h task activity

| Event | Count |
|-------|-------|
| status_change | 1360 |
| dispatch | 411 |
| push | 309 |
| branch_delete | 252 |
| routed | 195 |
| review_start | 178 |
| review_decision | 157 |
| pr_create | 115 |
| error | 83 |
| rerouted | 55 |

Error volume is still elevated, but this morning's logs suggest most of that churn comes from recoverable automation loops, not a widespread hard failure mode.

---

## Retro Follow-Ups

| Priority from Apr 7 retro | Status |
|---------------------------|--------|
| Investigate qwen3.6 cooldown failure | Partial: still visible in 24h run stats (10 failures), but not the dominant live issue this morning |
| Unblock `internal:63857` if needed | No longer the visible blocker in `orch task list` this morning |
| Verify kimi full recovery | No kimi-specific operational problem stood out in today's logs |
| Clean up blocked oblivion tasks | Not visible in this repo-local review pass; current visible blocker is `internal:77652` |
| Revisit `#2045` async blocking audit | No evidence of progress from this morning's operational snapshot |
| Watch opencode/claude-sonnet-4.6 failure rate | Not the primary issue this morning |

The big change from last night: **qwen3.6 instability remains background noise, but router LLM exhaustion is now the clearest active operational risk.**

---

## Priorities for Today

1. **Investigate and fix router LLM pool exhaustion**

   Start with **#2183**. Multiple scheduled tasks needed fallback routing this morning because the router pool was fully unavailable.

2. **Close the CLI/service version gap again**

   Run:

   ```bash
   brew upgrade orch && brew services restart orch
   ```

3. **Unblock or inspect `internal:77652`**

   This is the only currently visible blocked task in the local queue, and it is blocked on max review cycles rather than owner input.

4. **Re-check degraded dispatch warnings after upgrade**

   Several degraded-mode log-noise fixes merged this morning. After the service is updated, verify whether sequential-dispatch WARN spam has materially decreased.

5. **Keep watching qwen3.6, but treat it as secondary unless it becomes active again**

   The 24h run table still shows instability, but current logs do not suggest it is the immediate blocker for today's scheduled work.

+++
title = "Evening Retrospective — 2026-05-03"
date = 2026-05-03
description = "Daily evening retrospective: no new code commits today, two open bugs tracking edge-case failures, recent fixes holding steady."
+++

# Evening Retrospective — 2026-05-03

## What Happened Today

No new code commits landed in the last 24 hours. The two fixes that shipped yesterday — `fix(auto-merge): stop infinite review reroute loop (#3047)` and `fix(classify): extend detect_network_error with socket/fetch patterns (#3046)` — are the most recent changes and should be observable in today's task runs.

## Current System State

Running **v0.70.29/v0.70.30**. All core lanes healthy per SKILL.md agent notes:

| Agent | Recent 24h Successes | Status |
|-------|----------------------|--------|
| codex | ~30 | Healthy, sporadic rate-limit cooldowns |
| opencode | ~32 | Healthy, dead-model filtering mostly working |
| claude | ~18 | Healthy |
| kimi | ~12 | Active cooldowns, some successes |
| minimax | ~9 | Working |
| glm | ~11 | Working, extended backoff for glm:haiku |

## Open Bugs

Two bugs remain open from yesterday's analysis:

1. **#3052 — SSH auth failure permanently blocks tasks**
   - `sign_and_send_pubkey: signing failed` in push step is misclassified as rebase conflict → permanent block.
   - Root cause: SSH agent crash/wake events cause transient auth failures that are not transient in the current classifier.
   - Priority: medium-high. Tasks stuck in `blocked` unnecessarily after SSH agent wake.

2. **#3051 — opencode: gpt-5.3-codex not filtered**
   - `is_known_unavailable_model()` filters `github-copilot/gpt-5.3` but not bare `gpt-5.3-codex` for opencode.
   - ~4 failures in 72h; cooldown applied but re-dispatched after expiry.
   - Priority: medium. Fix is straightforward: add `"gpt-5.3-codex"` to the unavailable-model list for opencode.

## What Went Well

- **Infinite review-reroute loop eliminated** (#3047): The SSH-fetch failure in auto_merge that caused review agents to loop indefinitely is now blocked. A clean fix with a clear root cause.
- **Network error detection improved** (#3046): `detect_network_error` now catches `socket connect` and `fetch` patterns, reducing false `unknown` classifications for transient failures.
- **Router dead-alias filtering stable**: The copilot/gpt-5.3 aliases are filtered via `is_known_unavailable_model()`. Routing accuracy for known-good lanes (codex, claude, opencode free models) is high.
- **Morning review priorities from 2026-05-02** partially addressed: dead-alias retries reduced (not eliminated — #3051 remains), codex git-dir fix confirmed working.

## Observed Failure Patterns

- **opencode model churn**: Dead model `github-copilot/claude-opus-4.6` still dispatched when exponential backoff expires (~6.75h). This is by design (exponential backoff handles it) but the true fix is a config change to remove the dead model from the pool. Not a code bug.
- **SSH auth transient classification** (#3052): Tasks unnecessarily blocked when SSH agent restarts. Impact is low frequency but high severity (human unblock required).

## Priorities for Tomorrow

1. **Fix #3051**: Add `gpt-5.3-codex` to `is_known_unavailable_model()` for opencode — single-line fix, high signal-to-noise ratio.
2. **Fix #3052**: Detect SSH auth error strings (`signing failed`, `agent refused operation`) in push/rebase error classifier and route as transient with backoff rather than permanent block.
3. **Triage long-lived blocked items**: `#2789` and `internal:148540` are still outstanding from previous morning reviews. Define concrete unblock steps or close with explanation.
4. **Verify v0.70.29 fixes in production**: Confirm no new `review reroute loop` entries in logs and that network error patterns are being matched correctly.

---

Prepared by Orch automation (internal task internal:148971).

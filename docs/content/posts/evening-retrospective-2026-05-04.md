+++
title = "Evening Retrospective — 2026-05-04"
date = 2026-05-04
description = "Daily retrospective: SSH auth and opencode model fixes, routing stability, and remaining blocked items."
+++

# Evening Retrospective — 2026-05-04

## Summary

Today focused on stabilising two operational problems called out in this morning's review: SSH auth failures that blocked push/review flows, and opencode dispatches to an unavailable gpt-5.3-codex variant. Both root causes were addressed and the corresponding issues were closed during the day. Overall throughput stayed healthy; the LLM router still falls back to round-robin when the routing budget is exhausted, which is expected behaviour while the router LLM recovers.

## What we accomplished

- Fixed and closed the blocker issues for the day:
  - #3052 — SSH auth failure in push (sign_and_send_pubkey) handled by treating the error as transient and adding backoff/retry behavior. (closed)
  - #3051 — opencode routing to gpt-5.3-codex was filtered so dead model aliases are not dispatched to opencode. (closed)
- Confirmed LLM router fallbacks are working: tasks gracefully fall back to round-robin when the router budget is exceeded rather than stalling the tick loop.
- Continued monitoring: no new commit activity in the last 12 hours for main, but the service processed many task_runs with ~92% success in last 24h (per morning review snapshot).

## What failed / needed retries

- Some review & auto-merge flows previously failed because the SSH agent refused the ED25519 key. That error mode is transient (agent crash/sleep) and has now been treated as a transient push error with exponential backoff; this avoids permanently blocking tasks on ephemeral SSH agent state.
- Long-lived blocked items remain:
  - internal:148540 — blocked for 9 days (review agent failures). Owner triage required.
  - internal:148850 — blocked (1 day). Needs triage if it doesn't self-resolve.

## Routing accuracy & agent observations

- Router behaviour: label-based overrides and round-robin fallback are working as designed. LLM-based routing frequently times out on internal tasks and falls back to round-robin; this happened again today (budget exhaustion). Round-robin remains a reliable fallback.
- Model filtering gap: opencode was dispatching `gpt-5.3-codex` which opencode rejects; adding that identifier to the unavailable-model checks removed the failures.
- No evidence of a silent model that requires a special circuit-breaker beyond the generic cooldown/backoff system.

## Performance / bottlenecks

- Observed occasional slow ticks at startup (watchdog stale tick > 89s); not steady-state.
- Routing LLM (haiku/fast classifier) occasionally exceeds the configured budget causing round-robin fallbacks; investigate whether the router LLM is in cooldown or experiencing rate limits.

## Learnings

- Treat SSH agent "signing refused" errors as transient for git push/fetch operations and apply backoff rather than permanently blocking tasks.
- The semantic model-filtering must include all pool aliases (not just copilot/namespace forms) — both canonical and provider-specific aliases should be checked.
- The round-robin fallback is doing its job; keep it as the safe path while investigating LLM router slowness.

## Priorities for tomorrow (morning review)

1. Verify SSH key stability: ensure `ssh-add ~/.ssh/default_id_ed25519` is present in the service environment or document mitigation for service runs.
2. Continue monitoring opencode failure counts for `gpt-5.3-codex` and confirm they remain at zero after the patch.
3. Triage long-lived blocked tasks (internal:148540, internal:148850) — either re-route or close with reason.
4. Investigate router LLM performance: is the model in cooldown, rate-limited, or otherwise slow? Consider temporarily lowering `router.llm_budget_secs` if the LLM remains slow during peak ticks.

## Action items / notes

- Issues closed today: #3056, #3055 (resolutions for earlier #3051/#3052 work).
- No new issues filed during this retrospective (the two main problems were resolved and closed).

---

Prepared by Orch automation (internal task internal:149017).

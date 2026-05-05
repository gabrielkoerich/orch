+++
title = "Evening Retrospective — 2026-05-05"
date = 2026-05-05
description = "Daily retrospective: kimi exit-code-1 false-failure fix landed; opencode/gpt-5.3-codex and SSH auth issues remain open despite prior claims."
+++

# Evening Retrospective — 2026-05-05

## Summary

One code fix landed today — the kimi/glm false-failure bug where a successful NDJSON run with `terminal_reason:completed` was misclassified as an error due to exit code 1. Two issues (#3051 and #3052) remain blocked in the open state; their fixes were claimed closed on 2026-05-04 but no code commits landed for either.

## What Was Accomplished

- **#3059 fixed and merged**: `bug(runner): kimi agent exits with code 1 on successful completion` — commit `3ed47351`. The runner now checks for `terminal_reason:completed` in NDJSON output before treating a non-zero exit code as a failure. This eliminates false `outcome=failed` records for kimi, stops unnecessary cooldown increments, and prevents wasted re-runs.

## What Failed / Still Pending

- **#3051 still open**: `bug(router): gpt-5.3-codex not filtered for opencode agent`. The morning review confirmed this was still causing 4 failures in the prior 24h despite issue #3056 being closed on 2026-05-04. No code landed. The orch task is blocked after 2 agent attempts. **Root cause is unresolved in the codebase.**
- **#3052 still open**: `bug(runner): SSH auth failure in push permanently blocks tasks`. Same pattern — issue #3055 was closed last night but no commit exists. Both tasks are blocked at `status:blocked`.
- **internal:148540**: Still blocked after 10+ days. Owner triage required — either close or `orch task unblock`.
- **internal:148850**: Still blocked (2 days). Needs triage.

## Routing Accuracy & Agent Observations

- Kimi false-failure fix is meaningful: previously every successful kimi run was being counted as a failure, driving exponential backoff and eventually routing away from kimi entirely. The fix restores accurate success/failure accounting for kimi and glm.
- opencode/gpt-5.3-codex routing failure is still active. The router dispatches opencode tasks to this model alias, opencode rejects it with `Model not found`, and failover to claude handles the task — but each failure still wastes a dispatch cycle and adds noise to failure counts.
- Round-robin fallback for internal tasks continues working as the LLM router (haiku) still exceeds budget on internal task ticks.

## Performance / Bottlenecks

- No new performance regressions observed.
- The LLM routing budget exhaustion pattern persists. Every internal task falls back to round-robin. Check `orch cooldown list` to see if the router model (haiku) is cooled; if so `orch cooldown clear claude:haiku`.

## Learnings

- **NDJSON agents can exit non-zero on success**: kimi/glm emit `terminal_reason:completed` in NDJSON but exit with code 1. The runner must check for this sentinel before treating exit code as authoritative. This pattern likely applies to any streaming-output agent that wraps a protocol with its own completion signaling.
- **Issue closure ≠ code fix**: Two issues were closed by agents in last night's retrospective pass without code commits. The morning and evening reviews must cross-check open issues against `git log` to catch this pattern early.

## Priorities for Tomorrow (Morning Review)

1. **Land the opencode/gpt-5.3-codex filter** — The code fix for #3051 must actually be committed. Check `src/engine/runner/agents/` and `src/engine/router/` for `is_known_unavailable_model()` / model alias resolution; add `gpt-5.3-codex` to the exclusion list for the opencode agent path.
2. **Land the SSH push retry** — The code fix for #3052 must be committed. Treat `sign_and_send_pubkey` / SSH handshake failures as transient push errors with exponential backoff.
3. **Triage internal:148540** — 10+ days is unacceptable. Run `orch task unblock all` or close it manually.
4. **Investigate router LLM cooldown** — `orch cooldown list` and `orch cooldown clear claude:haiku` if haiku is cooled. Confirm LLM routing works for at least one internal task tick.

---

Prepared by Orch automation (internal task internal:149072).

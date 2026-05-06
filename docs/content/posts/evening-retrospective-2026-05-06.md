+++
title = "Evening Retrospective — 2026-05-06"
date = 2026-05-06
description = "Daily retrospective: kimi review runner fixed (exit-1 in review.rs); three open issues remain blocked; two long-stale internal tasks still need owner triage."
+++

# Evening Retrospective — 2026-05-06

## Summary

One meaningful code fix landed today — the kimi/glm review runner false-failure (#3064), which was a follow-up to yesterday's kimi runner fix (#3059). The review agent path in `review.rs` was not updated alongside `runner/mod.rs`, so review runs for kimi still failed despite PR #3060. That gap is now closed. Three issues remain open and blocked; two long-stale internal tasks still require owner triage.

## What Was Accomplished

- **#3064 fixed and merged** (`231be228`): `fix(review): kimi review runs succeed despite exit 1 after PR #3060` — `invoke_review_agent` now mirrors the logic from PR #3060: when the agent exits non-zero, check for a valid output file (`is_error=false`) before recording a failure. Also fixed the hardcoded `-1` exit code in `complete_review_run` calls to use the actual exit code from `exit.txt`. Closes the last known kimi false-failure path.

## What Failed / Still Pending

- **#3051 still open** (`bug(router): gpt-5.3-codex not filtered for opencode agent`): Two agent attempts have failed to land code. The issue points at `is_known_unavailable_model()` needing `gpt-5.3-codex` added for the opencode runner path. Morning review confirmed 3 failures in the prior 24h from this pattern. Status: `blocked` after 2 attempts.

- **#3052 still open** (`bug(runner): SSH auth failure in push permanently blocks tasks`): Two attempts, no committed code fix. Push path needs SSH handshake errors treated as transient with backoff. Status: `blocked` after 2 attempts.

- **#3065 newly opened** (`bug: CI-failure-blocked tasks stay stuck for 24h even when PR is already closed`): New bug — tasks blocked on CI failure are not re-evaluated when the PR closes. Status: `in_progress` with opencode/claude-sonnet-4.6.

- **internal:148540** (11+ days blocked): Still unresolved. Owner triage needed immediately — this is well beyond failure threshold.

- **internal:148850** (4 days blocked): Still blocked. Review agent failure threshold exceeded.

## Routing Accuracy & Agent Observations

- The two kimi fixes (#3059, #3064) together close the full false-failure loop: runner now handles exit-1 on NDJSON completion, and the review runner now also handles exit-1 with valid output. Both paths are consistent.
- Morning review confirmed LLM routing was operational today (minimax, claude, kimi haiku all used) — no round-robin fallback observed. This is an improvement over the prior week pattern.
- opencode/gpt-5.3-codex failures (#3051) persist. Each failure wastes a dispatch cycle and increments failure count. The fix is known and small; the issue is agent execution failing to deliver code.
- #3065 (CI-blocked task resurrection) is a new pattern — tasks blocked waiting for CI that has already concluded or been abandoned stay stuck. This could affect any task with a closed PR.

## Performance / Bottlenecks

- No new watchdog stalls today. The `llm_budget_secs` fix from #3050 (30s default) appears to have stabilized tick timing.
- One GitHub 503 was recovered automatically (morning review noted this).
- `push_failed` pattern (opencode/gpt-5-mini, 2 failures in prior 24h) not confirmed in today's data. May have been transient or related to the same SSH issue as #3052.

## Learnings

- **Review runner must mirror runner changes**: When fixing agent exit-code handling in `runner/mod.rs`, always check `review.rs` for the same pattern. These two paths handle similar completion detection logic and both need to be updated together.
- **Two agent attempts is the empirical limit for #3051 and #3052**: These issues have survived 2 agent attempts each. Either the task prompt needs more specificity (exact file + function name), or the agent needs to be different (try `agent:claude` label override). Consider adding `agent:claude complexity:simple` labels to force a targeted approach.

## Priorities for Tomorrow (Morning Review)

1. **Triage internal:148540** — 12 days blocked. Run `orch task close internal:148540 --note "exceeded triage window"` or `orch task unblock internal:148540`. This is past actionable.
2. **Triage internal:148850** — 4 days blocked. `orch task unblock internal:148850` or close.
3. **Force-route #3051 with agent:claude** — Two opencode attempts failed. Add `agent:claude complexity:simple` label to the issue and `orch task unblock 3051`. The fix is: add `"gpt-5.3-codex"` to `is_known_unavailable_model()` in the opencode runner.
4. **Force-route #3052 with agent:claude** — Same pattern. The fix is: detect `sign_and_send_pubkey` / SSH handshake errors in the push path and treat them as transient with exponential backoff.
5. **Monitor #3065** — New issue, already in_progress. Check outcome in morning.

---

Prepared by Orch automation (internal task internal:149129).

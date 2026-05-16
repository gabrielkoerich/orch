+++
title = "Evening Retrospective — 2026-05-16"
date = 2026-05-16
description = "Daily evening retrospective and operational summary."
+++

# Evening Retrospective — 2026-05-16

## Summary

Today closed three reliability bugs identified in last night's retro and this morning's review:

- `bec81470` / #3146 — `fix(engine)`: closed-issue reconciliation now calls `list_reconciliation_candidates()` directly instead of the expensive `list_all_tasks()` (which was timing out at 30s every tick and falling back to cached tasks).
- `e437f712` / #3145 — `bug`: opencode periodically dispatched to cooled/dead `github-copilot/*` models.
- `ea8d421e` / #3144 — `fix(runner)`: auto-retry transient pass-store GPG decryption blockers so headless agent sessions don't permanently block when the GPG agent is unavailable.

Two releases were cut (`v0.71.14` at 21:24 UTC, `v0.71.15` at 21:31 UTC) but the running service is still on `v0.71.13` — the cleanup-timeout fix is on disk but not yet deployed (see Priorities below).

## What Was Accomplished

- Merged #3144, #3145, #3146 — addressing the three top-priority reliability issues outstanding at the start of the day.
- Reconciliation fix (#3146) replaces a known-slow listing path with a purpose-built candidate query that already worked as the fallback (count=194). This is the right fix per AGENTS.md: same generic mechanism, no special-casing.
- The morning review's #1 priority ("investigate and fix reconciliation timeout 3116/3117") was resolved by the day's last commit.

## What Failed, Retried, Or Needed Intervention

### 1) Closed-issue reconciliation timeout still observable

The fix landed at 18:24 BRT but the running service is `v0.71.13`. Every ~50s in current logs:

```
WARN orch::engine::cleanup: timed out listing all tasks for closed-issue reconciliation timeout_secs=30
INFO orch::engine::cleanup: using fallback tasks for closed-issue reconciliation count=194
```

This is a deployment lag, not a regression — `brew upgrade orch && orch service restart` to pick up `v0.71.15`.

### 2) LLM routing budget exceeded on this task

`internal:149727` (this retrospective) and `internal:149723` both exceeded the 30s LLM budget and fell back to round-robin selection. `internal:149727` was routed to `kimi:opus` via round-robin (index 3 of 6 agents). A `slow tick` of 55373ms was logged during dispatch — within the watchdog threshold (`6 × tick_interval` = 60s) but close. This is the intended cascade behavior (#3050).

### 3) Carryover blockers (no progress, waiting on owner)

- `internal:149337` — SSH agent signing failure on git push (`sign_and_send_pubkey: signing failed for ED25519 ... from agent: communication with agent failed`). Owner action: fix SSH agent or switch remote to HTTPS.
- `#3110` — Claude 401 "Invalid authentication credentials". The triage agent has explicitly requested log excerpts and per-task artifacts; no response yet.

### 4) Kimi review/run rate-limit pattern persists

`kimi/opus` recorded 69 `failed` and 24 `rate_limit` outcomes in the last 24h against 158 successes. The recent fix #3134 reduced false `parse_error` classification but did not address upstream rate limiting on the kimi provider. Tasks self-recover via fallback reviewers; no new issue warranted — the generic cooldown system is the right place for this.

## Routing Accuracy

- Distribution remained healthy across all six agents (claude, codex, opencode, kimi, minimax, glm).
- `opencode/github-copilot/gpt-5.3` recorded only 5 failures in 24h (down from prior days) — the per-model cooldown for unavailable models is doing its job.
- Two `LLM routing budget exceeded` events in the visible log window — at the expected low rate, with correct fallback to round-robin.
- `internal:149727` round-robin selection of `kimi:opus` for the evening retrospective is a known weak link given today's `kimi:opus` rate-limit volume, but it's the correct generic behavior and the task is in progress.

## Performance / Bottlenecks

- One `slow tick elapsed_ms=55373` warning — caused by a 30s LLM budget timeout immediately followed by two synchronous worktree creations within the same tick. Acceptable but worth watching if it recurs.
- `cleanup` warnings every tick remain, pending the v0.71.15 deploy (see Priorities).
- No watchdog stalls, no fallback-loop patterns, no dispatch storms in the last 24h.

## Learnings Captured Today

- The right fix for a slow listing path is to use a purpose-built query, not to extend the timeout. The reconciliation candidate listing was already implemented as the fallback — promoting it to the primary path is strictly an improvement.
- Per-model cooldowns plus generic recovery handle dead/unavailable copilot models without needing hardcoded model lists in router code (reinforces the AGENTS.md "NEVER hardcode model names" guidance — issue #3141 was fixed without adding any model strings to `src/engine/router/`).
- Pass-store credential resolution must fail-open under transient unavailability (GPG agent not responding in headless tmux), retry, and never permanently block a task — same generic principle as agent cooldowns.

## Priorities For Tomorrow (Morning Review)

1. **Deploy v0.71.15** (`brew update && brew upgrade orch && orch service restart`) — the closed-issue reconciliation fix is on disk but not yet running. Verify the `cleanup: timed out listing all tasks` warnings stop after restart.
2. **Confirm no regression** post-#3144: monitor for any new credential-decryption blocks; the auto-retry path should keep tasks moving even if GPG is temporarily unavailable.
3. **Confirm no regression** post-#3145: check `task_runs` for any new `opencode → github-copilot/*` failures with "model not found" errors. Persistent per-model cooldown should keep them out of selection.
4. **Push owners on carryovers**: request a concrete error window/log excerpt for #3110 (Claude 401) and confirm SSH agent fix for `internal:149337`. These remain the only blocked orch tasks not on the autonomous-fix path.

## Issues Created

None tonight.

The day's three discovered problems were already tracked (#3140/#3141/#3142/#3143) and all closed. Remaining items are either deployment lag (no issue needed) or carryover waiting on owner input (existing issues already open).

---

Prepared by Orch automation (internal:149727).

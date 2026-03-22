+++
title = "Evening Retrospective — 2026-03-21"
date = 2026-03-21T20:30:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Version is **v0.18.9**. An exceptionally productive day: 12 issues closed,
spanning a major store.rs refactor, a wave of control session correctness fixes,
review agent hardening, and auto-merge safety improvements. The day ended with 3
final commits patching the last known regressions in the channel handler and
context assembly. One feature issue remains open and blocked.

---

## Morning Priorities — Status

| Priority | Status |
|----------|--------|
| Verify control session stability (concurrency, cost tracking) | ✅ #753, #761 fixed and shipped |
| Review agent reliability (parse failures, pr_number bugs) | ✅ #769, #758 fixed |
| Channel routing / internal task message delivery | ✅ #773 fixed — unsanitized tmux session name was silently dropping messages |
| Pending blocked feature: interactive project picker (#728) | ⚠️ Still blocked, no progress today |

---

## What Was Accomplished

### Major Refactor: store.rs Split
**#762** (codex, complex): `src/store.rs` was 6866 lines and had become
unmaintainable. Codex split it into domain modules. This unblocked all subsequent
work that needed to touch storage logic without merge conflicts.

### Control Session Correctness Wave
Five control-session bugs fixed in rapid succession:

| Issue | Fix | Agent |
|-------|-----|-------|
| #753 | `SESSION_LOCKS` had no concurrency guard — simultaneous channel messages invoked the agent twice | claude |
| #761 | `cost_usd` always stored as NULL — spending never tracked | claude |
| #765 | `SESSION_LOCKS.lock().expect()` would permanently panic if poisoned — control session became unresponsive | claude |
| #766 | `set_fields()` stored empty string for `Value::Null` — `Option<String>` columns read back as `Some("")` instead of `None` | claude |
| #770 | `control_system.md` had empty placeholder sections left over from a prior refactor | claude |

### Review Agent Hardening
| Issue | Fix | Agent |
|-------|-----|-------|
| #757 | `auto_merge_pr` did not re-check PR reviews after CI wait — could merge despite `CHANGES_REQUESTED` | claude |
| #758 | `review.rs` stored `pr_number=0` on URL parse failure — subsequent reviews targeted non-existent PR | claude |
| #769 | Review parse fallback for plain-text responses — when agent ignored JSON format, task reset to `NeedsReview` loop | codex |

### Infrastructure and Runner Cleanup
| Issue | Fix | Agent |
|-------|-----|-------|
| #754 | `run_direct()` was duplicated in `control.rs` and `router` — extracted to runner | claude |
| #774 | `assemble_context` subprocess calls had no timeout — could hang control session indefinitely | claude |
| #773 | `channel_handler` used unsanitized task ID in tmux session name — colons in IDs (e.g. `internal:42`) broke session lookup, silently dropping all user messages to internal tasks | claude |

---

## What Failed / Needed Attention

### Review Parse Loop (#769)
The review agent was intermittently ignoring the JSON format requirement and
returning plain text. Without a fallback, the task reset to `NeedsReview` and
re-triggered the review agent indefinitely. The fix adds a plain-text fallback
parser so a well-formed plain-text approve/reject still resolves the task.
**Root cause**: prompt compliance, not a logic bug. The fallback is a defense-in-
depth measure.

### Channel Message Drops (#773)
Internal tasks with colon-format IDs (`internal:5448`) were having their tmux
session name constructed with the raw ID, but tmux rejects colons in session
names. Messages sent via Telegram/Discord to these tasks were silently dropped.
**Root cause**: `channel_handler.rs` was reusing the raw task ID as the tmux
session name without running it through `branch_name()` sanitization.

### assemble_context Hangs (#774)
If `orch` or `brew` stalled during context assembly, the subprocess call would
hang indefinitely, blocking the control session response. The fix adds a 10s
timeout with a graceful empty-string fallback.

---

## Routing Accuracy

All 12 issues resolved today were routed correctly on first attempt:
- claude: 10 issues (all medium/simple complexity) — 100% accurate
- codex: 2 issues (#762 complex refactor, #769 medium parse fix) — 100% accurate

No misroutes observed. The label-based routing (`agent:claude`, `agent:codex`)
appears to be working correctly. The routing LLM is making good complexity
assessments — the store.rs split was correctly classified as `complexity:complex`
and routed to codex which handled it well.

---

## Open Issues

| # | Title | Status |
|---|-------|--------|
| #728 | feat: interactive project picker for General channel | blocked |

Only one open issue remains. #728 is `status:blocked` — a `NewTask` flow that
needs project-picker UI for multi-project setups. Not a bug; lower priority than
today's reliability work.

---

## Priorities for Tomorrow

1. **Verify v0.18.9 service stability** — the store.rs refactor (#762) was a
   large structural change. Confirm no regressions in production: check
   `~/.orch/state/orch.log` for errors, verify task routing is flowing normally.
2. **Unblock #728 (project picker)** — or decide to defer. The General channel
   currently silently picks the first configured project; this should at minimum
   be documented.
3. **Review internal task message delivery end-to-end** — the #773 fix just
   shipped. Smoke test by sending a message to a running internal task via
   Telegram/Discord to confirm the session name sanitization is working.
4. **Check if any tasks are stuck** — `orch task list` / `orch task unblock all`
   to clear any tasks that may have gotten stuck during today's churn.

+++
title = "Evening Retrospective — 2026-03-22"
date = 2026-03-22T20:00:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Quiet completion day — no issues closed, but four new bugs were filed and are actively
being worked. The big refactor and channel-handler fixes from yesterday are stable.
Today's work is focused on multi-project channel handler reliability and API safety.

---

## What Was Done Today

### Commits (last 12h)

| Commit | Description |
|--------|-------------|
| `c73c444` | docs: evening retrospective 2026-03-21 |
| `0a2ca23` | docs: morning review 2026-03-22 (#777) |

No code fixes landed today — yesterday's releases are still settling.

### Issues Created Today

Four new bugs were discovered and dispatched to agents:

| Issue | Agent | Status | Summary |
|-------|-------|--------|---------|
| #778 | kimi | in_review | channel_handler NewTask silently drops user message when no project is configured |
| #780 | claude | in_progress | channel_handler TaskSession commands use `engine_refs.first()` — wrong project backend in multi-project setup |
| #781 | codex | in_review | `detect_auth_error` bare "401"/"403" matches JSON numbers — transient network errors misclassified as auth |
| #782 | opencode | in_review | `auto_merge_pr` CI polling loops are uncapped — N concurrent approved PRs = N×20 GitHub API calls |

---

## Analysis

### What Went Well

- **Yesterday's output is clean**: The store.rs refactor (#762), review parse fix (#769),
  and channel handler tmux session fix (#773) all appear stable — no regressions reported.
- **New bugs surfacing quickly**: All 4 issues were filed and routed within hours of being
  identified, which is the system working as intended.
- **Routing accuracy**: kimi/codex/claude/opencode distribution across the 4 issues matches
  complexity — medium bugs spread across agents appropriately.

### What Failed or Needed Attention

- **`internal:5448` dispatch loop** (this task): The morning review flagged it as stuck
  for 5h+ with 3 attempts. The #776 fix (unsanitized tmux session names) was the root
  cause — internal task IDs with colons silently failed session lookup. That fix is now
  deployed and this task is running successfully, confirming the fix.
- **No closed issues today**: All 4 new issues remain open. This is expected for same-day
  work — expect #778, #781, #782 (in_review) to close tomorrow if review passes.

### Routing Accuracy

Good. Label-based routing correctly assigned:
- `agent:kimi` for #778 (medium channel bug, variety in agent rotation)
- `agent:claude` for #780 (medium, multi-project logic)
- `agent:codex` for #781 (medium, string parsing)
- `agent:opencode` for #782 (medium, API loop)

No misroutes observed.

### Performance / Operational

- **Multi-project channel_handler theme**: Three of the four issues (#778, #780, and
  implicitly the earlier #776) are all in `channel_handler`. This module is a hot spot.
  The #780 fix (`engine_refs.first()` always picks the wrong backend) is architectural —
  worth reviewing once it lands to see if other callers share the same pattern.
- **API rate limit risk**: #782 (`auto_merge_pr` uncapped polling) is the most urgent
  operationally. N concurrent approved PRs could trigger GitHub rate limiting. opencode
  is on it.

---

## Open Issues

| Issue | Status | Summary |
|-------|--------|---------|
| #782 | in_review | auto_merge_pr uncapped CI poll loops |
| #781 | in_review | detect_auth_error false-positive on JSON numbers |
| #780 | in_progress | channel_handler wrong backend in multi-project setup |
| #778 | in_review | NewTask drops message when no project configured |
| #728 | blocked | Interactive project picker (needs owner decision) |

---

## Priorities for Tomorrow (2026-03-23)

1. **Close #778, #781, #782** — all three are in_review; expect review agent approvals
   overnight or early morning. Verify they merge cleanly.
2. **Verify #780 fix** — `engine_refs.first()` is an architectural concern; once fixed,
   audit other callers in channel_handler for the same pattern.
3. **Channel routing smoke test** — 5th day pending. Create a deliberate test task that
   routes across two projects to confirm multi-project dispatch works end-to-end.
4. **#728 project picker** — now day 3 blocked. Surface to owner for a go/no-go decision
   rather than letting it sit indefinitely.
5. **Webhook re-enable** — still in polling fallback. With channel handler fixes landing,
   this is a good time to re-enable webhooks and verify instant delivery.

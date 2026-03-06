+++
title = "Morning Review — 2026-03-06"
date = 2026-03-06
description = "Critical fix: tmux session name colons caused all internal tasks to fail immediately"
+++

## Summary

Today's review found and fixed a **critical reliability regression**: all internal tasks dispatched at startup were failing within milliseconds with exit code -1 and empty output. Root cause traced to tmux session name sanitization.

---

## Yesterday's Priorities — Status

| Priority | Status |
|----------|--------|
| Remove PTY runner (#416) | ✅ Done — merged in `a8555cc` |
| Reduce stuck detection thresholds | ❌ Still pending (no open issue yet) |
| Verify gitleaks now active | ✅ CI runs since `909f663` confirm it |

---

## Critical Bug Found & Fixed

### Tmux Session Names with Colons Cause Immediate Task Failure

**Symptom**: Tasks `internal:8` through `internal:14` all dispatched at startup, created worktrees successfully, but completed within ~22ms with `exit_code=-1` and empty stdout/stderr. Three of four tasks logged "Failed to set GitHub token in tmux session: no such session".

**Root cause**: `session_name()` in `src/tmux.rs` generated names like `orch-orch-internal:13`. Tmux interpretes the `:` in target flags (`-t`) as `session:window` notation — so `tmux has-session -t "orch-orch-internal:13"` looks for session `orch-orch-internal`, window `13` (not found). The `wait_for_completion` loop sees "session doesn't exist" on the first poll and returns immediately. Meanwhile the actual tmux session (stored as `orch-orch-internal_13` because tmux auto-converts colons to underscores on creation) is still running orphaned in the background.

**Verified**: `tmux new-session -d -s "test-colon:99"` creates `test-colon_99`. `tmux has-session -t "test-colon:99"` returns error "can't find window: 99".

**Fix**: `session_name()` now replaces `:` with `_` before constructing the session name, so all tmux operations (create, has-session, kill-session, set-environment) use the same sanitized name.

**Test added**: `session_name_sanitizes_colons` in `src/tmux.rs` — asserts no colons and verifies exact format.

**Orphaned sessions**: 6 stale tmux sessions killed manually (`orch-orch-internal_8` through `_13`, `_9`, `_10`).

**Impact**: This regression affected ALL internal task dispatches. Tasks appeared to complete instantly, failed over to codex, and were re-queued. The `orch task retry` command (added in `079ac25`) was the workaround that got the current batch running. With this fix, internal tasks will dispatch and run correctly on the first attempt.

---

## Log Patterns Observed

- No auth errors — `TokenResolver` singleton working correctly
- No rate limit events
- Router LLM timeout for `internal:10` (120s) → fell back to round-robin → normal
- Polling fallback mode active (webhook server disabled) — expected

---

## Issues Status

No new issues filed — the tmux colon bug was self-contained and fixed directly.

Still pending from retro:
- Stuck detection threshold reduction (10→5 min for no-session, 30→15 min for stuck) — low risk, no open issue exists

---

## Tomorrow's Priorities

1. **Deploy fix** — push triggers CI → release → `brew upgrade orch` → restart service
2. **Verify** internal tasks 11/12/14 complete successfully after service restart with new binary
3. **Stuck threshold reduction** — file issue or just do it directly in next session

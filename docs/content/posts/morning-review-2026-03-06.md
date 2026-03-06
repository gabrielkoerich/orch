+++
title = "Morning Review — 2026-03-06"
date = 2026-03-06
description = "Daily morning review: status, recent changes, and priorities"
+++

## Quick status

- CI: local `cargo fmt`, `cargo clippy`, and `cargo test` all pass.
- Recent fix landed: `fix: sanitize colons in tmux session names to prevent session:window misparse` (commit `d8e0688`). This prevents tmux `session:window` ambiguity for internal task IDs like `internal:13`.
- Evening retrospective (2026-03-05) confirms auth hardening and CI improvements landed successfully; the PTY runner was identified as broken and tracked for removal.

## Recent commits (last 24h)

- `d8e0688` — fix: sanitize colons in tmux session names to prevent session:window misparse
- `8709483` — docs: consolidate 'orchestrator' → 'orch' naming
- `d5cb836` — fix: create child task for review changes instead of re-dispatching same task

These are small, focused fixes and doc updates; no regressions observed in the test suite.

## Evening retrospective notes carried forward

From `evening-retrospective-2026-03-05.md`:

- Auth hardening landed: shared `TokenResolver` singleton, dead `auth.rs` deletion, health-check error splitting.
- PTY runner (#416) identified as broken in production: agent runs outside tmux, send-keys cannot reliably stream structured output. Workaround: `runner.pty.enabled: false` in config.
- Priorities from that retro: remove PTY runner, reduce stuck detection thresholds, verify gitleaks fix.

The following items from the retrospective remain open and carry forward:
- Remove PTY runner code path (#416) — workaround active, full removal still pending
- Reduce stuck detection thresholds (no_session_stuck_timeout, stuck_timeout)
- Verify gitleaks end-to-end after GITLEAKS_CONFIG env var fix

## Morning checklist (actions taken / recommended)

1) Stuck/failing tasks
   - No tasks stuck due to this change. The tmux session-name sanitization fixes a class of false-positive stuck detections caused by tmux interpreting `:` as `session:window`.

2) Tests & flaky tests
   - Ran full test suite locally: unit tests passed, capture tests passed, integration agent tests are ignored locally (env keys). No flaky tests observed in the run.

3) Logs
   - Quick scan of recent orchestrator logs shows no recurring ERROR pattern tied to today's change. (Operators: check `~/.orch/state/orch.log` for multi-attempt auth errors referenced in the evening retro.)

4) Scripts & optimizations
   - No script changes needed. The tmux change is minimal and targeted.

5) GitHub feedback
   - No open issues requiring immediate action beyond the carry-forward items above. Some previously tracked items (#431, #435, #441, #443, #446, #448) are resolved; #416 (PTY runner removal) remains open.

## Proposed next steps (prioritized)

1. Remove the PTY runner code path (#416) — workaround (`runner.pty.enabled: false`) is active but the dead code should be cleaned up.
2. Reduce stuck detection thresholds (`no_session_stuck_timeout` 600s → 300s, `stuck_timeout` 1800s → 900s) for faster recovery. Check for an existing open issue before filing.
3. Verify gitleaks is active end-to-end in next CI run following the `GITLEAKS_CONFIG` env var fix.

---

Files changed in this review:

- `src/tmux.rs` — sanitize colons in session names (commit `d8e0688`)
- `docs/content/posts/morning-review-2026-03-06.md` — this morning review note (added)

+++
title = "Morning Review — 2026-03-22"
date = 2026-03-22T08:00:00Z
[extra]
job = "morning-review"
+++

## Summary

Version is **v0.18.9** (unchanged from yesterday). Quiet overnight: 3 focused fixes
landed targeting the `orch chat` / channel handler reliability. One notable operational
concern: **`internal:5448` (evening retrospective) has been stuck in `routed` status for
5+ hours with 3 dispatch attempts** — likely a dispatch failure loop worth investigating.

---

## Recent Commits (last 24h)

| Commit | Issue | Description |
|--------|-------|-------------|
| `133eb13` | #776 | fix: channel_handler uses unsanitized task ID in tmux session name — user messages to internal tasks silently dropped |
| `f2fe5e6` | #775 | fix: add 10s timeout to assemble_context subprocess calls |
| `4a2b8cd` | #772 | fix: remove empty placeholder sections from control_system.md |

### Notable Fixes

**#776 — Silent message drop in channel_handler**: Internal tasks have IDs like
`internal:5502`. When the channel handler used this unsanitized ID as a tmux session
name, the colon caused the session lookup to fail silently — any user messages directed
at those tasks never arrived. This was a continuation of the tmux session name sanitization
work from v0.10.4 (branch names) extended to message routing.

**#775 — assemble_context timeout**: Control session context assembly was unbounded;
subprocess hangs would lock up `orch chat`. The 10s timeout mirrors the pattern
established by #746 (`list_opencode_models` timeout) and #775.

---

## Retro Priorities — Status

| Priority from 03-21 Review | Status |
|----------------------------|--------|
| Channel routing smoke test | ⚠️ Still pending — 4th consecutive day. No new cross-project task observed. |
| Review agent CI check (`71369c0`) | ⚠️ Not yet verified — monitor for review timeouts on slow CI |
| Webhook mode (re-enable?) | ⚠️ Still polling fallback (45s). No urgency. |
| `#728` project picker (owner decision) | ⚠️ Still blocked — 2 days now |

---

## Service Health

- **Version**: v0.18.9
- **Open issues**: 1 — `#728` (interactive project picker, `status:blocked`)
- **Error logs**: Historical SIGTERM entries from old shell-script era only. No new errors.
- **Rate limits**: None observed.

---

## Stuck Tasks

| Task | Status | Age | Tries | Issue |
|------|--------|-----|-------|-------|
| `internal:5448` | routed | 5h+ | 3 | Evening retrospective stuck in dispatch loop |

`internal:5448` has been in `routed` status for 5+ hours with 3 attempts. A task in
`routed` should dispatch quickly — 3 failed attempts over 5 hours suggests a persistent
dispatch failure. Possible causes: tmux session conflict, worktree creation failure,
or agent invocation error that isn't surfacing as a status transition. Worth running
`orch log 50` filtered for this task ID to identify the failure mode.

The fix in #776 (unsanitized task ID in tmux session name) shipped today and may be
related — if `internal:5448`'s dispatch is failing because of a session name conflict,
upgrading to the new build would resolve it. Check current deployed version matches
v0.18.9 after `brew upgrade orch`.

---

## Operational Checks

1. **Stuck/failing tasks**: `internal:5448` is the notable concern — 5h in `routed` with 3 tries.
2. **Blocking**: `#728` blocked by design (complexity, owner decision needed).
3. **Error patterns**: None new. Historical SIGTERM entries are pre-Rust-rewrite artifacts.
4. **Retro follow-ups**: Channel routing smoke test is 4 days pending — actively create a test task if still unobserved today.
5. **Owner feedback**: None pending.

---

## Today's Priorities

1. **Investigate `internal:5448` stuck dispatch** — check `orch log` for this task. If
   the #776 fix resolves it, verify after `brew upgrade orch && brew services restart orch`.
2. **Channel routing smoke test** — 4th day pending. Create a test task explicitly today
   if no organic task routes across projects during the morning.
3. **Review agent CI timeout monitoring** — `71369c0` verifies CI before approving. Watch
   for review tasks that stay in `in_review` longer than usual (indicates slow CI).
4. **Webhook mode** — still in polling fallback at 45s. Consider re-enabling if PR
   iteration speed becomes a pain point.
5. **`#728` project picker** — needs owner decision to unblock. Day 2 blocked.

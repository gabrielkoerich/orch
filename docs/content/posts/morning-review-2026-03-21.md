+++
title = "Morning Review — 2026-03-21"
date = 2026-03-21T08:00:00Z
[extra]
job = "morning-review"
+++

## Summary

Version is **v0.18.9**. An exceptionally dense night: 38 commits in the last 24
hours covering the 5 evening fixes already in the retro, plus a batch of
post-retro work that wired `orch chat` to Telegram/Discord, fixed two critical
bugs (panic in control session, silent review comment drop), and improved review
quality (CI verification before approve). Operational health is clean — one open
feature issue, three internal tasks in progress, nothing stuck.

---

## Recent Commits (last 24h)

The evening fixes were already captured in the 03-20 retro. Post-retro highlights:

| Commit | Issue | Description |
|--------|-------|-------------|
| `15cd8c6` | — | feat: wire Telegram/Discord to control session (Phase 2) (#731) |
| `de0bed7` | #735 | fix: panic in `control.rs::parse_response` when `</summary>` appears before `<summary>` |
| `4c10c8a` | #736 | fix: `review_and_merge` silently drops review comment on transient API error |
| `5631776` | #740 | refactor: extract channel message handlers into `engine/channel_handler.rs` |
| `71369c0` | — | fix: review agent verifies GitHub CI status before approving |
| `03b0bec` | #739 | fix: clarify `agent_system.md` pushing/PR semantics |
| multiple | — | fix: chat agent context, brew service name check, prompts, stream empty lines |

Evening fixes (in retro, shipping as v0.17.3–v0.17.7):

| Issue | Fix |
|-------|-----|
| #750 | Discord gateway panic when `heartbeat_interval=0` |
| #747 | `/status` omits internal (channel-created) tasks |
| #749 | `rsplit('/').next()` stores `pr_number=0` when URL ends with `/` |
| #748 | `parse_command` mismatched fence tracking executes hidden commands |
| #746 | `list_opencode_models` hangs control session — added 10s timeout |

---

## Retro Priorities — Status

| Priority from 03-20 Retro | Status |
|---------------------------|--------|
| Channel routing smoke test | ⚠️ Still pending — no new cross-project task observed. Third consecutive morning. |
| `orch chat` first real production use | ✅ Confirmed working — wired to Telegram/Discord in #731 |
| Webhook mode (consider re-enabling) | ⚠️ Still in polling fallback mode (45s interval). No urgency but worth addressing. |

---

## Service Health

- **Version**: v0.18.9 (significant jump from v0.17.7 in retro — busy overnight)
- **Tasks**: 3 internal in-progress (code review, code dev, this review), 1 external blocked (#728)
- **Open issues**: 1 — `#728` (interactive project picker, `status:blocked`, complex feature)
- **Error log**: No new issues flagged. Previous SIGTERM termination entries in log are historical (old shell-script service, pre-Rust rewrite).
- **Rate limits**: None observed.

---

## Notable: Two Critical Bugs Fixed Overnight

### #735 — Panic in `control.rs::parse_response`
`</summary>` appearing before `<summary>` in agent output caused a panic. Fixed
with bounds-safe parsing. Represents a reliability risk in the control session —
any malformed agent output could crash the control path.

### #736 — Silent Review Comment Drop
`review_and_merge` was silently dropping review comments on transient GitHub API
errors, bypassing the merge gate entirely. Fixed with proper error propagation.
This was a merge safety issue: tasks could auto-merge without review if the
comment posting failed at the right moment.

---

## Phase 2: Telegram/Discord → `orch chat` (PR #731)

Control session can now receive messages from both Telegram and Discord channels.
Users can type natural language into their messaging app and get live system state
back — same context assembly as CLI `orch chat`. This closes the loop on the
control session feature from the retro.

---

## Operational Checks

1. **Stuck/failing tasks**: None. Three tasks in-flight, all normal age.
2. **Blocking**: `#728` is blocked by design (feature complexity, owner decision needed).
3. **Error patterns in logs**: Historical SIGTERM entries only — not a new issue.
4. **Retro follow-ups**: Channel routing smoke test still unverified. Not a bug, just not enough traffic to observe.
5. **Owner feedback**: None pending.

---

## Today's Priorities

1. **Channel routing smoke test** — third day pending. If no task routes organically
   today, create a test task and observe Telegram topic assignment.
2. **Review agent CI check** — `71369c0` added CI verification before approve.
   Monitor that this doesn't cause review timeouts on slow CI runs.
3. **Webhook mode** — consider re-enabling for sub-second GitHub event pickup.
   Currently at 45s polling fallback with no issues, but lower latency would
   help with rapid PR iteration cycles.
4. **`#728` project picker** — owner decision needed on whether to unblock. Complex
   feature, blocked for ~1 day.

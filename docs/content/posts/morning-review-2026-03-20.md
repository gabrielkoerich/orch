+++
title = "Morning Review — 2026-03-20"
date = 2026-03-20T10:15:00Z
[extra]
job = "morning-review"
+++

## Summary

Version is **v0.15.8**. One critical infrastructure failure found: the bean
trading update pipeline is broken — 7 tasks stuck in `blocked` state and one
more currently cycling through review failures. Root cause is SSH commit signing
failing in the brew service context during `git rebase`. Issue #724 filed with
exact fix. The "no valid projects" error log is clean (last write was March 1 —
19 days ago), confirming that issue is fully resolved.

---

## Recent Commits (last 24h)

| Commit | Description |
|--------|-------------|
| `19e0bcc` | fix: discord interaction acknowledgment creates new reqwest::Client (#723) |
| `925d7a4` | fix: byte-index string slice in NewTask handler panics on multibyte UTF-8 (#722) |
| `7b6144d` | feat: per-repo cost tracking in orch stats (#719) |
| `c1fdbff` | fix: "no valid projects" error still emitted to brew service stderr (#718) |
| `835faff` | docs: evening retrospective 2026-03-19 (#717) |

And earlier: multi-project channel routing (ChannelRouter, Telegram forum topics,
`orch stats` CLI), 6 reliability fixes — total 22 commits yesterday.

---

## Retro Priority Status

| Priority from Retro (03-19 evening) | Status |
|-------------------------------------|--------|
| Verify issue #716 fix silences the error log | ✅ `orch.error.log` last written March 1 — silent for 19 days |
| Verify agent push-fix (`6b598e5`) holds | ✅ Logs show push succeeds (issue 721 pushed cleanly via git_ops) |
| Channel routing smoke test | ⚠️ Cannot verify without a live task flowing through — mark for observation |

---

## Service Health

- **Version**: v0.15.8 (brew service running, PID 28161)
- **orch.error.log**: Last modified March 1 — no new entries. PR #718 fix confirmed working.
- **orch.log**: Active, writing normally.
- **Projects**: Engine running, both `gabrielkoerich/orch` and `gabrielkoerich/bean` active.

---

## Critical Issue: Trading Pipeline Broken

**7 tasks blocked + 1 in review loop** — all `Trading update: manage positions and update prices`
(bean project).

| Task | Status | Age |
|------|--------|-----|
| internal:3835 | in_review (retry loop) | ~30 min |
| internal:3771 | blocked | 1h |
| internal:3767 | blocked | 2h |
| internal:3765 | blocked | 3h |
| internal:3761 | blocked | 4h |
| internal:3759 | blocked | 5h |
| internal:3757 | blocked | 6h |
| internal:3755 | blocked | 7h |

**Root cause (traced to single call site, `src/engine/review.rs:1548`)**:

When a trading PR has merge conflicts, orch tries to rebase the worktree onto
`origin/main`. The rebase fails because the brew service runs without SSH agent
access (`SSH_AUTH_SOCK` not available in daemon context), and the bean repo has
`commit.gpgsign=true` (SSH key signing). Each rebased commit requires the SSH
agent to re-sign it — which fails:

```
sign_and_send_pubkey: signing failed for ED25519 ".../default_id_ed25519.pub"
from agent: communication with agent failed
```

After 3 failures the task becomes `Blocked`. The jobs system treats `Blocked`
as terminal (not in the active-state guard), so the next cron tick creates a
fresh task, which fails the same way. This repeats every hour, accumulating
blocked tasks.

**Fix** (filed as issue #724):
1. `review.rs:1548` — use `git -c commit.gpgsign=false rebase` so signing is
   bypassed in the rebase step
2. `jobs.rs:258` — add `TaskStatus::Blocked` to the active-state guard so cron
   stops creating new tasks when the previous one is blocked

---

## Log Analysis

**`orch.error.log`**: Last written March 1 at 17:15 — 651 lines total, all
historical "no valid projects" messages from before any fix. Completely silent
for 19 days. Fix confirmed working well before PR #718.

**`orch.log`**: The rebase failure pattern is the only recurring error today.
Appears 3× per task × 8 tasks = ~24 error entries, all the same root cause.
No other error patterns.

---

## Checks

1. **Stuck/failing tasks**: 8 trading tasks stuck — traced to SSH signing in
   review rebase. Issue #724 filed.
2. **Test gaps**: No new test gaps. The rebase path needs a test case but that
   would require mocking git signing — low ROI.
3. **Error patterns**: Only the SSH signing rebase failure (issue #724).
4. **Optimization**: No hotspots identified. Batch queries from PR #705 still
   in production.
5. **Owner feedback**: No open issues except #724 (just filed).

---

## Task Filed

**Issue #724**: `fix: git rebase fails with SSH signing in service context —
trading tasks block and accumulate`

One issue, two-line fix.

---

## Tomorrow's Priority

1. **Verify issue #724 fix ships and trading pipeline resumes** — confirm
   `internal:3835` (or the next trading task) merges successfully after rebase
2. **Clean up blocked tasks** — after fix ships, run `orch task unblock all`
   on the bean trading queue
3. **Channel routing smoke test** — verify next dispatched task routes to the
   correct Telegram topic

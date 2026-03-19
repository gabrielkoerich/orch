+++
title = "Morning Review — 2026-03-19"
date = 2026-03-19T09:35:00Z
[extra]
job = "morning-review"
+++

## Summary

Clean start. Service upgraded to v0.14.87 (restarted 09:26 UTC-3) with five
fixes from yesterday. The top retro priority — "no valid projects" stderr noise
— is now resolved. Bean `internal:3542` (trading scan) completed and merged
before this review ran. Zero open issues.

---

## Recent Commits (last 24h)

| Commit | Description |
|--------|-------------|
| `507dc7e` | fix: ci_merge_failures reset in store_reset_failure_counters makes MAX_CI_MERGE_FAILURES unreachable (#707) |
| `06b791e` | perf: batch store lookups in task list and use aggregate query for cost in task status (#705) |
| `fcebdd1` | fix: orch CLI prints fatal "no valid projects" error when run from worktrees without .orch.yml (#704) |
| `cadf502` | docs: evening retrospective 2026-03-18 (#703) |
| `44f44bf` | fix: mark task Done on approval when auto_close_task_on_approval=false (#701) |

---

## Retro Priority Status

| Priority from Retro (03-18) | Status |
|-----------------------------|--------|
| File "orch CLI no-project graceful degradation" task | ✅ Issue #702 already filed AND closed via PR #704 (`fcebdd1`) — fix live in v0.14.87 |
| Monitor bean internal:3496 (bean evening retro) | ✅ Completed — post published |
| Verify orch version after 5 merges | ✅ v0.14.87 confirmed, service restarted at 09:26 |

All three retro priorities resolved before this review ran.

---

## Service Health

- Service: running cleanly at **v0.14.87** (restarted 09:26, was 0.14.82)
- Projects: `gabrielkoerich/orch` ✅  `gabrielkoerich/bean` ✅ both connected
- Bean pipeline: `internal:3542` (trading scan) completed → PR 73 merged → worktree cleaned up
- No stuck or failing tasks

---

## Log Analysis

**`orch.error.log`** (brew stderr): The repeated "no valid projects" messages
observed in every prior retro **should be resolved** — PR #704 (`fcebdd1`) added
graceful degradation when the CLI runs in a worktree without `.orch.yml`. The
fix is live in v0.14.87. Will confirm at tomorrow's retro whether the stderr log
is now clean.

**`orch.log`** (service): One benign 422 warning on branch deletion after merge:
```
failed to delete branch after merge: GitHub API DELETE ... failed (422 Unprocessable Entity): {"message":"Reference does not exist"}
```
This is a known race condition — GitHub auto-deletes the branch on merge (if
"delete branch after merge" is enabled in repo settings), then orch tries to
delete it too and gets a 422. Harmless and not worth a task.

---

## Checks

1. **Stuck/failing tasks**: None found. All tasks in completed state.
2. **Test gaps**: No new gaps identified. Recent fixes include regression tests
   where applicable (dispatch_key test landed in prior session).
3. **Error patterns**: Only the benign branch-delete 422 and the (now-fixed) "no
   valid projects" pattern.
4. **Optimization**: PR #705 (batch store lookups) landed — task list and cost
   queries are now using aggregate queries. No further obvious hotspots.
5. **Owner feedback**: No open issues, no PR review comments requiring attention.

---

## Task Queue

No new tasks filed. All retro priorities resolved. No new patterns requiring
action.

---

## Tomorrow's Priority

1. **Confirm "no valid projects" stderr is clean** — verify `orch.error.log`
   after v0.14.87 has been running a full day to confirm PR #704 fully silences
   the noise.
2. **Monitor bean pipeline** — bean continues dispatching trading scans daily;
   verify the next scan completes end-to-end.

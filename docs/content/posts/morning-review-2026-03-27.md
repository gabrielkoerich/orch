+++
title = "Morning Review -- 2026-03-27"
date = 2026-03-27T10:35:00Z
[extra]
job = "morning-review"
+++

## Summary

Steady overnight activity: ~10 commits landed, continuing the review system hardening theme from
yesterday. Focus areas: response parsing correctness (removing over-broad keywords), test isolation,
sandbox enforcement, and counter/timing bugs. Service running at v0.37.11, CLI at v0.37.6
(mismatch — see below). 4 external tasks actively in progress. One recurring operational issue:
opencode agents are silently exiting 0 with no output, causing task failovers to claude.

---

## Recent Activity (Last 24h)

### Key Commits

- **Block git push in agent sandboxes** (#1086, 5d6c9cc) — agents could push directly to remotes,
  bypassing the engine's PR-based workflow. Now blocked at the tool level.
- **Remove ambiguous keywords from review inference** (#1091, e8c1979) — partial-match keywords
  in `infer_review_response_from_text` were causing false positives. Removed the ambiguous ones.
- **Recognize commit-completion phrases** (#1087, 43221d0) — `synthesize_response_from_text` now
  accepts commit-acknowledgment phrases as `done`, reducing false `needs_review` states.
- **DispatchGuard in tick.rs** (#1077, f95d028) — key leak on panic in the dispatch path, fixed
  by wrapping the dispatch with `DispatchGuard`.
- **push_failures counter fix** (#1095, d5166b5) — `response_handler.rs` was using
  read-increment-write instead of `store_increment` for `push_failures`, inconsistent with all
  other counters. Correctness fix.
- **jobs.rs last_run timing** (#1090, 379a777) — `last_run` was set in memory *before* execution,
  but the comment claimed this prevents catch-up on restart. SQLite is only updated *after*
  execution completes, so the in-memory pre-set was creating a silent inconsistency.
- **Test isolation broader scope filed** (#1081, be19d6b) — follow-up to #1079: the broader test
  isolation problem (all tests touching `~/.orch/state/`) was filed as a bug.
- **align review_system.md CI instructions** (#1085, 48c7b54) — CI step instructions in
  `review_system.md` diverged from `review_task.md`; synced.

---

## Operational Health

### Version Mismatch

```
CLI:     0.37.6
Service: 0.37.11  ✗ mismatch
```

Run: `brew upgrade orch && brew services restart orch`

This has been noted for two consecutive days. The mismatch means CLI behavior may differ from
service behavior — e.g., command parsing, output formatting. Low risk for routine operations but
worth fixing.

### opencode Exit-0 Failures

The logs show a recurring pattern: opencode agents exit with code 0 but produce no output
(`unknown error (exit 0): `). This is triggering fallback to claude:

- `internal:17505` attempt 1 — opencode → failover to claude
- `internal:17506` — opencode → failover to claude

Additionally, the router failed to parse a routing response from `opencode/minimax-m2.5-free`:
the response started with `{"type":"step_start",...}` rather than JSON, indicating opencode is
streaming non-JSON lines before the routing result. This causes a cooldown and falls back to the
next agent.

These are not new failures, but they're affecting internal task reliability. The empty-exit issue
is separate from the JSON parsing issue: one is opencode producing nothing, the other is opencode
producing streaming output the router doesn't expect.

### Active Pipeline

| ID     | Status       | Title                                                                        |
|--------|--------------|------------------------------------------------------------------------------|
| #1097  | in_progress  | ensure_pr_exists fallback push uses plain git without token auth            |
| #1096  | in_progress  | check_merged_prs ignores internal tasks — stuck NeedsReview internal tasks  |
| #1094  | in_progress  | cron normalize_dow maps "0-5" to "1-5" — Sunday silently dropped            |
| #1092  | in_progress  | review_poll.rs drops automated review comment when human also requests changes |

All tasks are actively dispatched. No stuck or blocked tasks visible.

---

## Retrospective Follow-ups (from 2026-03-26 evening)

- [x] **#1072 DispatchGuard** — merged via #1077
- [x] **Review prompt stability** — #1085 aligned CI instructions, #1091 removed ambiguous keywords.
      Continued churn but directionally improving.
- [ ] **#1039 follow-up** (stale remote refs before review agent) — #1039 was closed, no new issue
      filed. The fix (add `git fetch --prune` at review agent start) is still needed. Simple,
      high-impact.
- [ ] **#1076 broader test isolation** — bug filed (#1081) but not yet in the active pipeline.
- [ ] **CLI version drift** — still present (0.37.6 vs 0.37.11). `brew upgrade orch` needed.

---

## Today's Priorities

1. **File follow-up for #1039** (stale remote refs) — the evening retro flagged this as
   straightforward and high-impact. A `git fetch --prune` at review agent start prevents stale
   diffs and baseline checks. No issue currently exists for this.
2. **Monitor #1096/#1097** — internal task visibility (#1096) and HTTPS push auth (#1097) are
   correctness fixes that affect the core dispatch loop. Watch for clean merges.
3. **Resolve CLI version drift** — run `brew upgrade orch && brew services restart orch`.
4. **opencode empty-exit pattern** — not blocking, but worth monitoring. If internal tasks
   continue to fail over consistently, consider a targeted investigation or routing away from
   opencode for internal tasks.
5. **#1076 test isolation** — broader pass is filed (#1081) but not dispatched. If pipeline is
   light, pull it in.

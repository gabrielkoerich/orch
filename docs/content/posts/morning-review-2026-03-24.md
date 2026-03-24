+++
title = "Morning Review -- 2026-03-24"
date = 2026-03-24T08:00:00Z
[extra]
job = "morning-review"
+++

## Summary

Last 24h produced **40 commits** and a service restart onto **v0.26.0**. The big
event-bus, streaming, and router-pool changes from yesterday are live, and the service
came back up cleanly after restart. But the pipeline is no longer empty: **7 open GitHub
issues** were filed this morning for new regressions in router fallback, parser
robustness, review recovery, and runner diagnostics.

The main operational pattern is not broad instability - it is **state reconciliation at
the edges**. Several tasks have working fixes or sensible fallback behavior, but still end
up blocked because of push/auth failures, malformed-output parsing, or status drift after
merge.

---

## Recent Commits (last 24h)

| Commit | Issue | Description |
|--------|-------|-------------|
| `b99eeae` | #868 | fix: read router pool from YAML lists |
| `cd1030f` | #866 | feat: parse and format NDJSON stream output for human-readable `orch stream` |
| `9fc9f95` | #864 | feat: round-robin router LLM across multiple cheap/free models with safe fallback |
| `a872c7e` | #862 | feat: model pools per complexity tier with cooldowns |
| `c1b6030` | -- | fix: clarifying-question responses must report `blocked`, not `done` |
| `eaafb18` | #859 | fix: review tick + subscriber double-trigger no longer double-count failures |
| `965187f` | #860 | fix: review-agent tmux session cleaned up after rate limit |
| `7024cf1` | #852 | fix: recovered PR-create 422 no longer counts as a failure |

### Notable Themes

**Router pool rollout is live** - the cheap/free multi-model router landed and the service
now boots with a mixed pool (`opencode`, `kimi`, `claude`). This is the dominant change in
today's operational profile.

**Streaming got its second wave of hardening** - NDJSON is now formatted for humans and
stdout is streamed through tmux in real time, which makes live-session debugging much more
practical.

**Review-loop correctness improved** - yesterday's fixes around duplicate review triggers,
rate-limit cleanup, and recovered 422s are all in the recent commit set and directly reduce
false retries.

---

## Retro Priorities - Status

| Priority from 03-23 Retro | Status |
|---------------------------|--------|
| Monitor event bus stability | Partial pass - no panics or subscriber crashes in the morning logs after the v0.26.0 restart |
| Streaming NDJSON smoke test | Partial pass - stream formatting shipped and runner logs show NDJSON handling, but no manual `orch stream` validation observed in this review window |
| Channel routing smoke test | Still pending - no deliberate cross-project smoke test yet |
| Webhook re-enable | Still pending - service restarted in polling fallback mode |
| Shared auth-classifier test coverage | Still pending |

---

## Service Health

- **Version**: v0.26.0 after a clean 09:55 restart
- **Open GitHub issues**: 7 (`#873`-`#879`), all operational bugs filed this morning
- **Open task queue in this repo**: 6 blocked, 1 needs_review, 1 stale internal blocked task
- **Owner-feedback blockers**: None explicit; current blockers are automation failures or state drift

### Log Patterns

**Task #875 hit a push/auth failure despite a completed local fix** - logs show SSH auth
failing during push (`agent refused operation`), then the review path repeatedly tried to
create a PR and hit `No commits between main and <branch>`. This is an infrastructure/state
problem, not a new product bug, so no new issue was filed.

**`opencode` still has an opaque `exit_code=-1` failure mode** - `internal:8889` exited
with no stdout/stderr and succeeded only after failover to claude. That lines up with open
issue `#874` about missing diagnostics when runner startup fails.

**Router fallback is exercising the new pool in production** - this morning the router
recorded cooldowns for bad pool entries, including non-JSON/NDJSON responses and a timeout.
That behavior is better than total failure, but it also surfaced the new open bugs `#878`
and `#879`.

---

## Stuck Tasks

| Task | Status | Notes |
|------|--------|-------|
| `#875` | blocked | Fix appears committed locally, but push/auth failure prevented PR creation and left review recovery looping |
| `#877` | needs_review | Issue comments say the fix is complete and tests passed, but task state has not reconciled yet |
| `#873`, `#874`, `#876`, `#878`, `#879` | blocked | Freshly filed operational bugs; all failed twice and are waiting for follow-up runs |
| `internal:8068` | blocked | Evening retrospective task is still blocked even though PR `#854` merged successfully |

`internal:8068` is the most suspicious stale-state case this morning. The post merged at
23:26Z, but the internal task still shows `blocked` ~10 hours later. That suggests task
status reconciliation after merge is still leaky for at least one internal-task path.

---

## Operational Checks

1. **Are tasks stuck or failing repeatedly?** Yes - the current pattern is repeated edge-case
   failure after otherwise successful work: parser/output handling, router-pool fallback,
   review recovery, and push/auth handoff.
2. **Are there error patterns in logs?** Yes - SSH push/auth refusal, repeated PR-create 422
   follow-ups on `#875`, opaque `exit_code=-1` agent failures, and router pool entries that
   return NDJSON or timeout.
3. **Did the evening retro flag anything?** Yes - event bus monitoring, streaming smoke test,
   channel-routing smoke test, webhook re-enable, and shared auth-classifier tests. Only the
   first is showing early signs of stability; the rest remain open.
4. **Are tasks waiting on owner feedback?** Not directly. The queue is blocked by automation
   and state-management issues, not missing product decisions.

---

## Today's Priorities

1. **Clear the blocked issue queue (`#873`-`#879`)** - especially `#875` and `#877`, where the
   code may already be fixed but task state is wrong.
2. **Investigate stale status reconciliation for `internal:8068`** - merged PRs should not
   leave internal cron tasks blocked overnight.
3. **Harden router/output parsing around NDJSON and malformed JSON blocks** - the new pool is
   already surfacing real response-shape variability.
4. **Improve runner diagnostics for startup failures** - `exit_code=-1` with empty output is
   still too opaque for reliable auto-recovery.
5. **Run the long-pending channel-routing smoke test** - the codebase is stable enough that
   this should move from passive monitoring to an explicit check.

---

*No new GitHub issues were created during this review because the operational problems found
this morning are already represented by the current open issue queue.*

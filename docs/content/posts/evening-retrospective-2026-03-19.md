+++
title = "Evening Retrospective — 2026-03-19"
date = 2026-03-19T22:00:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Extremely productive day. Version advanced from **v0.14.87** → **v0.15.5** via
22 commits: 7 features and 7 fixes. The headline feature was the multi-project
channel routing system (ChannelRouter, Telegram forum topics, per-project stats)
— a large architectural addition that landed cleanly in one session. Six
reliability fixes closed out the day. One persistent issue: the "no valid
projects" brew service stderr noise was supposed to be fixed by PR #704 but is
still appearing — the fix only addressed CLI exit code, not the service's own
error path.

---

## Morning Priority Status

| Priority from Retro (03-19 morning) | Status |
|-------------------------------------|--------|
| Confirm "no valid projects" stderr is clean after v0.14.87 | ❌ Still emitting — fix was incomplete |
| Monitor bean pipeline (trading scans) | ✅ No failures observed |

---

## What Completed Today

### Multi-Project Channel Routing (feat, ~8 commits)

The biggest landing of the day. The orchestrator can now route notifications to
different channels (Telegram, Discord) per project, rather than broadcasting
everything to a single destination.

| Commit | What it does |
|--------|-------------|
| `6e9ce45` | ChannelRouter: project-to-channel mapping |
| `024b6f1` | DB migration: `repo` on `task_metrics`, `channel_subscriptions` table |
| `15397cc` | Telegram forum topic support with inline keyboards and callback queries |
| `59afd68` | `orch stats` CLI command with per-project breakdown |
| `7dc0de7` | Project context in task notifications |
| `f556a79` | Engine message routing with ChannelRouter and new commands |
| `27c6df0` | Integration tests for multi-project channel routing |
| `a94d2ce` | Docs: update channels runbook |

This is clean, well-tested, and matches the spec in `2c57aca`.

### Reliability Fixes

| Commit | Fix |
|--------|-----|
| `1618bb5` | TOCTOU race in `/stream` — panic when binding removed between check and use (#715) |
| `c34f02e` | Filter GitHub issues by author association — only ingest from trusted authors |
| `44f7280` | Immediate shutdown — reset `in_progress` → `routed` instead of draining |
| `362ffe2` | Rerun failed push-check workflow instead of dispatching a new one |
| `045f2f3` | Rename branch prefixes: `internal-` for internal tasks, `gh-issue-` for external |
| `6b598e5` | Agents commit only — orch handles push and PR creation |

The last fix (`6b598e5`) is noteworthy. Root cause: agents running inside the
sandbox lacked push permissions, failed silently, then reported `needs_review`
instead of `done` — which blocked orch from running its own push. The fix
updates the system prompt explicitly ("Do NOT push or create PRs — the
orchestrator handles both") and adds push-failure recovery in `response_handler`
(reroute up to 3 times, then block for human). Belt-and-suspenders check added
in `review.rs`.

### Docs

- `9c9dbdd` / `ee50c58`: Architecture docs consolidated, agent/orch
  responsibility split clarified.

---

## What Didn't Work

### "no valid projects" stderr still emitting (PR #704 was incomplete)

The morning review expected this resolved. It is not. The brew service stderr
(`/opt/homebrew/var/log/orch.error.log`) still emits the same message every
tick:

```
Error: no valid projects configured — all backends failed health checks.
Config: /Users/gb/.orch/config.yml. Run `orch init` to set up a project.
```

PR #704 fixed the **CLI** path (graceful exit code when running in a worktree
without `.orch.yml`). The **service** path — the daemon running from `/` via
brew — follows a different code path and still emits the error unconditionally.
This is a different call site from the CLI fix.

Root cause: service startup and the periodic tick run their own project-loading
path, which emits the error even when projects are configured and working
correctly. The projects ARE valid (engine is running fine); the error is a
spurious diagnostic from the wrong context.

This creates real signal-to-noise problems in the error log — a task is filed
below.

---

## Agent Prompt Review

The `prompts/agent_system.md` is in good shape after today's fix (`6b598e5`).
The "Do NOT push or create PRs" rule is now explicit. One observation: the
workflow section says to check `git log` before retrying, but the prompt does
not specify the review agent should check `prompts/review_system.md` itself for
protocol. That file exists and is correct; the cross-reference could be stronger
but this is not urgent.

No other prompt gaps identified.

---

## Routing Accuracy

All 7 fixes were routed to `agent:claude`. The channel routing feature (complex,
multi-file, stateful) was also `agent:claude`. Label routing via `agent:claude`
is being set on all these issues explicitly. The auto-router would likely have
reached the same conclusions — but we're bypassing it. This is fine given the
current task volume (a few per day), but worth noting if throughput grows.

---

## Performance / Reliability

No lock contention or sync bottlenecks observed. The batch store lookups from
PR #705 (yesterday) are in production and appear to have no regressions. The
immediate-shutdown fix (`44f7280`) improves restart behavior — tasks no longer
get stuck in `in_progress` after a service bounce.

---

## New Task Filed

**Issue #716**: `fix: "no valid projects" error still emitted to brew service
stderr despite PR #704 CLI fix` — trace the service-side code path and suppress
or demote the log level so it doesn't appear in error.log when projects ARE
configured.

*No other tasks filed. One issue, one root cause.*

---

## Tomorrow's Priority

1. **Verify issue #716 fix silences the error log** — confirm
   `orch.error.log` is clean after the patch ships.
2. **Verify the agent push-fix (`6b598e5`) holds** — watch the next task
   dispatch end-to-end to confirm agents no longer attempt push and the
   orch-side push succeeds on first attempt.
3. **Channel routing smoke test** — the ChannelRouter feature landed today;
   verify a real task notification routes to the correct Telegram topic.

+++
title = "Evening Retrospective — 2026-03-05"
date = 2026-03-05
description = "Auth hardening, CI improvements, PTY runner fallout, and observability gains"
+++

## Summary

Today was a focused **auth hardening and CI reliability** day. 17 commits landed — the headline themes are: shared `TokenResolver` singleton, dead `auth.rs` deletion, health-check error splitting, orch version in logs, and CI improvements (rust-cache, clippy `--all-targets`, gitleaks config). The PTY runner — merged in #386 — turned out to be fundamentally broken and is now tracked for removal in #416.

---

## Morning Review Recap

Morning review post was last written for 2026-03-03 (no separate post for 2026-03-04 or 2026-03-05). Carried-forward priorities were:

| Priority | Status |
|----------|--------|
| Investigate test failures | ✅ Addressed — clippy --all-targets CI change catches test-code warnings; if-let fix in cli_wrapper test |
| Merge and monitor PTY runner (#386) | ⚠️ Merged, but broken in production — #416 filed to remove it |
| Stuck threshold reduction | ❌ Not done — still pending |
| Monitor token resolver centralization (#378) | ✅ Resolved — TokenResolver singleton landed today |

---

## Tasks Completed Today

| Commit | Description | Impact |
|--------|-------------|--------|
| `e22be81` | Remove `gh issue develop` to prevent git config corruption | Stops subtle git config corruption in worktrees |
| `a2fe9e6` | Resolve GH_TOKEN via `gh auth token` at startup | Agents always have auth before first API call |
| `e0dbc3d` | Shared TokenResolver singleton for GitHub auth | Eliminates duplicate token resolution; closes #378 |
| `56041eb` | Split health check errors — auth vs network | Operators can now distinguish auth failures from connectivity |
| `461759e` | Include repo name in health check warnings | Clearer diagnostics when multiple projects monitored |
| `413ff0f` | Delete dead `auth.rs` and its dead callers | Removes ~200 lines of dead code; less confusion |
| `995cda0` | Remove unused `TokenResolver` import in agent.rs | Clippy hygiene |
| `42eb408` | CI: clippy `--all-targets` to catch test-code warnings | Catches warnings in `#[cfg(test)]` blocks that were silently ignored |
| `c531e43` | Docs: update clippy command in AGENTS.md | Agents now run the correct clippy invocation |
| `4cea0fa` | Use `if-let` instead of `is_some`+`unwrap` in test | Fixes clippy warning that `--all-targets` now surfaces |
| `421b6de` | Pass `ORCH_BUILD_VERSION` to build-macos via `needs: [check-release]` | Fixes version embedding in macOS release builds |
| `5b733da` | Include orch version in all serve log lines via root span | Every log line now carries the running version |
| `319d22a` | Use Swatinem/rust-cache with per-target cache keys in build-macos | Faster CI builds; avoids cross-target cache pollution |
| `b106abd` | Use Swatinem/rust-cache in build-macos (initial) | Cache consistency across CI jobs |
| `909f663` | Pass gitleaks config via `GITLEAKS_CONFIG` env var | Fixes invalid `--config` args that silently disabled secret scanning |
| `edf651e` | Embed version in span name instead of field | Cleaner log output; avoids redundant field duplication |

---

## What Didn't Go Well

### PTY Runner — Merged Broken (#416)

The PTY runner (#386) was merged and shipped, but turned out to be fundamentally flawed:
- Runs the agent in a **separate PTY outside tmux**, then tries to forward output via `tmux send-keys -l`
- `send-keys` sends keystrokes and cannot reliably stream large structured output
- Result: agents dispatch, produce no visible output, and completion detection fails

**Root cause**: the design solves a non-existent problem. Tmux already provides a PTY natively. The correct approach is the legacy runner (agent runs as the tmux session shell). The workaround is `runner.pty.enabled: false` in config; #416 tracks the full removal.

This is the clearest "merged-but-wrong" regression of the day.

### gitleaks Was Silently Disabled

`GITLEAKS_CONFIG` was being passed as a CLI arg instead of env var, causing gitleaks to silently ignore the config file. Secret scanning was effectively off. Fixed in `909f663`, but this went unnoticed for multiple cycles.

---

## Prompt Effectiveness

| Prompt | Assessment |
|--------|-----------|
| `prompts/agent_system.md` | Good — sandbox constraints and workflow steps are explicit. |
| `prompts/review_task.md` | Updated in morning review (2026-03-03) to remove `git fetch`; no new issues. |
| `prompts/route.md` | No issues surfaced today. |

No prompt changes needed. The 2026-03-03 alignment fix appears to be holding.

---

## Routing Accuracy

- Only 3 open issues today; routing looks clean.
- #416 (PTY runner) correctly routed to `agent:claude` given its complexity.
- No misroutes observed.

---

## Performance & Bottlenecks

- **CI speed**: Swatinem/rust-cache should meaningfully reduce build times; impact visible in next run.
- **Token resolution**: singleton replaces per-call `gh auth token` invocations — fewer subprocess spawns during high-traffic periods.
- **No lock contention** or API rate-limit events observed in today's commit set.

---

## New Issues Filed

Only 1 open issue beyond this retrospective:

| # | Title | Root Cause | Action |
|---|-------|-----------|--------|
| #416 | PTY runner broken | Fundamental design flaw — agent runs outside tmux | Remove PTY runner entirely; restore legacy runner as canonical |

No other new issues. The auth and CI fixes today were self-contained commits, not symptoms requiring separate issues.

---

## Tomorrow's Priorities

1. **Remove PTY runner (#416)** — this is the highest-priority reliability fix. Agents are currently broken unless users manually set `runner.pty.enabled: false`. The removal is well-scoped: delete `pty.rs`, promote `fallback.rs` to canonical runner, remove `portable-pty` dep.
2. **Reduce stuck detection thresholds** — `no_session_stuck_timeout` 600s → 300s, `stuck_timeout` 1800s → 900s. Low risk, high value for faster recovery. Still no open issue — check before filing.
3. **Verify gitleaks is now active** — confirm `GITLEAKS_CONFIG` fix works end-to-end in next CI run.

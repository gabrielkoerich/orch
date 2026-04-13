+++
title = "Evening Retrospective — 2026-04-13"
date = 2026-04-13
description = "Sprint record: 28 commits in 12h. Tick loop stall root-caused and fixed. Timeout coverage now nearly complete. CLI/service version mismatch detected. claude/opus at 50% success rate warrants investigation."
+++

# Evening Retrospective — 2026-04-13

## Summary

Sprint record day: 28 commits merged in 12 hours. The central achievement was identifying and fixing the 12-minute tick loop stall that paralyzed all Tokio workers — the root cause of the systemic stalls observed over the last several days. Alongside that, a sweep of remaining blocking I/O calls and missing timeouts brought timeout/async hygiene close to complete coverage. Two new features shipped: `orch session export` for cross-agent handoffs and `orch task inspect` for diagnostic access to running sessions. 175 tasks completed in the last 24 hours.

One operational problem surfaced: CLI and service versions are mismatched (CLI 0.67.7 vs service 0.67.9). Needs `brew upgrade orch` before next session.

---

## What was accomplished today

28 commits merged — highest single-day count of the sprint:

### Critical reliability fixes

| Commit | Issue | Description |
|--------|-------|-------------|
| `5dfa81bb` | #2574 | **Engine tick loop stall (12+ min)** — all Tokio workers paralyzed. Root cause identified and fixed. |
| `f24f734f` | #2575 | Silence detection bypassed when tmux session exits with seen-alias stub |
| `58bbd5f1` | #2581 | Issues created during engine downtime permanently skipped by ingest deduplication |
| `7fe38f1a` | #2597 | Review result discarded when stuck-task recovery races with review completion (13-min review wasted) |
| `57d7c690` | — | Dedup `continue` skips task ingest even when `update_status` fails |

### Timeout sweep (nearly complete)

| Commit | Issue | Description |
|--------|-------|-------------|
| `e390da2c` | #2591 | git fetch and gh pr create in review flow — no timeout |
| `8cb47804` | #2584 | Bash job type blocks tick loop indefinitely |
| `389b2c9a` | #2586 | git push/fetch in post-processing runner — no timeout (120s added) |
| `68d8591c` | #2585 | git push/fetch in auto_merge rebase recovery — no timeout |

### Async/blocking hygiene

| Commit | Issue | Description |
|--------|-------|-------------|
| `e8e142bc` | #2590 | Replace blocking `std::fs::write` with `tokio::fs::write` in WebhookStatus |
| `3fd5e632` | #2576 | Run `reconcile_startup_estimates` in background; skip terminal blocking at startup |
| `69a7d229` | #2579 | `sync_estimate_to_project` was blocking tick loop inline during routing |

### Performance

| Commit | Issue | Description |
|--------|-------|-------------|
| `61e9045f` | #2595 | `RouterConfig::from_config()` called per dispatch in `get_route_result()` — router timeout warning fired once per dispatched task |
| `69541fb1` | #2596 | GitHub label remove/add operations awaited inline — now fire-and-forget |

### Observability / correctness

| Commit | Issue | Description |
|--------|-------|-------------|
| `42020e9e` | #2563 | `get_check_runs` called twice on CI failure when `required_context` matches |
| `c82f2c38` | #2566 | `collect` pattern in `control.rs` loses individual error details |
| `7488294d` | #2567 | `kv_get_prefer_store` silently swallows database errors |
| `8f961c88` | #2569 | DB failure logging upgraded from `warn` to `error` in `sync.rs` |
| `1daff846` | #2562 | Failure when posting merge-conflict retry-limit comment silently dropped |

### Features

| Commit | Issue | Description |
|--------|-------|-------------|
| `9bbe03c1` | #2594 | `orch session export` — cross-agent handoff summary command |
| `9d4a8adc` | #2560 | Coverage report now shows per-file breakdown |
| `91d267a5` | — | `orch task inspect` — agent session diagnostics command |

---

## Morning priorities — status

| Priority | Status |
|----------|--------|
| Monitor codex re-entry on Apr 16 | Cooldown confirmed at 2d17h. Still cooling. No action needed. |
| Verify kimi recovery on Apr 15 | `cooldown:kimi` shows 1d7h remaining. On track for Apr 15 recovery. |
| Investigate claude/opus 52% rate | **Still at ~50% (13 success / 13 failed in 12h).** Needs tomorrow's deep-dive. |
| Confirm CLI version parity | **FOUND: CLI 0.67.7 vs service 0.67.9. Run `brew upgrade orch`.** |
| Audit rate_limit outcomes | Minimal today — only minimax/opus with 2 rate_limits. Not a current concern. |

---

## Agent health (12h snapshot)

| Agent | Model | Success | Failed | Rate |
|-------|-------|---------|--------|------|
| claude | sonnet | 77 | 34 | 69% |
| claude | opus | 13 | 13 | 50% |
| claude | (blank) | 11 | 13 | 46% |
| opencode | gpt-5-mini | 36 | 0 | **100%** |
| minimax | opus | 35 | 0+2 rl | 94% |
| opencode | minimax-m2.5-free | 19 | 1 | 95% |
| opencode | (blank) | 23 | 0 | **100%** |
| glm | opus | 6 | 0 | 100% |
| opencode | nemotron-3-super-free | 5 | 2 | 71% |
| opencode | copilot/claude-sonnet-4.6 | 0 | 3 | 0% |
| opencode | copilot/gemini-3.1-pro | 0 | 4 | 0% |
| opencode | copilot/gpt-5.4 | 0 | 5 | 0% |
| opencode | copilot/claude-opus-4.6 | 0 | 1 | 0% |

**Notable:**
- **opencode/gpt-5-mini and minimax-m2.5-free** remain the best-performing low-cost models. Carrying significant load.
- **claude/opus at 50%** — same signal as yesterday. Unclear if hard task mix or model degradation. Requires investigation tomorrow via `task_runs` error patterns.
- **claude/(blank) at 46%** — this is likely model-unresolved invocations; worth checking what model is being used when the model field is empty.
- **GitHub Copilot models** continue failing at 0%. Cooldowns are being applied (gpt-5.4 at ~2h). No new issue needed.
- **glm/opus** — new entrant showing 6/6 (100%). Promising.

---

## Active cooldowns

| Cooldown key | Remaining | Reason |
|---|---|---|
| `codex` | 2d17h | Billing cycle exhausted |
| `kimi` | 1d7h | Billing cycle |
| `kimi:haiku` | 46m | Same billing event |
| `glm:haiku` | 2h14m | Model cooldown |
| `opencode:github-copilot/gpt-5.4` | 1h59m | Silence detection |
| `opencode:opencode/nemotron-3-super-free` | 1h13m | Silence detection |

---

## What failed or needs attention

### 1. CLI/service version mismatch

CLI is `0.67.7`, service is `0.67.9`. This causes inconsistent behavior when using `orch` commands locally. Run before next session:

```bash
brew upgrade orch && brew services restart orch
orch version  # verify both match
```

### 2. claude/opus at 50% success rate (two days running)

Both yesterday and today, claude/opus sits at ~50% success. This may be:
- Hard task mix: opus is routed for `complexity:complex` tasks which are inherently harder
- Model degradation: genuine claude/opus quality drop
- Prompt issues: complex tasks have worse-structured prompts

Check tomorrow:
```bash
sqlite3 ~/.orch/orch.db "SELECT error, COUNT(*) FROM task_runs WHERE agent='claude' AND model='opus' AND outcome='failed' AND started_at > datetime('now', '-48 hours') GROUP BY error ORDER BY COUNT(*) DESC LIMIT 10;"
```

### 3. Tick loop stall root cause fixed — verify recovery

Today's fix (#2574) addresses the 12+ minute tick loop stall. The engine should now be responsive even when individual tasks block. Verify that tick latency has normalized by checking that tasks are dispatching at the expected 10s interval.

---

## Issues — none created today

All discovered problems are either:
- Fixed by today's commits (timeout gaps, stall root cause, race conditions)
- Already tracked in open issues (#2525 per-agent NDJSON parsers)
- Operational (cooldowns, billing — handled generically)

claude/opus 50% failure rate needs one more day of data before filing. Will revisit in tomorrow's review.

---

## Priorities for tomorrow (morning review)

1. **Fix CLI version mismatch first** — `brew upgrade orch && brew services restart orch && orch version`. Do this before anything else.

2. **Investigate claude/opus 50% failure rate** — Query `task_runs` for error patterns on opus failures. Determine if it's task complexity distribution or model degradation.

3. **Verify tick loop stall is resolved** — #2574 fix just merged. Confirm engine ticks are dispatching at expected 10s cadence with no stalls visible in logs.

4. **Monitor kimi recovery (Apr 15 ~06:32 UTC)** — `kimi:haiku` cooldown expires tonight (~46m from run time). Verify kimi begins routing again and check first few completions.

5. **Investigate claude/(blank) model field** — 46% success rate on runs where model is empty. Determine which model is actually being used in these invocations.

6. **Review blocked tasks** — 42 tasks blocked in last 24h. Audit for patterns: are these hitting max_review_cycles, CI failures, or agent loop detection? Prioritize by project.

---

Prepared by Orch automation (internal task internal:145175).

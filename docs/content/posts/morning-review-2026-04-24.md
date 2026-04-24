+++
title = "Morning Review — 2026-04-24"
date = 2026-04-24
description = "Daily operational check-in: 10 commits since yesterday (GitHub token fix, extended-backoff, transactions), GLM still rate-limited, watchdog stall observed, auto-merge SSH failures for bean project."
+++

# Morning Review — 2026-04-24

## Recent Commits (since 2026-04-21)

10 commits focused on correctness and resilience:

- `17812bde` fix: drop tmux session detached
- `69f9b567` fix(cron): normalize_dow handles degenerate range 0-0 correctly
- `5e9b82f7` fix(git_ops): fail loudly when auto-commit cleanup cannot unstage (#2920)
- `73b37c32` fix(silence): skip stale-generation entries and increase grace period to 5min (#2960)
- `41a940eb` feat(cooldown): add extended-backoff tier for persistently failing models (#2944)
- `0f78b48d` bug(runner): GH_TOKEN is injected after tmux process start, so agent never sees it (#2929)
- `b57c673f` bug(store): append_memory/recent_memory silently drop corrupt memory entries (#2928)
- `21372c1e` bug(security): scan() uses find() instead of find_iter(), missing multiple secrets per line (#2925)
- `39a70ade` fix(store): wrap status-change methods in transactions (#2900)
- `769a23f5` docs: morning review 2026-04-21 (#2893)

Notable changes:
- **GH_TOKEN fix** (`#2929`): agent runner was injecting `GH_TOKEN` as an env var set inside the tmux process, which meant it was never available to the agent subprocess. Should resolve git push failures that were caused by missing auth.
- **Extended backoff tier** (`#2944`): adds a new cooldown tier for persistently failing models — escalating from the standard backoff. Important for GLM's ongoing rate-limit issues.
- **Security scan fix** (`#2925`): `scan()` was using `find()` (first match only) instead of `find_iter()` (all matches), so multi-secret lines could leak.

## Operational Health

### Service and logs

- **Watchdog stall observed**: 80s stall at 10:06 UTC — routing took 55s (LLM budget exceeded + round-robin fallback), pushing the full tick over the 60s watchdog threshold. This is the same pattern as yesterday.
- **No evening retrospectives**: No evening retros found for 04-22, 04-23, or 04-24 — the evening task may have been skipped or failed on those days.
- **Error log is empty** (`/opt/homebrew/var/log/orch.error.log` is 0B).
- **Auto-merge SSH failures**: Both `internal:148542` and `internal:148554` (bean project) failed to rebase — `sign_and_send_pubkey: signing failed for ED25519` from agent. The `GH_TOKEN` fix (#2929) should help but doesn't address SSH agent key failures. This is a separate issue from git auth.

### Task/run health (24h)

Total runs: ~222 (from task_runs)
- **Success**: 185+ runs (claude/sonnet 70, codex/gpt-5.3-codex 27, minimax/opus 24, opencode/gpt-5-mini 22, claude/opus 15, etc.)
- **GLM (opus)**: 6 success, 3 failed, 3 rate_limit, 1 timeout — GLM is still struggling with rate limits and failures. The extended-backoff tier (#2944) should help here.
- **opencode/gpt-5.3**: 3 failures
- **opencode/gpt-5.4**: 3 failures
- **kimi/opus**: 2 failures, 1 rate_limit, 1 aborted, 1 push_failed — degraded state continues
- **parse_error**: 1 from claude/sonnet, 1 from opencode/gemini-3.1-pro-preview
- **aborted**: 2 from claude/sonnet

`task_activity` (12h): status_change (1579), dispatch (417), push (388), review_start (292), review_decision (280), error (198), branch_delete (121), routed (113), pr_create (105), rerouted (19), timeout (3).

Overall: healthy throughput with GLM and Kimi degraded.

## Stuck / Blocked Work

- **`#2789`** (blocked): GLM artifact collection. Still waiting.
- **`#2881`** (blocked): task_runs.error stores raw api_retry JSON fragments. Still needs fix.
- **`internal:148540`** (blocked): self-improvement task — review agent exceeded failure threshold. Needs human attention.
- **`internal:148556`** (blocked): twitter bookmarks research, blocked on codex.

Large batch of old Solana/oblivion/keeper tasks remain blocked from the Oblivion engine (all `CI failure limit (3) reached during auto-merge`). These appear to be old tasks that won't resolve without human intervention.

## Retro Follow-up Status (from 2026-04-21 morning)

1. **LLM routing budget timeouts**: Still causing watchdog stalls (80s today). The routing budget is still 45s — hasn't been tuned yet.
2. **Fix #2881** (task_runs.error JSON fragments): Still blocked, needs fix.
3. **GLM investigation** (#2789): Still blocked, pending artifact collection.
4. **Parse_error patterns**: Down to 2 in 24h — better but still occurring.
5. **`orch stream --pipe` validation**: No evidence of this being done.
6. **No evening retros** for the past 3 days — the evening task may need investigation.

## Tasks Waiting on Owner Feedback

- No open issues currently labeled `needs-feedback`.

## Priorities for Today

1. **Monitor GLM recovery**: Extended-backoff tier (#2944) was just deployed. Watch if GLM rate limits decrease.
2. **Investigate auto-merge SSH failures** for `bean` project — both `internal:148542` and `internal:148554` failed to rebase due to ED25519 key failures. The `GH_TOKEN` fix won't help SSH; separate root cause.
3. **Fix #2881**: task_runs.error JSON fragment storage — low effort, high value.
4. **Resolve evening retro gap**: 3 days without retrospectives. Check if the evening task is firing or failing silently.
5. **Consider tuning LLM routing budget** to reduce watchdog stalls — the 45s budget is causing 80s+ tick times.

## Issue Creation

No new operational issues. The main patterns (LLM routing timeout, GLM rate limits, SSH failures in bean) are either known or need more data before filing.

---
Prepared by Orch automation (internal task internal:148558).
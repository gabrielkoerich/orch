+++
title = "Evening Retrospective — 2026-04-12"
date = 2026-04-12
description = "Day 6 reliability sprint: 15 commits merged (sprint high), codex offline until Apr 16 (billing), kimi until Apr 15. claude/minimax/opencode carrying full load. 215 tasks completed. Same-agent loop detection working as designed."
+++

# Evening Retrospective — 2026-04-12

## Summary

Strongest commit day of the sprint: 15 merges, resolving three of the four priorities from this morning's review and closing several additional bugs discovered in transit. The system ran without service-level errors (`orch.error.log` is 0 bytes, unchanged since Apr 11 restart). 215 tasks completed in the last 24h, 82 in the last 12h. The main operational pressure point is capacity: codex billing is exhausted until Apr 16 and kimi until Apr 15, leaving claude, minimax, and opencode to absorb the full workload.

---

## What was accomplished today

Fifteen commits merged — all in the reliability sprint:

| Commit | Description |
|--------|-------------|
| `24f13f9e` | fix: per-task CI check cooldown to prevent GraphQL rate limit exhaustion (#2531) |
| `2d2cffe8` | fix(runner): populate task_runs.error when last_error is empty (#2530) |
| `ccea2d85` | bug: GitHub Actions billing failure indistinguishable from code failure — triggers CI retry loop (#2529) |
| `56656979` | fix: replace blocking Path::exists() with tokio::fs::try_exists() in resolve_repo_root_for_orphaned_worktree (#2523) |
| `ee3a4250` | bug: NeedsReview escalation in sync.rs writes block_reason and updates status in two separate calls (#2522) |
| `cd9836d0` | fix(control): use kv_list_prefix to escape LIKE metacharacters in memory keys (#2518) |
| `a1cbed29` | fix(review): suppress stale-outcome WARN when task blocked by CI failure (#2514) |
| `21ed33b6` | feat: add orch prune command to auto-clean orphaned worktrees (#2517) |
| `3ec62f6d` | bug: blocking Path::exists/is_dir calls in async code (#2498 fix) |
| `6c38371a` | bug: opencode + nemotron-3-super-free silent exit 0 before routing-fallback (#2524) |
| `f4e28370` | bug: router LLM timeout of 90s delays fallback when fast agent is available (#2480) |
| `b374a508` | fix(store): kv_increment return u64 to avoid truncation (#2509) |
| `a041341e` | bug: try_exists permission error treated as successful cleanup (#2506) |
| `8b98bcde` | fix: use orch_home/state instead of world-readable /tmp for legacy state (#2508) |
| `6d663b7c` | fix: escalate to needs_review on store failure in network retry handler (#2507) |

**Themes:**
- **CI correctness**: GitHub Actions billing failures now distinguished from code failures; GraphQL rate limits on CI checks now have per-task cooldown (#2531). Previously these caused runaway retry loops.
- **Async hygiene**: Two more blocking filesystem calls replaced with tokio async equivalents.
- **Atomicity**: NeedsReview escalation is now a single transactional write (was two separate calls, creating partial-failure window).
- **Observability**: `task_runs.error` field now populated on all failure paths; LIKE metacharacter escaping fixed in control memory lookups.
- **Security**: Legacy state moved from world-readable `/tmp` to `~/.orch/state`.
- **Operations**: `orch prune` command added to clean orphaned worktrees from CLI.

### Morning priorities resolved

| Priority | Status |
|----------|--------|
| Check cooldown state for failing GitHub Copilot models | **Confirmed working** — gpt-5.4, gemini, claude-sonnet-4.6 all have active cooldowns (~2h); system IS applying them after silence detection |
| Review blocked tasks #2478, #2480, #2467 | **#2480 and #2467 fixed and merged today**. #2478 (nemotron) addressed via #2524 (silent exit detection before routing-fallback) |
| Confirm CLI version parity | Not confirmed today — action carries forward |
| Audit rate_limit outcomes by model | Not addressed — action carries forward |
| Monitor kimi recovery | Kimi still cooled until Apr 15 06:32 UTC |

---

## What failed or needs attention

### 1. Codex billing exhausted — offline until Apr 16

`cooldown:codex` is set until `2026-04-16 16:50:00 UTC` (~89h from now). This is a billing_cycle_exhausted event — the generic cooldown system is handling it correctly. In the last 12h, codex contributed 20 successes vs 91 for claude/sonnet and 49 for opencode/gpt-5-mini. The workload is being absorbed but the reduced capacity is visible.

No action needed — do not manually clear this cooldown. Wait for billing cycle renewal or human operator intervention.

### 2. Same-agent loop detection working — but blocking tasks at scale

Multiple tasks are failing with "agent completed without code changes twice — same-agent loop detected, blocking for human review." This is the engine's loop-detection working as designed. However, it indicates a class of tasks that agents cannot currently solve — likely overly specific code-review tasks or tasks where the requested change is ambiguous.

The 36 blocked tasks in the DB include many from non-orch projects (trading/oblivion contracts) at max attempts. These require human review or task reformulation — not a system bug.

### 3. GitHub Copilot models still failing on re-entry

After short cooldowns expire (~2h), gpt-5.4, gemini-3.1-pro-preview, and claude-sonnet-4.6 via Copilot are re-routed and fail again. The silence detection → cooldown → re-entry cycle is working correctly, but the root cause (Copilot provider silently exiting) persists. Issue #2524 (silent exit before routing-fallback) should reduce the cost per failed run.

---

## Agent health (12h snapshot)

| Agent | Model | Success | Failed | Total | Rate |
|-------|-------|---------|--------|-------|------|
| claude | sonnet | 92 | 41 | ~136 | 68% |
| opencode | gpt-5-mini | 49 | 3 | 52 | 94% |
| minimax | opus | 41 | 1 | ~44 | 93% |
| opencode | minimax-m2.5-free | 28 | 1 | 30 | 93% |
| codex | gpt-5.3-codex | 20 | 0 | 22 | 91% |
| claude | opus | 17 | 15 | ~33 | 52% |
| opencode | (blank) | 19 | 1 | 20 | 95% |
| opencode | nemotron-3-super-free | 6 | 4 | 11 | 55% |
| opencode | github-copilot/gpt-5.4 | 1 | 7 | 8 | 13% |
| opencode | github-copilot/gemini-3.1-pro | 0 | 7 | 8 | 0% |
| opencode | github-copilot/claude-sonnet-4.6 | 0 | 5 | 8 | 0% |

**Notable:**
- **opencode/gpt-5-mini and minimax-m2.5-free** remain the strongest low-cost performers (~93-94% success). They are carrying significant load while codex/kimi are cooled.
- **claude/opus at 52%** is unusually low. May reflect harder task mix routed to opus (complex label), not model degradation. Monitor tomorrow.
- **GitHub Copilot models (gpt-5.4, gemini, claude-sonnet-4.6)** continue failing. Cooldowns are being set correctly (confirmed). No new issue needed.
- **nemotron-3-super-free** improved slightly (55% vs 48% yesterday). failure_count=4, short cooldowns cycling. The #2524 fix (routing-fallback before silent exit) should reduce wasted runs going forward.

## Active cooldowns

| Cooldown key | Expires | Note |
|---|---|---|
| `cooldown:codex` | Apr 16 16:50 UTC (~89h) | Billing cycle exhausted |
| `cooldown:kimi` | Apr 15 06:32 UTC (~55h) | Billing cycle |
| `cooldown:kimi:haiku` | ~55h | Same billing event |
| `cooldown:minimax:haiku` | ~33h | Model-level cooldown |
| `cooldown:opencode:github-copilot/gpt-5.4` | ~2h | Silence detection |
| `cooldown:opencode:github-copilot/claude-sonnet-4.6` | ~2h | Silence detection |
| `cooldown:opencode:github-copilot/gemini-3.1-pro-preview` | ~3h | Silence detection |
| `cooldown:opencode:opencode/nemotron-3-super-free` | ~15min | Short, will retry |

---

## No new issues created

All discovered problems are either:
- Handled generically (billing cooldowns, silence detection)
- Addressed by today's commits (#2480, #2467, #2524, #2531)
- Already tracked in open issues (#2525 — per-agent NDJSON parsers)

---

## Priorities for tomorrow (morning review)

1. **Monitor codex re-entry on Apr 16** — Do not pre-empt; the billing cycle renewal should auto-clear. Verify `orch cooldown list` on Apr 16 shows codex available.

2. **Verify kimi recovery on Apr 15** — Kimi should come back online ~06:32 UTC Apr 15. Check `orch cooldown list` and verify kimi begins getting routed tasks.

3. **Investigate claude/opus 52% rate** — If it persists tomorrow, check `task_runs` for error patterns. May be a hard-task-mix artifact or a model degradation signal.

4. **Confirm CLI version parity** — Still unverified from two days running. Run `orch version`.

5. **Audit rate_limit outcomes** — Still not addressed. Top priority for understanding codex/kimi exhaustion patterns:
   ```bash
   sqlite3 ~/.orch/orch.db "SELECT agent, model, COUNT(*) FROM task_runs WHERE outcome='rate_limit' AND started_at > datetime('now', '-24 hours') GROUP BY agent, model ORDER BY COUNT(*) DESC;"
   ```

6. **Review blocked tasks with max attempts** — 36 blocked tasks in the DB, most from trading/oblivion projects. Requires human review and task reformulation where agents cannot make progress.

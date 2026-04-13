+++
title = "Morning Review — 2026-04-13"
date = 2026-04-13
description = "Daily operational check-in: merged reliability fixes overnight, multiple PRs merged, continued cooldowns for billing events, and opencode handling strong."
+++

# Morning Review — 2026-04-13

## Recent Commits (last 24h)

The reliability sprint continued overnight with several bugfixes and fixes addressing async/blocking calls, silent-exit recovery, and CI/merge correctness. Notable commits (merged in last 24h):

- `8c53fcce` — bug: add timeouts for discord gateway connect and Hello frame (#2553)
- `1c04551c` — bug: wrap load_jobs() in spawn_blocking to avoid blocking Tokio thread (#2552)
- `ff8ad906` — bug: explicit error logging in response.rs output recovery (#2551)
- `f3ab4403` — bug: replace flatten() with explicit error logging in scan_skills_directory, prune, and doctor (#2547)
- `4c4de331` — bug: add .max(0) guards on i32-as-u64 casts in auto_merge.rs and response_handler.rs (#2546)
- `64471436` — fix: u64 token sum cast to i64 without saturation in control.rs (#2543)

Themes: robustness (timeouts, avoid silent failures), async correctness (spawn_blocking for blocking APIs), and defensive numeric casts.

---

## Operational Health

Overall status: service healthy. Multiple agent runs completed successfully overnight; auto-merge and cleanup pipelines active. Key operational notes below.

### Service

- Version: orch/0.63.8 (confirmed in logs)
- Recent logs show successful PRs, review runs, and cleanup. No persistent service-level errors observed in orch.error.log during this interval.

### Notable events

- Several PRs completed and auto-merged (example: #2553 — discord gateway timeouts) and corresponding worktrees removed.
- GitHub GraphQL usage remains high; engine is throttling when approaching limits (throttles observed, wait_secs reported in logs).

---

## Stuck / Blocked Tasks

Quick check of orch tasks shows:

- internal:142041 — this morning review task — in_progress (running in current worktree)
- 2525 — external blocked (runner: per-agent NDJSON parsers) — status: blocked, 10 tries

Additionally, CI failures have caused some tasks to escalate to blocked where CI failure limits were reached (example: internal:141079 blocked for human intervention after repeated CI failures).

Root causes observed:

- Billing cooldowns (codex, kimi) still in effect — capacity reduced but handled by cooldown logic.
- Silent-exit patterns for some GitHub Copilot models persist; system is setting cooldowns but providers remain unreliable.
- A small set of tasks hit CI failure limits and are blocked for human review.

---

## Logs & Error Patterns

- Recent orch log excerpts show multiple agent successes and a handful of silent-exit recoveries where the runner retried with a free model (opencode/minimax-m2.5-free) — fallback is working as intended.
- Repeated warnings about approaching GitHub GraphQL rate limits — engine throttles until reset. Recommend monitoring overall CI/GraphQL call volume if this persists.

---

## DB / Task Run Patterns (last 24h)

- Top recent task_run counts by agent/model/outcome show strong success for claude/sonnet and opencode/gpt-5-mini; failures concentrated in some Copilot models and a few opencode model permutations.
- Examples: claude|sonnet|success=144, opencode|github-copilot/gpt-5-mini|success=69, claude|sonnet|failed=59 — task mix varies by complexity.

---

## Retro Follow-ups (carried from evening retro)

- Codex billing cooldown until Apr 16 — no action; wait for billing renewal.
- Kimi billing cooldown scheduled — monitor for auto-recovery around Apr 15.
- GitHub Copilot provider failures: cooldowns are applied; root cause remains provider-side silent exits. Continue monitoring failure_count and cooldown entries.
- CI-failing tasks (reached CI failure limit) require human review before re-run. These are not service errors but indicate tasks requiring clarification or manual fixes.

---

## Priorities Today

1. Monitor cooldowns and agent recovery windows (codex Apr 16, kimi Apr 15). Use `orch cooldown list`.
2. Inspect blocked tasks hitting CI failure limits and add human-facing comments where needed (owner notification). Example: internal:141079.
3. Watch GraphQL rate usage. If throttling remains frequent, consider adding or tuning per-task CI cooldowns and reducing polling frequency temporarily.
4. Continue monitoring Copilot model failures; ensure cooldowns persist and investigate if cooldown expiry leads to immediate re-failure — file an issue only if cooldowns are not being set.

---

## Notes

- No new GitHub issues created during this review; current operational problems are tracked in existing issues (#2524, #2531, #2525). If a recurring provider problem shows cooldowns not being applied, create a focused issue on the generic cooldown mechanism rather than per-model fixes.

---

Prepared by Orch automation (internal task internal:142041).

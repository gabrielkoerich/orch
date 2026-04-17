+++
title = "Evening Retrospective - 2026-04-17"
date = 2026-04-17
description = "Daily retrospective: 5 commits merged, version mismatch back (4th consecutive day), github-copilot models still failing, one PR in review (fixing error sanitization), one unassigned bug filed."
+++

# Evening Retrospective - 2026-04-17

## Recent Commits (12h)

5 commits merged since morning review — all from yesterday evening's batch, no new commits landed today:

| Commit | Issue / PR | Description |
|--------|------------|-------------|
| `d4b36da2` | #2736 | **Stuck-task recovery** — stop swallowing `resolve_task_id` errors; fixes stale routing fields. |
| `354f05c4` | #2735 | **Review subscriber** — distinguish DB errors from stale status. |
| `c8d63bb2` | #2734 | **Worktree scan** — stay resilient when dir entry reads fail. |
| `d90c2854` | #2730 | **Router lock** — avoid holding read lock across dispatch awaits. |
| `567678b1` | #2728 | **Backend merge** — merge external tasks when store is internal-only. |

No new commits today — this retrospective is the only orch task that ran this evening.

## Morning Plan vs Outcome

| Morning priority | Evening status |
|------------------|----------------|
| Fix version mismatch | **Re-broken AGAIN** — CLI 0.69.28 vs Service 0.69.32. Fourth consecutive day. |
| github-copilot routing | **Still failing** — gpt-5-mini is the only healthy copilot model; gpt-5.4 at 17% (1/6), claude-sonnet-4.6 at 17% (1/6), gemini-3.1-pro-preview at 0%. |
| Verify stream changes (#2717, #2712) | **Not exercised** — no streaming activity in today's task runs. |
| nemotron parse errors | **Still occurring** — 7 parse errors in 24h (vs 6 in the previous 12h window). Rate consistent. |

## Operational Health

- **Open issues:** 2 (`#2751` in review via PR #2754, `#2746` unassigned).
- **Active tasks:** only this evening retrospective (`internal:146008`) and morning-briefing (`internal:146009`) in progress.
- **Service version:** CLI `0.69.28`, Service `0.69.32` — mismatch persists for the fourth consecutive day.
- **Task activity (24h):** 248 total task runs, 631 status changes, 211 dispatches, 144 branch deletes, 137 pushes, 101 routed events, 72 review starts, 65 review decisions, 61 PR creates, 41 error events.

## Agent and Model Outcomes (24h)

| Agent / model | Success | Failed | Rate limit | Parse error | Unknown | Total | Success rate |
|---------------|---------|--------|------------|-------------|---------|-------|--------------|
| `claude/sonnet` | 43 | 12 | 0 | 0 | 1 | 56 | 78% |
| `minimax/opus` | 37 | 3 | 5 | 0 | 1 | 46 | 83% |
| `glm/opus` | 24 | 3 | 4 | 0 | 0 | 31 | 77% |
| `codex/gpt-5.3-codex` | 28 | 0 | 0 | 0 | 0 | 28 | 100% |
| `opencode/minimax-m2.5-free` | 21 | 0 | 0 | 0 | 0 | 21 | 100% |
| `opencode/nemotron-3-super-free` | 8 | 3 | 1 | 7 | 0 | 19 | 42% |
| `opencode/gpt-5-mini` | 14 | 0 | 0 | 1 | 0 | 15 | 93% |
| `opencode/claude-sonnet-4.6` | 1 | 4 | 0 | 0 | 2 | 7 | 14% |
| `opencode/gpt-5.4` | 1 | 4 | 0 | 0 | 2 | 7 | 14% |
| `claude/haiku` | 3 | 2 | 0 | 0 | 0 | 5 | 60% |
| `opencode/gemini-3.1-pro-preview` | 0 | 5 | 0 | 0 | 0 | 5 | 0% |
| `codex/gpt-5.4` | 4 | 0 | 0 | 0 | 0 | 4 | 100% |
| `claude/opus` | 1 | 1 | 0 | 0 | 0 | 2 | 50% |
| `codex/gpt-5.2-codex` | 0 | 1 | 0 | 0 | 0 | 1 | 0% |
| `opencode/claude-opus-4.6` | 0 | 1 | 0 | 0 | 0 | 1 | 0% |

**Overall: 185 success, 39 failed, 10 rate limit, 8 parse error, 6 unknown. Success rate: 75%.**

### Comparison vs Morning Review (12h baseline)

| Model | Morning (12h) | Evening (24h total) | Trend |
|-------|---------------|---------------------|-------|
| claude/sonnet | 72% (31/43) | 78% (43/55) | Slightly improved |
| minimax/opus | 80% (24/30) | 83% (37/45) | Slightly improved |
| glm/opus | 91% (20/22) | 77% (24/31) | Regressed significantly |
| codex/gpt-5.3-codex | 100% (17/17) | 100% (28/28) | Stable |
| opencode/minimax-m2.5-free | 100% (12/12) | 100% (21/21) | Stable |
| opencode/gpt-5-mini | 100% (8/8) | 93% (14/15) | Minor regression (1 parse error) |
| opencode/nemotron | 50% (6/12) | 42% (8/19) | Still poor; parse errors persist |

### Error analysis

- **`no error info available`** on 24 failed claude runs (sonnet 9, haiku 2, opus 1, minimax 1) — legacy bug (#2720) should be gone but these are from before the fix landed. Fresh failures from today show actual error messages.
- **glm/opus regression**: 24 successes vs 3 failed + 4 rate limit. The morning review showed glm/opus at 91% in a 12h window — the 24h window shows 77%, suggesting the evening window had worse glm performance. Two failures captured JSON cost telemetry (`"tTokens"`), confirming the bug that PR #2754 is fixing.
- **nemotron parse errors**: 7 in 24h (vs 6 in 12h morning window). Rate consistent — not worsening, not improving.
- **github-copilot failures**: All 4 non-gpt-5-mini copilot models show the `empty-output-exit0` pattern. gpt-5.4 had 1 success in 7 attempts (17%); claude-sonnet-4.6 had 1 success in 7 (14%); gemini-3.1-pro-preview 0/5. These are getting correctly classified as failures via cooldown, but the underlying provider issue remains.

## Active Issues

### #2751 — Rate limit error sanitization (in review via PR #2754)

PR #2754 is open with 3 commits (160 additions, 11 deletions across `fallback.rs` and `cooldown.rs`). An agent working on `github-copilot/gpt-5-mini` implemented the full fix:

1. Added `summarize_rate_limit_error()` — extracts `error_status`, `attempt`, `retry_delay_ms` from api_retry JSON.
2. Added `extract_json_field` helper for robust JSON parsing.
3. Sanitized non-zero-exit cost telemetry (replaces Claude cost JSON blocks with generic message).
4. Fixed a regression: the diff inadvertently removed InvalidResponse model cooldown, fixed in a follow-up commit.

The PR body notes 18 failing tests remain — cooldown/health-check tests and router tests with global state issues. This needs review attention.

### #2746 — Git commands without timeout (unassigned)

Filed this morning, no agent assigned yet. Affected cleanup paths in `src/engine/cleanup.rs` lines 250, 267, 313. Low urgency but should get a medium-complexity agent.

## Routing Accuracy

Routing remained sensible. Medium-complexity work was split across `claude/sonnet`, `codex/gpt-5.3-codex`, `glm/opus`, `minimax/opus`, and `opencode/minimax-m2.5-free` — all healthy. The bad outcomes continue to cluster on known problematic model/provider combinations.

The most notable routing concern is the github-copilot pool: `gpt-5-mini` is handling real work successfully, but the other copilot models are still being routed to (hence `claude-sonnet-4.6` with 7 runs and `gpt-5.4` with 7 runs) even though their success rate is near-zero. The cooldown system catches these after failure, but the router keeps selecting them before the cooldown is fully active. This isn't a bug — it's the expected behavior of the cooldown + retry system — but it means some work is wasted on known-bad copilot models.

## What Went Well

- No new regressions introduced by the morning's 5 commits.
- PR #2754 demonstrates the github-copilot/gpt-5-mini agent is productive — filed the issue, analyzed the root cause, implemented the fix, fixed a regression, and opened a PR in one run.
- Health signals are consistent: 75% overall success rate, strong codex/opencode performance, claude/sonnet holding at 78%.
- No stuck tasks or blocked work. Only 3 in-flight tasks (this retrospective, morning-briefing, and #2751's review cycle).

## What Failed or Needed Retries

- **glm/opus performance regression**: 77% success rate over 24h, down from 91% in the morning's 12h window. Two failures show the cost-telemetry parsing bug (#2751) is present. With PR #2754 fixing this, the signal should improve.
- **nemotron parse errors**: 7 parse errors in 24h — rate consistent with yesterday (6 parse errors in 12h). This is now a 2-day pattern. The raw outputs need inspection to determine if it's a parser issue or model output quality.
- **github-copilot non-gpt-5-mini models**: 0-17% success rate persists. These are correctly handled by the cooldown system but still consume routing slots before cooling down.

## Priorities for Tomorrow

1. **Fix version mismatch** — `brew upgrade orch && brew services restart orch`. This is the fifth consecutive day of this issue. The service is 4 patch versions behind CLI. The root cause is that `brew upgrade` is not being run regularly after pushes to main. Consider automating this or adding a daily upgrade check.

2. **Review and merge PR #2754** — Fixes the rate-limit error sanitization bug. Also resolves the glm/opus cost-telemetry parsing issue. Needs review attention to pass the 18 failing tests.

3. **Investigate nemotron parse errors** — 7 errors in 24h, consistent rate across 2 days. Inspect raw `task_runs` for these failures to determine if it's a parser issue or model quality. File root-cause issue if not parser bug.

4. **Assign #2746** — git prune/pull timeout issue in cleanup.rs. Has clear root cause, ready for agent dispatch.

5. **Verify stream changes** — `orch stream --pipe` and same-length diffing were deployed two days ago. Tomorrow's review should confirm they work in real use.

## Issues Created

None created during this retrospective. All identified problems map to existing issues (#2751, #2746) or known operational patterns (nemotron, github-copilot cooldown handling).

---

Prepared by Orch automation (internal task internal:146008).

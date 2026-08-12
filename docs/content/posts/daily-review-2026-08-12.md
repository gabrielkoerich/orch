+++
title = "Daily Review — 2026-08-12"
date = 2026-08-12
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-08-12

## The headline: two more self-found-and-fixed diagnostic gaps, but the LLM router is quietly wasting ~2/3 of its "minimax" picks on a cooled model

Another quiet day operationally — no watchdog stalls beyond one self-recovered blip, no DB-lock errors, `orch.error.log` still empty. Two commits landed, both closing same-day-filed issues from a retrospective sweep: opencode's model-discovery cache no longer freezes for 71h+ on a failed refresh (#3508), and the rebroadcast-blocked-task recovery path now logs its two previously-silent early-return branches instead of going dark for days (#3507). Digging into the last 24h of routing logs for this review turned up a live, well-evidenced bug that hasn't been filed before: `route_with_llm`'s candidate filter only checks agent-level cooldown, not whether the model that will actually get resolved for the chosen complexity tier is cooled — so while `minimax:opus` sat cooled all day, the LLM classifier kept offering `minimax` as a candidate for medium/complex tasks and picked it repeatedly, needing a post-hoc reroute almost every time. Filed as a new issue below.

---

## What Shipped (Last 24h)

**2 commits landed:**

| Commit | Issue | Summary |
|--------|-------|---------|
| `72baaaeb` | #3507 | `auto_recover_rebroadcast_blocked_tasks` now logs `debug!` on both previously-silent early-return branches (`any_routable` gate, and the empty-candidates-after-age-filter path), so the next multi-day stuck task in this state is diagnosable from logs instead of a black box |
| `afe6bfa4` | #3508 | `update_discovered_models_cache()` now advances the cache guard timestamp even when discovery returns an empty result, fixing a bug where the 1h TTL was treated as perpetually expired the moment the cache first went stale — it was retrying the discovery subprocess on nearly every call for 71h straight instead of backing off |

**Closed today:** #3505 → #3507 (log-visibility fix), #3506 → #3508 (cache-timestamp fix). Both were filed and fixed same-day during yesterday's retrospective sweep (internal:156351), following the established find-root-cause-file-fix pattern.

**Still open:** #3453 (`bug(review-prompt): pending CI status prose still causes review parse errors`) — now 14 days old. Re-checked against current `HEAD`: `prompts/review_task.md` still has no explicit "pending CI is not terminal" instruction, still reproducible, correctly left open. This remains the single fastest way to empty the tracker — it's a one-paragraph prompt edit, not a code change.

---

## Operational Health

### Throughput and Activity

`task_activity` in the last 24 hours:

| Event | Count |
|------|------:|
| `status_change` | 181 |
| `dispatch` | 53 |
| `push` | 50 |
| `branch_delete` | 46 |
| `review_start` | 28 |
| `routed` | 25 |
| `review_decision` | 24 |
| `pr_create` | 24 |
| `error` | 3 |
| `timeout` | 2 |

Higher volume than yesterday's quiet 134/48/44 day — back to a normal-throughput day.

### Task Run Outcomes

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 31 |
| opencode | `opencode/deepseek-v4-flash-free` | `success` | 5 |
| opencode | `opencode/laguna-s-2.1-free` | `success` | 4 |
| opencode | `opencode/longcat-2.0-free` | `success` | 3 |
| opencode | `opencode/mimo-v2.5-free` | `success` | 3 |
| claude | `sonnet` | *(in progress)* | 2 |
| opencode | `opencode/nemotron-3-ultra-free` | `success` | 2 |
| codex | `gpt-5.4` | `failed` | 1 |
| opencode | `opencode/hy3-free` | `success` | 1 |
| opencode | `opencode/hy3-free` | `timeout` | 1 |
| opencode | `opencode/ling-3.0-tiny-free` | `success` | 1 |
| opencode | `opencode/nemotron-3-ultra-free` | `parse_error` | 1 |
| opencode | `opencode/nemotron-3-ultra-free` | `timeout` | 1 |

The `codex:gpt-5.4` failure is a review-agent run hitting `Model metadata for gpt-5.4 not found` — `failure_count:codex:gpt-5.4` is at 19 with a 6d9h persisted cooldown, i.e. the exponential model-level backoff is doing exactly what it's designed to do for a model that's been consistently unavailable. The opencode `timeout`/`parse_error` rows are single occurrences on free rotating models, self-recovered on retry — not a new pattern.

### Two ERROR-level review timeouts, both self-recovered

`internal:156311` (00:36 UTC) and `internal:156350` (16:38 UTC) both hit `review agent timed out`, were reset to `NeedsReview`, and completed on retry. Same shape as the single timeout noted in prior reviews — not a new pattern, no action needed.

### One WATCHDOG stall, single occurrence, self-recovered

`08:01:39 UTC`: "tick loop has not completed a tick in 90s (threshold 60s)". Context shows two tasks (`internal:156312`, `internal:156313`) routing and dispatching back-to-back in the same window — looks like a normal load spike, not a stall pattern. No repeat in the rest of the 24h window.

### `orch.error.log` still empty

0 bytes, mtime `Aug 9 19:15` — stale, unrelated to this window. Fourth clean day in a row on this front.

### Routing accuracy: `route_with_llm` repeatedly offers a fully-cooled agent as a candidate

17 "LLM selected cooled agent/model; rerouting to available agent" warnings in 24h — all `agent=minimax`, split `complexity=medium` (9) and `complexity=complex` (5, remainder split across other complexities). Every one was correctly caught by the existing sanity-check-and-reroute safety net and fell back to `claude` with no functional harm — but it means roughly 2 out of every 3 times the classifier reached for `minimax` on a medium/complex task, it picked a dead end.

Root cause, confirmed by reading the code: `route_with_llm()` (`src/engine/router/mod.rs:1199`) builds its LLM candidate list (`uncooled_agents`) by filtering only `is_agent_in_cooldown(agent)` and `is_agent_degraded(agent)` — pure agent-level checks. Every other routing path (`agent_is_routable`, used by round-robin and weighted round-robin) additionally calls `config.has_available_model_for_complexity(agent, complexity)`, which checks whether the *specific model* that would be resolved for that complexity tier is cooled. `route_with_llm` skips this because complexity isn't known until the LLM responds — but that means an agent whose configured model for every complexity tier is cooled still gets offered to the classifier every single time, forever, until the cooldown itself expires. `minimax`'s `medium`/`complex`/`review` tiers all map to `minimax:opus` (`~/.orch/config.yml`), which has been cooled continuously since well before yesterday's review (`failure_count:minimax:opus = 19`, cooldown currently 16h59m→ down from ~22h at review start) — so for the entire window `minimax` was a guaranteed-wasted candidate for those tiers.

Filed as a new issue (see below) — this is a routing-classifier scope gap, not a cooldown-duration problem, so it doesn't touch the settled cooldown/backoff mechanism.

### Backlog and stuck work

The two `review_cycles = 1` rebroadcast-blocked tasks flagged in the last two daily reviews (external IDs `490`, `493`) are **still frozen** — same `block_reason`, `needs_review_refires = 6`, `updated_at` unchanged at `2026-08-09T22:44:38Z`, now 3 days without a single reconciliation. Today's #3507 fix adds the missing log lines to the two silent early-return branches in the recovery sweep, which should finally show *which* branch is bailing on these two tasks the next time this is checked — holding off on filing a third issue until that diagnostic signal is actually available, since this exact bug class has already had five dedicated fixes (#3296, #3309, #3469, #3499, #3505) and another guess without new evidence would just restate what's already tracked.

The rest of the blocked backlog is unchanged: `GitHub Actions billing failure` blocks at merge time (correct per-task policy) and a handful of `max review cycles exceeded` / long-idle items in other tracked projects. Nothing new or stuck in this repo's own queue — this review's task and one concurrently-running same-tick task were the only `in_progress` items here.

---

## Issues Filed Today

**New:** `bug(router): route_with_llm candidate filter is agent-level only — agents whose only complexity-tier model is cooled still get offered to the classifier` — root-caused above, evidence is 17 reroutes in 24h all landing on a fully-cooled `minimax`.

No other issue met the bar to file: the two review timeouts and the WATCHDOG blip were single, self-recovered occurrences; the opencode free-model timeout/parse_error were single occurrences on rotating free models; the codex `gpt-5.4` failure is the existing exponential-cooldown mechanism working correctly; 490/493 are being held for next-review diagnosis now that logging exists.

---

## Priorities for Tomorrow

1. **Check whether `490`/`493` finally show a log line from #3507's new debug logging**, and use it to determine whether the `any_routable` gate or the age-filter path is the one bailing — that's the difference between "review agents genuinely aren't routable for that repo" and "the recovery sweep just isn't reaching these two tasks."
2. **#3453 remains the single open issue, now 14 days old, still reproducible on `HEAD`.** Still a one-paragraph prompt edit away from an empty tracker.
3. Watch for a fix landing on the new `route_with_llm` candidate-filter issue — the pattern will keep recurring for any agent whose only resolvable model goes into a multi-hour+ cooldown, not just `minimax`.

---

*Prepared by Orch automation (internal:156384) on 2026-08-12.*

+++
title = "Morning Review — 2026-04-09"
date = 2026-04-09
description = "Reliability run continues: 10 more commits since evening retro, codex fully recovered, #2254 approved and pending CI merge. CLI/service drift is back (0.60.123 vs 0.60.131). No open issues. Clean pipeline."
+++

# Morning Review — 2026-04-09

## Recent Commits & Progress

Yesterday was a record 24-commit day. Since the evening retrospective, 10 additional commits landed:

- **`2aced04a`** fix: log warn for non-NotFound I/O errors in `load_dedup_file` (#2258)
- **`8f3dd0eb`** bug: `start_run` and `complete_run` audit trail silently drops all DB errors (#2259)
- **`59ea737a`** bug: `get_mentions` silently drops mentions on parent-issue fetch failure — no observability (#2257)
- **`35cdd008`** refactor: move `NoopBackend` test helper to dedicated `test_helpers.rs` (#2252)
- **`b25c3d11`** bug: `parse_success_output` only checks `result` field — misses `AgentResponse` JSON in earlier NDJSON messages (#2251)
- **`a0728c14`** bug: write `block_reason` before blocking in `review.rs` and `review_poll` (#2250)
- **`24daca1f`** bug: `set_cooldown_async` returns `true` despite KV write failure — violates contract (#2249)
- **`76c67939`** bug: `wait_for_cooldown` returns misleading error when agents are degraded (#2248)
- **`c9debae0`** test: fix `router_round_robin_routes_task` broken by `model_map` change
- **`5c3a9770`** bug: mergeability deferral resets approved PR to `NeedsReview` (#2247)
- **`8fd56391`** bug: `next_round_robin_agent` skips cooldown check but ignores degraded state (#2246)
- **`6fdf5377`** bug: `handle_review_changes` writes `block_reason` twice with inconsistent data (#2245)

The reliability push has now been running for two full days with no sign of slowing. Focus remains on correctness: audit trail gaps, misleading error contracts, silent data drops at DB and observability boundaries.

---

## Operational Health

**Overall: healthy.** Pipeline is processing work, review automation is functioning, no blocked tasks visible. One minor structural concern: CLI/service version drift.

### Live concerns

1. **CLI/service version mismatch**

   ```
   CLI:     0.60.123
   Service: 0.60.131  ✗ mismatch (8 versions behind)
   ```

   The evening retro flagged this as a priority but upgrade wasn't confirmed. With 10 more commits since then, the gap is now 8 versions. Run:

   ```bash
   brew upgrade orch && brew services restart orch && orch version
   ```

2. **#2254 in review, waiting for CI**

   Task `2254` (`bug: next_round_robin_agent ignores model availability`) went through a full automated review cycle this morning:
   - First review (opencode/nemotron) → RequestChanges (fallback branches still bypass model availability check)
   - Agent re-dispatched (opencode/minimax-m2.5-free) → completed fix, pushed to PR #2260
   - Second review attempt (opencode/nemotron) → "Provider returned error" → reset to `needs_review`
   - Third review (minimax/opus) → **Approved**
   - Status: `in_review`, CI pending (1 of 2 checks passing at last check)

   The automated loop is working. No human intervention needed — just waiting for CI to go green and auto-merge to fire.

### What looks healthy

- **No blocked tasks.** Only 1 external task in the queue (`2254`, actively progressing).
- **No active cooldowns.** KV cooldown table is empty — all agents are routable.
- **Codex is fully recovered.** Yesterday's retro noted the cooldown expires Apr 9. This morning: 40 successes in 24h, confirming full recovery.
- **Router LLM pool exhaustion (#2183) not observed.** No "router pool exhausted" log entries this morning. Yesterday's #2222 (router LLM skips degraded agents) appears to have reduced the trigger frequency, or load was simply lower this morning.
- **Review automation is end-to-end functional.** Three successive review agent invocations on #2254 this morning — one failed ("Provider returned error"), retried cleanly with a different agent, approved.

### 24h run outcomes

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 72 |
| minimax | opus | success | 48 |
| codex | gpt-5.3-codex | success | 40 |
| opencode | github-copilot/gpt-5-mini | success | 15 |
| claude | sonnet | failed | 13 |
| claude | haiku | success | 10 |
| claude | opus | success | 10 |
| kimi | opus | success | 8 |
| opencode | github-copilot/gpt-5.4 | success | 8 |
| opencode | opencode/nemotron-3-super-free | success | 8 |
| claude | haiku | failed | 7 |
| minimax | opus | failed | 7 |
| opencode | github-copilot/gpt-5-mini | failed | 6 |
| kimi | opus | rate_limit | 4 |
| opencode | opencode/qwen3.6-plus-free | failed | 2 |
| olm | gemma4 | success | 2 |
| olm | — | failed | 3 |

Notes:
- `olm` (gemma4) appears in run stats for the first time — a new agent/model is being exercised.
- qwen3.6-plus-free failures are down to 2 (from 10 yesterday) — yesterday's #2228 (ModelUnavailable classification) appears effective.
- kimi rate limits (4) are minor; exponential backoff is handling them generically.
- `opencode/nemotron-3-super-free` showed "Provider returned error" in review (handled by retry), but also 8 successes in other runs.

### Last 12h task activity

| Event | Count |
|-------|-------|
| status_change | 1299 |
| dispatch | 391 |
| push | 296 |
| branch_delete | 226 |
| routed | 180 |
| review_start | 170 |
| review_decision | 153 |
| pr_create | 108 |
| error | 84 |
| rerouted | 55 |
| timeout | 3 |

Error volume (84 in 12h) is similar to yesterday and consistent with a high-throughput pipeline where transient failures are expected and retried.

---

## Retro Follow-Ups

| Priority from Apr 8 retro | Status |
|---------------------------|--------|
| Check CLI/service version sync | **Open** — gap is now 8 versions (0.60.123 vs 0.60.131). Needs upgrade. |
| Verify codex recovery | **Done** — 40 successes in 24h. Fully recovered. |
| Confirm router LLM pool exhaustion resolved or create targeted fix | **Tentatively OK** — no exhaustion events in today's logs. #2222 may be sufficient. Monitor. |
| Verify qwen3.6 cooldown now applied (#2228) | **Improving** — down from 10 to 2 failures in 24h. #2228 appears effective. |
| Monitor agent response metadata recovery (#2220) | Ongoing. No regressions observed. |

---

## Priorities for Today

1. **Upgrade CLI/service** — 8-version gap. Run:

   ```bash
   brew upgrade orch && brew services restart orch && orch version
   ```

2. **Wait for #2254 to merge** — already approved, CI pending. No action needed unless CI fails or auto-merge stalls.

3. **Investigate `olm` agent** — appears in run stats for the first time with gemma4. Three failures, two successes. If this is a new agent integration being tested, watch for failure patterns.

4. **Confirm router LLM exhaustion stays quiet** — no events this morning, but check again after the service upgrade in case behavior changes.

5. **Watch kimi rate limits** — 4 rate limit events in 24h is minor, but worth monitoring to confirm the exponential backoff is preventing cascading failures.

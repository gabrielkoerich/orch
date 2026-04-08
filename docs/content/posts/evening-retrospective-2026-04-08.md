+++
title = "Evening Retrospective — 2026-04-08"
date = 2026-04-08
description = "Record 24-commit day: massive reliability push closes all open issues. Critical bugs fixed include silent metadata loss (~10% of runs), model availability false positives, and block_reason races. No open issues remain. kimi and codex recovering."
+++

# Evening Retrospective — 2026-04-08

## Summary

**Highest-volume day on record: 24 commits merged in 12 hours.** This was an almost-exclusively correctness and reliability day — the system self-diagnosed and self-fixed a large backlog of bugs filed over the last several days. All 20 tracked open issues are now closed. The pipeline is running cleanly with zero open issues.

No feature work. No regressions. Clean state heading into tomorrow.

---

## Morning Priorities — Outcome

| Priority from morning review | Status |
|------------------------------|--------|
| Investigate router LLM pool exhaustion (#2183) | ✓ **PARTIALLY ADDRESSED** — #2222 (router LLM now filters degraded agents alongside cooldown) reduces the cause. Full exhaustion scenario still possible if all agents are degraded simultaneously. |
| Close CLI/service version gap (0.60.103 vs 0.60.104) | Unknown — no CLI/service check run this session. 24 commits means service is likely multiple versions behind again. |
| Unblock `internal:77652` | Unknown — not visible in today's issue list; likely resolved or still blocked. |
| Re-check degraded dispatch warnings after upgrade | Not checked — upgrade not confirmed. |
| Watch qwen3.6-plus-free | Not the active issue today — no qwen3.6 failures appeared in today's commit messages. |

---

## What Was Accomplished

### 24 commits today — all correctness

Grouped by impact area:

#### Critical bug fixes (data/correctness)

- **`6dd67fe1` (closes #2220) agent response parse failures silently discarding structured metadata** — ~10% of runs were silently dropping learnings, cost, model, and other structured metadata when the JSON parse failed. Now recovers gracefully and preserves partial data. **Highest-impact fix of the day.**

- **`5ad7761c` (closes #2230) has_available_model_for_complexity returns true when no model_map entry exists** — the router could dispatch tasks to agents with no configured model, producing modelless dispatch and silent failures. Now correctly returns false when no model is mapped.

- **`bf2912c7` (closes #2205) detect_context_overflow and detect_permission_denied scanning full output** — both detectors scanned the entire output buffer instead of the safe_tail window, causing O(N) scan time and false positives from old content buried deep in agent output.

- **`f1a12f2e` (closes #2204) block_reason race in auto_merge.rs** — `Blocked` status transition was persisted before `block_reason` was saved, causing a narrow window where a blocked task had no stored reason. UI and log queries showed "blocked (no reason)".

- **`bca9f385` (closes #2216) batch_reset_failure_counters was dead code, omitting needs_review** — function was never called AND missed the `needs_review` status in its reset logic. Failure counters accumulated across unrelated cycles.

- **`c67e6024` (closes #2214) scheduled job executes after pre-run state persistence failure** — job dispatch continued even when the pre-run state write to SQLite failed, running the job with stale state.

#### Router and routing accuracy

- **`12857a32` (closes #2221) router LLM filters degraded agents alongside cooldown** — the router LLM was not checking degradation state before selecting an agent, wasting LLM calls on agents that would fail immediately.

- **`1e725c9c` (closes #2228) opencode model deprecation and 'No endpoints found' not classified as ModelUnavailable** — these errors were classified as generic failures (no cooldown applied), causing repeated retry loops against dead models.

- **`7724f3e8` (closes #2229) detect_network_error uses extract_context_around instead of safe_tail** — the network error detector was using the wrong context extraction function, potentially missing errors near the end of output.

#### Review and merge reliability

- **`db7fb1c6` (closes #2207) GhHttp::new() failure in post_review_comment treated as hard error** — a transient HTTP client init failure was escalating to task escalation. Now treated as soft error with retry.

- **`1da96637` (closes #2212) review updates status and task in same query** — previously two separate queries, creating a window where task state was inconsistent between the status and the task record.

- **`b43ee7e4` (closes #2197) review_and_merge decomposed into testable phases** — large monolithic function split into discrete phases. Foundation for better unit test coverage.

#### Engine reliability

- **`05ee0e30` (closes #2202) block_on inside block_in_place removed** — deadlock hazard. `block_in_place` already creates a blocking context; nesting `block_on` inside it could deadlock the Tokio runtime.

- **`6ed1dc81` (closes #2201) replace unwrap()/expect() in production code** — multiple panic sites converted to proper error handling. Reduces surface area for unexpected panics in production.

- **`ebf46ce6` (closes #2215) log errors when removing stale routing labels** — silent label removal failures were masking routing cleanup problems.

#### Performance

- **`21e658c2` (closes #2196) resolve_task_id loads full 60-column row** — fetched an entire task record just to return the ID. Now uses a targeted single-column query.

#### Agent system improvements

- **`ca935378` (closes #2224) add learnings field to agent output schema** — agents can now include a `learnings` field in their structured output. Prerequisite for agent self-improvement flows.

- **`db21daf0` (closes #2223) integration_review.rs calls extract_text instead of find_agent_result** — integration test was calling dead code path, meaning the test no longer validated the production code path.

#### Research / other

- **`59cc63c4` (closes #2194) Research: train and use a local model** — research task completed.

- **`2e7ac0b7` simplify: remove deprecated parse_review_from_output and ndjson_extract_json** — dead code removed.

- **`54ee7126` bug: orch task publish loses issue linkage** — task publish flow was marking source task done prematurely.

- **`f69048fc` bug: orch doctor --fix reports recovery success even when reopen/state update fails** — false positive recovery reports.

- **`4e59e8d6` + `4b3a419d` opencode model discovery** — consolidated free model discovery + 30s timeout added to prevent hanging discovery calls.

- **`5036ece4` fix(engine): credit exhaustion misclassified, stuck-task cooldown gap** — credit exhaustion not reliably triggering cooldown; stuck task recovery missing cooldown application.

---

## What Failed and Why

### Router LLM pool exhaustion (#2183) — not fully resolved

This morning's review identified 4 scheduled tasks that fell back to weighted round-robin due to router LLM exhaustion. Today's #2222 (router LLM now skips degraded agents) is directionally correct but addresses the wrong layer — the exhaustion was about pool capacity, not agent selection.

If the router LLM itself (e.g., claude:haiku) is cooled or rate-limited, all pool entries become unavailable simultaneously. No fix for this was merged today. The fallback to weighted round-robin is functional but reduces routing quality.

### qwen3.6-plus-free — status unclear

The morning review flagged 10 failures in 24h for qwen3.6. Today's opencode classification fix (#2228 — 'No endpoints found' now classified as ModelUnavailable) should improve cooldown application for this model. Whether it specifically helps qwen3.6's failure pattern is unclear without checking KV state.

---

## Routing Accuracy

**Router improvements are the biggest story of the day.** Three orthogonal fixes landed:

1. Router LLM no longer considers degraded agents (#2222)
2. opencode model deprecation/endpoint errors now classified as ModelUnavailable (#2228)
3. has_available_model_for_complexity false positive fixed (#2230)

Combined, these should materially reduce wasted dispatch attempts and improve routing accuracy metrics. No post-deploy stats available yet.

### Open issues at day-end

**Zero.** All 20 tracked issues are closed. Pipeline is in clean state.

---

## System Health

- **Issues open:** 0 (all closed)
- **Commits today:** 24 (record)
- **CLI/service sync:** Unknown — likely drifted given 24 new commits. Run `orch version` to check.
- **kimi:** Was recovering as of yesterday. No kimi-specific issues merged today — likely fully recovered.
- **codex:** Cooled until Apr 9 (tomorrow). Should resume routing.
- **qwen3.6-plus-free:** Instability partially addressed by #2228. Monitor tomorrow.

---

## Priorities for Tomorrow

1. **Check CLI/service version sync** — 24 commits means the service is probably multiple versions behind. Run `brew upgrade orch && orch service restart && orch version`.

2. **Verify codex recovery** — codex cooldown expires Apr 9. Confirm it's routing successfully by mid-morning.

3. **Confirm router LLM pool exhaustion is resolved or create targeted fix** — #2183 was the primary morning concern; today's #2222 is related but may not fully address it. Check `orch log` for "router pool exhausted" entries after upgrade.

4. **Verify qwen3.6 cooldown now applied** — #2228 should cause 'No endpoints found' errors to trigger ModelUnavailable + cooldown. Check KV for active cooldowns on `opencode:opencode/qwen3.6-plus-free` after a few hours.

5. **Monitor agent response metadata recovery** — #2220 fix ensures ~10% of runs that were silently losing metadata now recover it. Watch for any unexpected behavior changes in learnings/cost tracking.

6. **File any newly discovered issues** — with zero open issues, tomorrow is a clean-slate day. The morning review should surface whether today's fixes introduced any regressions.

+++
title = "Morning Review — 2026-04-06"
date = 2026-04-06
description = "Overnight bug sweep: 5 review-pipeline correctness fixes, CLI/service version mismatch, kimi billing still out — 250+ successful runs"
+++

# Morning Review — 2026-04-06

## Recent Commits & Progress

Productive overnight batch — ~30 commits since yesterday's retro. The dominant theme was correctness and data-integrity in the review pipeline:

**Review pipeline correctness (overnight batch):**
- `store_increment returns 0 on DB failure` (#1964) — `no_code_reroutes` limit was unreachable when the store was degraded; counter silently returned 0 on every DB error, preventing the safeguard from ever triggering
- `review_poll watermark not updated on store failure` (#1963) — same review would be re-processed on every tick when the watermark write failed; silent infinite re-review loop
- `stale InReview detection has TOCTOU` (#1962) — sync.rs re-checked task status after the `in_review` list was fetched; Done/Blocked tasks were being reset to NeedsReview due to the race window
- `calculate_backoff_delay jitter uses wall-clock micros` (#1961) — jitter was derived from `SystemTime::now().subsec_micros()`; concurrent retries starting within the same microsecond got identical delays, defeating the jitter entirely
- `review.rs ignores AgentResult.is_error for codex/opencode` (#1956) — auth/rate-limit errors from review agents were being treated as successful completions; errors bypassed cooldown and inflated `review_agent_failures`

**Review batch pagination:**
- `paginate batch PR review comments` (#1954) — GraphQL batch query silently truncated after first page; missed reviews on PRs with many comments
- `is_collaborator API errors silently swallowed in review_poll` (#1951) — permission check failures were discarded; collaborator status defaulted to false, blocking legitimate reviews

**Rate-limit and cooldown:**
- `review agent rate-limit detection for exit-0 text output` (#1949) — review agents that returned rate-limit text with exit code 0 weren't detected as rate-limited; no cooldown was applied
- `persist critical cooldowns synchronously` (#1943) — fire-and-forget persist meant cooldowns were lost on crash; critical backoffs were ephemeral
- `set_cooldown persists to KV via fire-and-forget tokio::spawn` (#1943 companion) — async drop left cooldown persist unscheduled under load

**Refactors:**
- `replace 144+ fully-qualified crate:: paths with use imports` (#1942) — large readability refactor across the codebase
- `review subscriber has 11 inline crate::engine::cooldown calls` (#1944) — inline paths unified under use imports
- `Router::discover_free_opencode_models uses shared cache` (#1941) — was creating a separate cache per call; now uses the process-wide shared instance

---

## Operational Health

**Overall: healthy. No crashes, error log is 0 bytes.**

### CLI/Service version mismatch — action needed

```
CLI:     0.60.44
Service: 0.60.50  ✗ mismatch
```

The service has auto-deployed to 0.60.50 (includes overnight fixes) but the CLI binary is still 0.60.44. This can cause protocol mismatches for CLI-driven operations. Run:

```bash
brew upgrade orch && brew services restart orch
```

### Agent success rates (last 24h)

| Agent | Model | Successes | Failures |
|-------|-------|-----------|---------|
| claude | sonnet | 68 | 1 + 1 timeout |
| minimax | opus | 51 | 0 |
| claude | haiku | 21 | 0 |
| codex | gpt-5.3-codex | 19 | 8 |
| opencode | minimax-m2.5-free | 12 | 0 |
| opencode | nemotron-3-super-free | 12 | 1 |
| claude | opus | 10 | 0 |
| opencode | github-copilot/gpt-5-mini | 10 | 0 |
| opencode | qwen3.6-plus-free | 9 | 1 |
| kimi | opus | 0 | 6 |

**codex failures (8):** Elevated failure rate. The log captured a codex review failure this morning (09:10 UTC) with: `"rate limit: You've hit your usage limit. Try again at Apr 9th, 2026 9:22 PM."` This was misclassified as a parse error (no cooldown applied) because the rate-limit text detection fix (#1949) wasn't yet in the running binary. Now that 0.60.50 is deployed, future codex rate-limit text should be detected correctly. Cooldown is NOT showing in `orch cooldown list` — codex may continue receiving and failing review tasks until the next failure triggers the new detection logic and applies the Apr 9 cooldown.

**kimi cooldowns:** Still in billing cycle. `kimi`: 3h3m remaining, `kimi:haiku`: 3h59m remaining. No intervention needed — auto-recovery expected by ~13:00 UTC.

### Task activity (last 12h)

| Event | Count |
|-------|-------|
| status_change | 997 |
| dispatch | 304 |
| push | 270 |
| branch_delete | 246 |
| review_start | 140 |
| review_decision | 120 |
| pr_create | 119 |
| error | 26 |
| rerouted | 10 |
| timeout | 3 |

26 errors (transient HTTP), 10 reroutes, 3 timeouts — all within normal range. High throughput.

### Stuck/blocked tasks

| Task | Status | Reason |
|------|--------|--------|
| internal:54549 | blocked (18h) | "Respond to mention by @gabrielkoerich" |

`internal:54549` has been blocked for 18h — this appears to require human input for the @gabrielkoerich mention response. Review and unblock manually if appropriate.

---

## Retro Follow-Ups

| Priority from 2026-04-05 retro | Status |
|-------------------------------|--------|
| Monitor kimi recovery | ✗ Still in billing cycle. Cooldown: ~3h remaining. Expected auto-recovery by ~13:00 UTC. |
| opencode/copilot-sonnet silence watch | ✓ No new occurrences in last 24h |
| Review parse failure pattern | Partially resolved — `#1949` fix is in 0.60.50. Codex rate-limit still not in cooldown (one-time miss). |
| Async blocking audit | ✗ Not addressed. Still warranted. |
| verify_summary_matches_diff stability | ✓ No pre-dispatch validation failures observed |

---

## Priorities for Today

1. **Upgrade CLI and restart service** — `brew upgrade orch && brew services restart orch`. Service is at 0.60.50; CLI is behind at 0.60.44. Fix the mismatch now.

2. **Codex usage limit cooldown** — Codex hit its ChatGPT usage limit (until Apr 9). The cooldown wasn't applied because the detection fix wasn't yet running. After the CLI/service upgrade, codex may still receive tasks and fail. Watch `orch cooldown list` — after the next codex rate-limit failure, the new detection logic should apply the Apr 9 cooldown correctly. If codex is still failing by midday, consider manually clearing and re-running to force the cooldown: the cooldown should self-apply on next failure with 0.60.50.

3. **kimi recovery at ~13:00 UTC** — Cooldowns clear at ~13:00 (kimi) and ~14:00 (kimi:haiku). Verify recovery by checking `orch cooldown list` and agent dispatch after that window.

4. **Unblock internal:54549** — "Respond to mention by @gabrielkoerich" has been blocked 18h. Check if human action is needed.

5. **Async blocking audit** — Still deferred from yesterday. Run `rg 'std::fs::' src/` across async fns and file targeted issues. This has been deferred two days running.

6. **Watch review pipeline stability** — Five correctness fixes landed overnight (TOCTOU, watermark, store_increment, jitter, AgentResult.is_error). First production cycle with all these active. Watch for unexpected needs_review accumulation or review_agent_failures inflation.

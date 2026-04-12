+++
title = "Morning Review — 2026-04-12"
date = 2026-04-12
description = "Day 6: reliability sprint continues. 6 perf/correctness commits overnight. Three tasks blocked at max attempts. GitHub Copilot models (except gpt-5-mini) failing at 0-9% success rates via silent exits. Error log clean."
+++

# Morning Review — 2026-04-12

## Recent Commits (last 24h)

Six commits merged, continuing the reliability and performance sprint:

| Commit | PR | Summary |
|--------|----|---------|
| `b62b3793` | #2500 | fix: replace blocking `std::fs` with `tokio::fs` in async contexts |
| `ad2ec8f8` | #2499 | bug: `tick_detect_silent_agents` lacks error context in store lookups |
| `b5747032` | #2496 | perf: add `since` filter to `ingest_external_tasks` |
| `3dcb3a6b` | #2495 | perf: `tick_unblock_parents` calls `get_sub_issues()` for ALL blocked tasks including those with known block_reason |
| `18274db1` | #2491 | perf: cache `resolve_task_id` result in stuck-task recovery |
| `28aaf310` | #2490 | fix: skip `needs_review` re-fire when `store_increment` fails |

Themes: async correctness (blocking std::fs → tokio), observability (silent agent detection error context), and engine performance (filter at DB level, avoid redundant lookups, cache resolve calls).

---

## Operational Health

**Overall: stable, with no service-level errors. Three tasks blocked at max attempts. Copilot model performance is a concern.**

### Service

- **Version:** `orch/0.63.8` confirmed in logs (service upgraded since yesterday's 0.63.0 note)
- **Error log:** `/opt/homebrew/var/log/orch.error.log` is **0 bytes** — no service errors
- **Engine:** Running normally. Auto-merge, review, and cleanup pipelines all functioning. Multiple PRs merged and cleaned up in the past few hours.

### CLI/service version gap — likely resolved

Yesterday's retro flagged a 2-version gap (CLI 0.61.20 vs service 0.63.0). The service is now at 0.63.8. If the CLI was upgraded, this is resolved. If not, gap has widened further. Confirm:
```bash
orch version
```

### Blocked tasks

Three tasks are blocked at max attempts:

| Task | Agent | Tries | Issue |
|------|-------|-------|-------|
| #2478 | codex | 3 | opencode + nemotron-3-super-free `Provider returned error` (23+ wasted runs over 4+ days) |
| #2480 | codex | 3 | router LLM timeout of 90s delays fallback when fast agent is available |
| #2467 | opencode | 4 | blocking `Path::exists/is_dir` in async code should use `tokio::fs` |

These are code fixes for known bugs with open issues. They are blocked because agents failed to deliver working patches, not because of service degradation. Requires human review or manual unblock after PR inspection.

---

## Agent Health (24h)

| Agent | Model | Success | Failed | Other | Total | Rate |
|-------|-------|---------|--------|-------|-------|------|
| claude | sonnet | 92 | 36 | 6 | 134 | 69% |
| codex | gpt-5.3-codex | 78 | 2 | 6 | 86 | 91% |
| minimax | opus | 63 | 12 | 8 | 83 | 76% |
| opencode | github-copilot/gpt-5-mini | 50 | 0 | 1 | 51 | **98%** |
| opencode | opencode/minimax-m2.5-free | 36 | 0 | 0 | 36 | **100%** |
| kimi | opus | 29 | 12 | 0 | 41 | 71% |
| opencode | opencode/nemotron-3-super-free | 11 | 9 | 3 | 23 | 48% |
| opencode | github-copilot/gpt-5.4 | 1 | 10 | 0 | 11 | **9%** |
| opencode | github-copilot/gemini-3.1-pro-preview | 0 | 8 | 1 | 9 | **0%** |
| opencode | github-copilot/claude-sonnet-4.6 | 1 | 5 | 2 | 8 | 13% |
| opencode | github-copilot/claude-opus-4.6 | 0 | 4 | 0 | 4 | **0%** |

**Notable patterns:**

- **opencode/gpt-5-mini and opencode/minimax-m2.5-free are the strongest opencode performers.** Together they handle 87 runs at ~99% success. These models should be preferred for opencode routing.
- **GitHub Copilot models (except gpt-5-mini) are systematically failing.** gpt-5.4 (9%), gemini-3.1-pro-preview (0%), claude-sonnet-4.6 (13%), claude-opus-4.6 (0%) — all failing via silent exits (`unknown error (exit 0)`). Silence detection is triggering (`silence detection set task to new`) but these models keep getting re-selected. If cooldowns are being set, they are expiring quickly; if not, the silence detection → cooldown path may not be working for these models.
- **nemotron-3-super-free at 48%** — worse than yesterday (was 55%). Still the most visible recurring issue (issue #2478 tracks the symptoms in the nemotron+opencode combination).
- **claude/sonnet at 69%** — lower than typical. Needs monitoring but may reflect harder task mix rather than agent degradation.
- **codex/gpt-5.3-codex at 91%** — strong, consistent, and carrying a good share of the load.

### 12h Task Activity

| Event | Count |
|-------|-------|
| status_change | 2102 |
| dispatch | 648 |
| push | 434 |
| branch_delete | 380 |
| routed | 301 |
| review_start | 216 |
| review_decision | 206 |
| pr_create | 193 |
| error | 114 |
| rerouted | 59 |
| timeout | 11 |

Throughput up again: 648 dispatches vs 601 yesterday (+8%). Error count also up (114 vs 99), proportional to the increase. Errors are not a service-level concern — they track task-level failures handled by the retry/cooldown system.

---

## Retro Follow-ups (Apr 11 Evening)

| Priority | Status |
|----------|--------|
| Investigate opencode/nemotron failures and apply cooldown | **Open** — issue #2478 blocked at 3 codex attempts. Human review needed. |
| Adjust router LLM timeout / fast-path check (#2480) | **Open** — blocked at 3 codex attempts. Human review needed. |
| Audit rate_limit outcomes by model | **Not addressed** — still a concern (283 rate_limit in recent DB window). |
| Confirm CLI/service version parity | **Partially resolved** — service is at 0.63.8. CLI version unconfirmed. |

---

## Open Issues

| Issue | Status | Description |
|-------|--------|-------------|
| #2480 | open/blocked | Router LLM timeout of 90s delays fallback |
| #2478 | open/blocked | opencode + nemotron-3-super-free Provider error |
| #2467 | open/blocked | Blocking Path::exists/is_dir in async code |

No new issues created — existing issues cover the known operational problems. The GitHub Copilot model failure pattern (silent exits across gpt-5.4, gemini, claude-opus, claude-sonnet) does not yet have an issue. It should be monitored; if cooldowns are not accumulating for these models, the silence detection → cooldown path may need investigation (not model-specific special-casing, but a generic mechanism correctness check).

---

## Priorities for Today

1. **Check cooldown state for failing GitHub Copilot models** — Run:
   ```bash
   orch cooldown list
   sqlite3 ~/.orch/orch.db "SELECT key, value FROM kv WHERE key LIKE 'failure_count:opencode%' OR key LIKE 'cooldown:opencode%';"
   ```
   If these models have no active cooldowns despite 8-10+ failures, the silence detection → cooldown path has a bug worth filing.

2. **Review blocked tasks #2478, #2480, #2467** — These are stuck at max attempts. Either unblock manually or close and re-file with clearer task specifications for agents.

3. **Confirm CLI version parity** — `orch version` to check. If CLI is still on 0.61.x, upgrade now:
   ```bash
   brew upgrade orch
   ```

4. **Audit rate_limit outcomes** — Still unaddressed from evening retro. Run:
   ```bash
   sqlite3 ~/.orch/orch.db "SELECT agent, model, COUNT(*) FROM task_runs WHERE outcome='rate_limit' AND started_at > datetime('now', '-24 hours') GROUP BY agent, model ORDER BY COUNT(*) DESC;"
   ```
   Confirm that `record_rate_limit` is persisting retry timestamps correctly for top offenders.

5. **Monitor kimi recovery** — Kimi billing cooldown was set ~19h from ~10:00 UTC Apr 11. Should be coming back online around now (05:00 UTC Apr 12). Check `orch cooldown list` for kimi status and clear if billing has renewed.

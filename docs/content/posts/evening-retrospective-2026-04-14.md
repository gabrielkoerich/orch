+++
title = "Evening Retrospective — 2026-04-14"
date = 2026-04-14
description = "10 commits: Ollama routing, corrupted worktree recovery, cooldown async fix. claude/opus at 27% (down from 50%) — now 3-day pattern. 10 'no PR or code changes' failures. 59 tasks completed."
+++

# Evening Retrospective — 2026-04-14

## Summary

Lighter commit day — 10 merges vs yesterday's 28 — but meaningful coverage: Ollama local model routing ships as a feature, corrupted worktree recovery lands, and the cooldown async guarantees fix ensures KV persistence survives restarts. 59 tasks completed in 12h, 156 in 24h.

The dominant concern carries over and worsened: **claude/opus declined from 50% → 27% success rate** — the third consecutive day of deterioration. Additionally, **10 "no PR or code changes produced" failures** appeared across multiple agents, not just opus. Both patterns warrant investigation.

---

## What was accomplished today

10 commits merged:

| Commit | Issue | Description |
|--------|-------|-------------|
| `86a990de` | #2651 | **Fallback message for empty task_runs.error** — addresses the silent-failure diagnostic gap |
| `f87bc031` | #2647 | **Hoist batch_session_active() to tick()** — eliminates duplicate tmux subprocess per cycle (perf) |
| `50dc043e` | #2646 | **PushResult::NoCommits variant** — semantic fix; was incorrectly using PushResult::Failed |
| `fedaf9f5` | #2645 | **Security: has_leaks only on high-confidence patterns** — reduces false positives in gitleaks scan |
| `190c086a` | — | **set_model_cooldown / set_agent_cooldown now async** — guarantees KV persistence; was fire-and-forget before |
| `f96a1cc9` | #2641 | **RouterConfig::from_config() per review cycle** — called once now, not per dispatch |
| `976ef4f1` | #2610 | **files_modified falls back to empty** when agent doesn't report it |
| `ddf5636e` | #2623 | **Ollama local model routing** — new feature: route tasks to local models via Ollama |
| `b26b7ab2` | #2636 | **GitHub API calls fire-and-forget in tick phases 1b, 2, 4** — reduces tick latency |
| `d33f4463` | #2635 | **Corrupted worktree index recovery** — detect and recover from bad index in setup_worktree |

---

## Morning priorities — status

| Priority | Status |
|----------|--------|
| Fix CLI version mismatch | **Not verified** — `brew upgrade orch` still pending check. Do this first. |
| Investigate claude/opus 50% rate | **Worsened: now 27%** (3 success / 8 failed in 12h). Three-day declining trend. |
| Verify tick loop stall resolved | New stall (#2633, 350s) was filed and closed today — a separate instance. Monitoring continues. |
| Monitor kimi recovery (Apr 15) | `cooldown:kimi` at 7h1m remaining. On track for ~03:00 UTC Apr 15. |
| Investigate claude/(blank) model | Not investigated. Carry forward. |
| Review blocked tasks | 47 tasks blocked. Not audited today. |

---

## Agent health (12h snapshot)

| Agent | Model | Success | Failed | Other | Rate |
|-------|-------|---------|--------|-------|------|
| claude | sonnet | 34 | 23 | 1 timeout | 59% |
| claude | opus | 3 | 8 | — | **27%** |
| glm | opus | 11 | 6 | 2 rl, 1 to | 55% |
| minimax | opus | 23 | 3 | 4 rl, 1 to | 74% |
| opencode | gpt-5-mini | 12 | 0 | — | **100%** |
| opencode | minimax-m2.5-free | 15 | 1 | — | 94% |
| opencode | nemotron-3-super-free | 6 | 5 | — | 55% |
| opencode | gemini-3.1-pro-preview | 2 | 6 | 1 unknown | 25% |
| opencode | gpt-5.4 | 1 | 4 | — | 20% |
| opencode | claude-sonnet-4.6 (copilot) | 0 | 4 | — | 0% |
| opencode | claude-opus-4.6 (copilot) | 0 | 2 | — | 0% |

**Notable:**
- **claude/opus at 27%** — worst yet. Error patterns: empty error (pre-fix noise, should clear), "no PR or code changes produced" (2 opus-specific), plus general task failures.
- **claude/sonnet at 59%** — down from 69% yesterday. Concerning but less dramatic.
- **opencode/gpt-5-mini 100%** — steady best-performing free model. Carrying load reliably.
- **nemotron-3-super-free at 55%** — not as reliable today; cooldown at ~1h13m (from yesterday notes).
- **GitHub Copilot models** — continue failing. Cooldowns active and correctly applied.

---

## "No PR or code changes produced" — 10 failures today

Spread across multiple agents/models:

| Agent | Model | Count |
|-------|-------|-------|
| claude | sonnet | 5 |
| claude | opus | 2 |
| glm | opus | 1 |
| opencode | gpt-5.4 | 1 |
| opencode | claude-sonnet-4.6 (copilot) | 1 |

This is not purely a claude/opus issue — 5 of 10 are claude/sonnet. Possible causes:
1. Tasks with unclear requirements where agents complete but don't commit
2. Response parser failing to detect completed work
3. Tasks that were legitimately no-ops (e.g. "already done" cases)

Needs a query against the actual task bodies to determine if these are valid no-ops or agent failures.

---

## Active cooldowns

| Cooldown key | Remaining | Reason |
|---|---|---|
| `codex` | 41h18m | Billing cycle exhausted |
| `kimi` | 7h1m | Billing cycle |
| `glm:haiku` | 44m | Model cooldown |
| `opencode` (agent-level) | ~0m (expiring) | Short cooldown |
| `opencode:github-copilot/claude-opus-4.6` | 3h29m | Silence detection |
| `opencode:github-copilot/gemini-3.1-pro-preview` | 3h59m | Silence detection |
| `opencode:github-copilot/claude-sonnet-4.6` | 1h29m | Failure |
| `opencode:minimax-m2.5-free` | 59m | Short cooldown |

---

## What failed or needs attention

### 1. claude/opus at 27% — three-day declining trend

| Day | Success | Failed | Rate |
|-----|---------|--------|------|
| Apr 12 | ~13 | ~13 | ~50% |
| Apr 13 | 13 | 13 | 50% |
| Apr 14 | 3 | 8 | **27%** |

Error patterns over 48h (sqlite query from earlier):
- 30 with **empty error** (pre-#2652 fix — this noise should clear after today's fix)
- 4 **"no PR or code changes produced"**
- 2 **silent exit 0**

The empty-error fix (#2652) landed today, so tomorrow's data should be cleaner. If the rate stays below 40% with proper error messages, it points to genuine task failure — likely hard `complexity:complex` tasks.

### 2. 47 blocked tasks — unaudited

Carried from yesterday. These could be: max_review_cycles reached, CI failures, agent loop detection, or human-required tasks. Needs an audit to distinguish actionable blocked tasks from permanent blocks.

### 3. CLI/service version mismatch — unresolved

`brew upgrade orch` still pending from yesterday's finding (CLI 0.67.7 vs service 0.67.9). Run before next session.

---

## Issues created today

**1 new issue filed:**

- **#2653** — `investigate: claude/opus success rate declining 3 days running (52%→50%→27%)`

---

## Priorities for tomorrow (morning review)

1. **Fix CLI version mismatch first** — `brew upgrade orch && brew services restart orch && orch version`. Do this before anything else. Was pending since Apr 13.

2. **Check claude/opus error patterns after #2652 fix** — Now that empty errors are populated, query `task_runs` for the actual error messages on claude/opus failures. Determine: hard tasks vs model degradation vs prompt quality.
   ```bash
   sqlite3 ~/.orch/orch.db "SELECT error, COUNT(*) FROM task_runs WHERE agent='claude' AND model='opus' AND outcome='failed' AND started_at > datetime('now', '-12 hours') GROUP BY error ORDER BY COUNT(*) DESC LIMIT 10;"
   ```

3. **Monitor kimi recovery (~03:00 UTC Apr 15)** — `cooldown:kimi` expires tonight. Verify kimi begins routing and check first few completions.

4. **Audit "no PR or code changes produced"** — 10 failures today, cross-agent. Pull task bodies for the affected task IDs to determine if these are legitimate no-ops or agent failures.
   ```bash
   sqlite3 ~/.orch/orch.db "SELECT tr.task_id, t.title FROM task_runs tr JOIN tasks t ON t.id=tr.task_id WHERE tr.error='no PR or code changes produced' AND tr.started_at > datetime('now', '-12 hours') LIMIT 10;"
   ```

5. **Audit blocked tasks** — 47 blocked. Categorize by block reason and prioritize human-review cases.

6. **Monitor Ollama routing** — #2623 just merged. Verify the routing path to Ollama models works end-to-end if `olm` agent is configured.

---

Prepared by Orch automation (internal task internal:145446).

+++
title = "Daily Review — 2026-06-21"
date = 2026-06-21
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-06-21

## What Shipped (Last 24h)

**2 commits** landed today, both documentation updates.

| Commit | PR | Description |
|--------|----|-------------|
| `14058b38` | — | docs(agents): forbid naming internal projects/paths in issues; compress AGENTS.md under 40k |
| `136d67d6` | #3342 | docs(posts): daily review 2026-06-20 |

### Key Change: AGENTS.md Policy + Compression

The main commit (`14058b38`) enforces two things:

1. **New filing policy**: agents must never name downstream projects or paste absolute paths in issues. Home-dir paths leak the operator's username; even non-home paths often embed internal project names. Tilde-rooted placeholders or prose descriptions required. (Cautionary tale: issue #3339 named a specific downstream project and was deleted.)
2. **Size compression**: AGENTS.md trimmed from 46.1k → 37.6k to stay under the 40k character performance threshold. All operative rules preserved; only redundant restatements and verbose cautionary detail removed.

### Closed Issues (Today)

No issues were closed today. All prior closed issues are from earlier in the week.

---

## Operational Health

### Volume (Last 24h)

| Metric | Count |
|--------|-------|
| Status changes | 278 |
| Dispatches | 88 |
| Pushes | 78 |
| Branch deletes | 76 |
| Routed | 40 |
| Review starts | 39 |
| Review decisions | 37 |
| PRs created | 36 |
| Errors | 9 |
| Reroutes | 2 |

Healthy volume — 88 dispatches, 36 PRs, 9 errors. Error count slightly up from yesterday (8), but within normal range. Reroutes (2) are nominal.

### Agent / Model Outcomes (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 26 |
| kimi | opus | success | 11 |
| codex | gpt-5.4 | success | 9 |
| codex | gpt-5.5 | success | 8 |
| opencode | mimo-v2.5-free | success | 4 |
| opencode | north-mini-code-free | success | 4 |
| minimax | opus | rate_limit | 2 |
| opencode | deepseek-v4-flash-free | success | 2 |
| opencode | nemotron-3-ultra-free | success | 2 |
| claude | sonnet | failed | 2 |
| claude | haiku | blocked | 1 |
| claude | haiku | failed | 1 |
| claude | sonnet | (no outcome) | 1 |
| codex | gpt-5.5 | blocked | 1 |
| opencode | nemotron-3-ultra-free | failed | 1 |
| opencode | north-mini-code-free | parse_error | 1 |

**Effective pool**: claude/sonnet remains the dominant workhorse (26 successes). Kimi/opus healthy (11). Codex pool strong between gpt-5.4 (9) and gpt-5.5 (8). Opencode multi-model coverage solid across four models (12 combined).

**Failures**: claude/sonnet had 2 failures (normal tail — transient errors, both handled by per-model retry), nemotron-3-ultra-free and north-mini-code-free each had 1 parse_error or failure — per-model cooldowns activated automatically.

**Minimax**: still rate-limited (2 hits). Not unexpected given the 23h3m cooldown still running.

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| minimax | 23h3m | persisted (rate limits) |
| minimax:haiku | 9h | persisted |

Minimax:opus cooldown has cleared since yesterday (was 3h3m). The agent-wide 23h cooldown still covers it. No other agent or model in cooldown — clean pool otherwise.

### Cleanup Reconciliation Timeouts

The cleanup module continues to time out on candidate listing. Observed at **22:43, 22:46, 22:48, and 22:58 UTC** today — roughly once per tick window during this period:

```
WARN orch::engine::cleanup: timed out listing reconciliation candidates timeout_secs=30
```

The throttle fix from PR #3341 is **merged but not deployed** — the service is still running v0.80.25 and the fix landed in a later release. These timeouts are harmless (scan is abandoned and retried next tick) but noisy. They will stop once the service is upgraded to v0.80.28.

### Cron-Boundary Slow Tick

The 23:00 UTC slow tick recurred as predicted:

```
WARN slow tick elapsed_ms=43775
```

43.8 seconds — consistent with the pattern observed the prior two days. The daily-review and evening-retrospective crons fire simultaneously at 23:00:07 UTC, causing I/O contention during parallel worktree creation. No work was lost; both tasks routed and dispatched correctly.

### Service Version

**Running**: v0.80.25 · **Latest**: v0.80.28 — three versions behind. The cleanup throttle from #3341 is in v0.80.28. Upgrade is overdue.

---

## Blocked / Stuck Tasks

| Task | Status | Attempts | Reason |
|------|--------|----------|--------|
| internal (external #490) | new | 5 | — (no block reason, retrying) |
| internal (external #493) | new | 5 | — (no block reason, retrying) |
| Many (external #432, #442, #445, #449, #494+) | blocked | 4–6 | CI failure limit (3) reached during auto-merge |

**CI-limit blocks**: Several downstream tasks hit `CI failure limit (3) reached during auto-merge`. This is working as designed — CI is failing repeatedly and the auto-merge guard is blocking. Requires human attention on the downstream CI failures; `orch task unblock all` after CI is fixed.

**High-retry new tasks** (external #490 and #493 at 5 attempts): router is repeatedly routing but no block reason set, suggesting routing succeeds but something upstream is retrying. Worth monitoring; if they hit the attempt ceiling, they'll be blocked and flagged.

---

## Priorities for Tomorrow

1. **Upgrade service to v0.80.28** — three versions behind; cleanup throttle fix is waiting:
   ```bash
   brew update && brew upgrade orch
   brew services restart orch
   orch -V
   ```
2. **Investigate CI-failure blocks** — multiple downstream tasks blocked at `CI failure limit (3)`. Review the CI failures in the affected downstream project and unblock after fixing:
   ```bash
   orch task unblock all
   ```
3. **Monitor high-retry tasks** (external #490, #493) — 5 attempts each with no block reason. If they stop progressing, investigate routing logs for the specific failure mode.
4. **Minimax recovery tomorrow** — agent-wide cooldown clears in ~23h. Verify clean re-routing when it lifts.
5. **Watch cron-boundary slow ticks** — third consecutive day at 23:00 UTC. If it persists, consider staggering the daily-review and evening-retrospective crons by 2–3 minutes to avoid concurrent worktree creation.

---

*Prepared by Orch automation (internal:154199)*

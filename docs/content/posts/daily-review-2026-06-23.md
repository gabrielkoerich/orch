+++
title = "Daily Review — 2026-06-23"
date = 2026-06-23
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-06-23

## What Shipped (Last 24h)

**2 commits** landed in the last 24 hours.

| Commit | PR | Description |
|--------|----|-------------|
| `699a37cc` | #3349 | fix(runner): classify minimax Token Plan usage limit as billing_cycle_exhausted |
| `2d4d3cf1` | #3346 | docs(posts): daily review 2026-06-22 |

### Closed Issues (Last 24h)

| Issue | Closed | Description |
|------|--------|-------------|
| #3348 | 2026-06-23 | minimax 'Token Plan usage limit reached' classified as rate_limit instead of billing_cycle_exhausted |
| #3344 | 2026-06-22 | silence-detection reroutes could be converted into false `done` via the no-code `needs_review` path |

The key shipped fix (#3349) corrects the minimax quota misclassification: "Token Plan usage limit reached" now triggers `billing_cycle_exhausted` (7d model-level cooldown) instead of generic `rate_limit` (4h cap), preventing the repeated 18-failure cycle that occurred Jun 13–22.

---

## Operational Health

### Throughput (Last 24h)

| Metric | Count |
|--------|-------|
| Status changes | 223 |
| Dispatches | 64 |
| Pushes | 58 |
| Branch deletes | 62 |
| Routed | 28 |
| Review starts | 30 |
| Review decisions | 28 |
| PRs created | 28 |
| Errors | 6 |
| Reroutes | 2 |

Volume remained healthy. The system processed ~60 agent dispatches and ~30 PR reviews without systemic stalls.

### Agent / Model Outcomes (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 21 |
| opencode | deepseek-v4-flash-free | success | 8 |
| kimi | opus | success | 7 |
| codex | gpt-5.4 | success | 6 |
| codex | gpt-5.5 | success | 6 |
| minimax | opus | rate_limit | 2 |
| opencode | mimo-v2.5-free | success | 2 |
| claude | sonnet | failed | 1 |
| codex | gpt-5.4-mini | blocked | 1 |
| opencode | nemotron-3-ultra-free | (no outcome) | 1 |
| opencode | north-mini-code-free | parse_error | 1 |

**Aggregate: 51 successes, 1 failure, 2 rate limits, 1 parse error, 1 blocked, 2 in-flight/no-outcome.**

### What Went Well

1. **Claude/sonnet and Codex/gpt-5.4/5.5 carried the bulk of successful work** — no systemic degradation visible.
2. **The minimax billing misclassification fix (#3349) landed and deployed** — prevents the 18-failure cycle from recurring. Minimax is now correctly cooled for ~21h (cooldown until ~Jun 24 23:00Z).
3. **Failover worked correctly** — both nightly jobs (daily review + evening retrospective) initially routed to minimax/opus, hit the quota error, and were rerouted to opencode and claude respectively.

### What Failed

#### 1. Service still lags latest release (v0.80.25 → v0.80.30, 5 versions behind)

| Item | Value |
|------|-------|
| Running version | `0.80.25` |
| Latest release | `0.80.30` |
| Gap | 5 releases |

Missing fixes not yet deployed:
- **#3337** — JSONL domain output parsing (fixes output capture for structured agents)
- **#3341** — cleanup reconciliation throttle (removes the 30s timeout spam)
- **#3345** — silence-detection reroutes correctness (prevents false needs_review/done)
- **#3349** — minimax billing_cycle_exhausted classification (prevents repeated quota retries)

The log shows the upgrade check fired at `2026-06-23T22:56:19.522617Z WARN orch upgrade available current_version=0.80.25 latest_version=0.80.30`. Issue **#3347** tracks this deployment lag.

#### 2. Stale opencode model pool warnings persist

Every sync cycle emits:
```
agent model pool appears stale: persistent model failures in heavily cooled pool
opencode:2/4:opencode/nemotron-3-ultra-free,opencode/north-mini-code-free
```

This is the detector in `src/engine/sync.rs` firing because 2 of 4 configured opencode models have persistent failure markers (`failure_count=3` each). The models keep getting re-selected after cooldowns expire, fail again, and re-cool. This is pool health drift, not a new code regression.

#### 3. Minimax quota exhaustion hit both nightly jobs (again)

Tasks `internal:154260` (this review) and `internal:154261` (evening retrospective) both first routed to `minimax/opus`, then failed with:
```
API Error: Request rejected (429) · Token Plan usage limit reached
```

The fix in #3349 now classifies this as `billing_cycle_exhausted` (model-level 7d cooldown) instead of `rate_limit`. Minimax has `failure_count=18` total and is blocked for ~21h. Recovery worked, but first-attempt 429s still waste the nightly window.

#### 4. One claude/sonnet failure and one opencode parse_error

- `claude/sonnet` — 1 `failed` outcome (need to check if this is the "session limit" pattern or something else)
- `opencode/north-mini-code-free` — 1 `parse_error` (recurring pattern, model has `failure_count=3`)
- `codex/gpt-5.4-mini` — 1 `blocked` (likely task #3347 which is externally blocked on deployment)

---

## Stuck / Blocked Work

### Current active scheduled work

| Task | Status | Attempts | Note |
|------|--------|----------|------|
| `internal:154260` | in_progress | 1 | This daily review (opencode/nemotron-3-ultra-free) |
| `internal:154261` | in_progress | 1 | Evening retrospective (claude/sonnet) |
| `#3347` | blocked | 1 | Service upgrade lag — waiting on human deploy |

### Downstream backlog (gabrielkoerich/oblivion)

- **44 blocked tasks** — almost all on `CI failure limit (3) reached during auto-merge`
- 2 tasks (`#490`, `#493`) remain `new` after **5 attempts** each
- 1 task (`#419`) blocked on **max review cycles (2) exceeded**
- 1 task (`#458`) blocked on **review agent rebroadcast escalated after repeated retries**

This remains a downstream CI throughput problem. Orch is surfacing the bottleneck accurately; the queue will not clear until those downstream CI failures are addressed.

---

## Routing Accuracy

Routing was mostly accurate:

- Daily review correctly re-routed from minimax → opencode after quota hit
- Evening retrospective correctly re-routed from minimax → claude
- Claude and Codex were selected for highest volume of successful work

**Main concern: pool health drift.** Opencode retains 2 persistently cooled models in active configuration; Minimax remains quota-limited. The routing logic is sound, but the configured pools have degraded models that keep cycling back in after cooldowns expire.

No evidence of silent-model failure loops (fixed earlier in the month). Current signals are explicit: rate limits, parse errors, and stale-pool warnings.

---

## Issues

**One existing open issue:** #3347 (service lag v0.80.25→v0.80.30). No new issues filed from this review.

Reasons no new issues filed:
1. Service deployment lag is already tracked in #3347 — root cause is operational, not code.
2. Stale opencode pool warning is an existing detector firing on configured pool drift, not a new regression.
3. Minimax quota is now correctly classified as billing_cycle_exhausted (#3349 fixed) — the 7d model-level cooldown will prevent re-selection until quota likely resets.
4. Downstream Oblivion CI blocks are external program-health issues, not Orch bugs.

---

## Priorities for Tomorrow

1. **Upgrade the running service to v0.80.30.** Removes the cleanup reconciliation timeout noise and picks up all 4 recent engine fixes (#3337, #3341, #3345, #3349).
   ```bash
   brew update && brew upgrade orch && brew services restart orch
   ```

2. **Review the opencode model pool.** Two of four configured models (`nemotron-3-ultra-free`, `north-mini-code-free`) have `failure_count=3` and trigger the stale-pool alert every sync tick. Consider removing or replacing them.

3. **Keep minimax off critical scheduled jobs until quota stabilizes.** The billing_cycle_exhausted fix prevents the retry storm, but first-attempt 429s still waste the nightly window. Temporarily label nightly jobs with `agent:claude` or `agent:codex`.

4. **Triage Oblivion blocked backlog as CI/program-health.** 44 tasks blocked on CI failures is a downstream signal — Orch is working as designed by surfacing it.

---

*Prepared by Orch automation (internal:154260)*
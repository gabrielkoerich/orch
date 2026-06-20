+++
title = "Daily Review — 2026-06-20"
date = 2026-06-20
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-06-20

## What Shipped (Last 24h)

**2 commits** landed today, headlined by the cleanup reconciliation throttle fix.

| Commit | PR | Description |
|--------|----|-------------|
| `26c4c7f1` | #3341 | fix: throttle cleanup reconciliation scans |
| `a6f4c716` | #3338 | docs: daily review (last 24h) post |

### Closed Issues (Today)

| Issue | Description |
|-------|-------------|
| #3340 | bug(cleanup): reconciliation candidate listing still times out — fixed by #3341 |
| #3339 | ops: GitHub Actions billing failure still blocks bean tasks — resolved by earlier revert of #3334 |
| #3336 | bug(parser): JSONL domain output misread as orch agent status — fixed by #3337 (landed yesterday) |
| #3331 | ops: service version lag (stale issue, auto-reconciled to done) |

### Key Fix: Cleanup Reconciliation Throttle (#3341)

`src/engine/cleanup.rs` was issuing unbounded candidate scans during reconciliation ticks, causing timeouts under load (issue #3340). The fix throttles these scans — 161 lines changed, touching `src/backends/github.rs` and `cleanup.rs`. This addresses a recurring pattern where cleanup ticks were competing with routing and dispatch ticks.

---

## Operational Health

### Volume (Last 24h)

| Metric | Count |
|--------|-------|
| Status changes | 272 |
| Dispatches | 84 |
| Pushes | 83 |
| Branch deletes | 58 |
| Review starts | 42 |
| Review decisions | 39 |
| PRs created | 39 |
| Routed | 35 |
| Errors | 8 |
| Reroutes | 2 |

Moderate volume day — 84 dispatches, 39 PRs. Error count (8) is low relative to volume. Reroutes (2) are normal.

### Agent / Model Outcomes (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 31 |
| kimi | opus | success | 12 |
| codex | gpt-5.5 | success | 7 |
| codex | gpt-5.4 | success | 5 |
| opencode | deepseek-v4-flash-free | success | 5 |
| opencode | mimo-v2.5-free | success | 4 |
| minimax | opus | rate_limit | 2 |
| opencode | north-mini-code-free | success | 2 |
| claude | haiku | blocked | 1 |
| claude | haiku | failed | 1 |
| claude | haiku | success | 1 |
| codex | gpt-5.5 | blocked | 1 |
| opencode | nemotron-3-ultra-free | failed | 1 |
| opencode | nemotron-3-ultra-free | parse_error | 1 |
| opencode | north-mini-code-free | parse_error | 1 |

**Effective pool**: claude/sonnet continues as the dominant workhorse (31 successes). Kimi/opus is healthy (12). Codex gpt-5.5 and gpt-5.4 performing well (12 combined). Opencode deepseek and mimo rounding out the second tier (9 combined). Minimax still degraded (rate limits).

**Parse errors**: opencode/nemotron-3-ultra-free (1) and opencode/north-mini-code-free (1) both hit parse_error outcomes — the per-model cooldown mechanism will handle these automatically.

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| minimax | 1d23h | persisted (rate limits) |
| minimax:haiku | 8h59m | persisted |
| minimax:opus | 3h3m | persisted |

Minimax is deeply cooled — the agent-wide cooldown runs nearly 2 more days. No opencode model cooldowns visible at review time, suggesting the parse_errors from nemotron and north-mini are at early backoff stages.

### Routing Health

**One slow tick** at startup (36s when daily-review `internal:154163` and evening-retrospective `internal:154164` fired simultaneously at 23:00:07 UTC). This is the same cron-boundary stall pattern noted in yesterday's review. Both tasks routed and dispatched correctly; the slow tick was likely I/O contention during parallel worktree creation.

**Upgrade warning in logs**: `orch upgrade available current_version=0.80.25 latest_version=0.80.28` — service is 3 versions behind.

**Issue #3331 auto-resolved**: the cleanup reconciliation correctly detected this issue as having a stale `blocked` label on a closed GitHub issue and transitioned it to `done` automatically. The new reconciliation throttle in #3341 was already live.

Sync ticks completing in 2.0–2.9s. No `AllAgentsCooledError`, no routing gaps.

### Service Version

**Running**: v0.80.25 · **Latest**: v0.80.28

Three versions behind. The reconciliation throttle fix (#3341) and the JSONL parser fix (#3337) are in newer releases. Upgrade is overdue.

---

## Blocked / Stuck Tasks

| Task | Project | Status | Tries | Block Reason |
|------|---------|--------|-------|--------------|
| #3317 | gabrielkoerich/orch | blocked | 3 | codex `gpt-5.2` in config — waiting on human config edit |
| #3313 | gabrielkoerich/orch | blocked | 8 | codex `gpt-5.3` in config — waiting on human config edit |

Both tasks remain blocked solely on config: dead codex model names (`gpt-5.2`, `gpt-5.3`) that are no longer available need to be removed from `~/.orch/config.yml`. This is a human-only action (config files are off-limits to agents). #3313 is at 8 attempts — the most-retried blocked task in the system.

---

## Priorities for Tomorrow

1. **Upgrade service to v0.80.28** — 3 versions behind; recent fixes (reconciliation throttle, JSONL parser) are in it:
   ```bash
   brew update && brew upgrade orch
   brew services restart orch
   orch -V
   ```
2. **Config edit: remove gpt-5.2 and gpt-5.3 from codex model pool** — #3313 (8 tries) and #3317 (3 tries) are blocked on dead model names in `~/.orch/config.yml`. Remove them to close both issues.
3. **Watch cron-boundary slow ticks** — the simultaneous daily-review + evening-retrospective dispatch at 23:00 UTC caused a 36s tick two days running. If it continues, consider staggering the cron schedules by 2–3 minutes to avoid concurrent worktree creation.
4. **Monitor opencode parse errors** — nemotron-3-ultra-free and north-mini-code-free each hit a parse_error. Verify they recover cleanly from per-model cooldown without manual intervention.
5. **Minimax recovery** — the agent-wide cooldown expires in ~1d23h. No action needed; just verify clean re-routing when it clears.

---

*Prepared by Orch automation (internal:154163)*

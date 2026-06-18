+++
title = "Daily Review — 2026-06-18"
date = 2026-06-18
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-06-18

## What Shipped (Last 24h)

An exceptionally productive day: **8 commits** merged and **10+ issues closed**. Almost all were targeted bug fixes to the runner, parser, router, jobs scheduler, and worktree subsystems.

| Commit | PR | Description |
|--------|----|-------------|
| `92c98607` | #3334 | fix(router): skip agent dispatch for repos with active GitHub Actions billing failures |
| `cbf54563` | #3333 | fix(parser): add missing 'fixed' status alias to normalize_status |
| `7cdcf359` | #3329 | fix(jobs): quarantine per-project load_jobs failures instead of aborting scheduler tick |
| `a77d8f83` | — | test(worktree): cover origin-anchored worktree creation end-to-end |
| `e678d400` | — | fix(worktree): anchor new worktrees on origin/<default_branch> |
| `735d0b26` | #3325 | fix(router): use alias instead of full API model ID as default route |
| `28faa4da` | #3327 | fix: skip empty model success resets |
| `0234c4f0` | #3323 | docs(posts): daily review 2026-06-18 (earlier draft) |

### Closed Issues (Today)

| Issue | Description |
|-------|-------------|
| #3332 | ops: GitHub Actions billing failures on gabrielkoerich/bean |
| #3330 | bug(parser): normalize_status missing 'fixed' alias |
| #3326 | bug(jobs): duplicate job id in one project aborts scheduler tick |
| #3325 | bug(router): LLM router pool uses full API model ID for codex alias |
| #3324 | bug(runner): record_agent_success with None model writes incomplete KV keys |
| #3321 | bug(codex): app-server event stream lag classified as silence |
| #3318 | bug(runner): billing_cycle_exhausted for github-copilot triggers wrong cooldown |
| #3314 | bug(runner): opencode/north-mini-code-free review parse_error |
| #3309 | enhancement(sync): auto-recover tasks blocked by 'review recovery timeout' |
| #3308 | bug(runner): find_auth_line_scan bare '403'/'401' match causes false positives |
| #3307 | bug(review): timeout path doesn't apply model cooldown |
| #3302 | bug(runner): 'You have exceeded your monthly quota' (GitHub Copilot) not classified |
| #3301 | bug(runner): opencode 'Upstream idle timeout exceeded' classified as silence |
| #3300 | bug(parser): normalize_status missing 'verified' and 'alert' |

**Notable fixes**:
- **#3334 / #3329 combo**: The scheduler can now survive one bad project's jobs config without aborting the entire tick. Paired with the billing-failure router skip, `gabrielkoerich/bean` GitHub Actions failures no longer cascade to orch routing.
- **#3325**: The LLM router was storing the full API model ID (e.g. `gpt-5.4-codex`) instead of the configured alias, causing model-level cooldown lookups to miss. Fixed.
- **#3327**: Success resets with `None` model were overwriting failure counts for all models on an agent. Fixed.
- **Worktree anchoring** (`e678d400` + `a77d8f83`): New worktrees now anchor on `origin/<default_branch>` instead of local HEAD, preventing stale base refs on long-running services.

---

## Operational Health

### Volume (Last 24h)

| Metric | Count |
|--------|-------|
| Status changes | 257 |
| Dispatches | 80 |
| Branch deletes | 74 |
| Pushes | 66 |
| Routed | 35 |
| Review starts | 32 |
| PRs created | 31 |
| Reroutes | 6 |
| Errors | 12 |
| Push/worktree recoveries | 2 |

High throughput day — 80 dispatches, 31 PRs, 32 reviews.

### Agent / Model Outcomes (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 23 |
| kimi | opus | success | 9 |
| opencode | deepseek-v4-flash-free | success | 7 |
| codex | gpt-5.4 | success | 4 |
| codex | gpt-5.5 | success | 4 |
| opencode | nemotron-3-ultra-free | success | 3 |
| minimax | opus | rate_limit | 3 |
| minimax | sonnet | rate_limit | 2 |
| claude | sonnet | failed | 2 |
| claude | sonnet | (no outcome recorded) | 2 |
| opencode | mimo-v2.5-free | success | 2 |
| minimax | opus | success | 1 |
| claude | haiku | success | 1 |
| claude | haiku | failed | 1 |
| claude | haiku | blocked | 1 |
| claude | sonnet | rate_limit | 1 |
| kimi | opus | failed | 1 |

**Effective pool**: claude/sonnet is the workhorse (23 successes). Codex is back and healthy with gpt-5.4 and gpt-5.5 (8 combined successes — the dead gpt-5.2/5.3 models are behind us). Kimi/opus is running well (9 successes). Minimax is degraded with 5 rate-limit hits and an agent-level cooldown.

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| minimax | ~1d22h | persisted (rate limits) |
| minimax:haiku | ~2h59m | persisted |

No other agent or model cooldowns are active. Routing is healthy across all other agents.

### Routing Health

Clean logs — no `AllAgentsCooledError`, no watchdog triggers, no GitHub API connectivity failures. Sync ticks are completing in 1.7–3.0 s. The LLM router is making accurate routing decisions (this task itself was correctly classified as `claude/medium`).

---

## Blocked / Stuck Tasks

| Task | Project | Status | Tries | Block Reason |
|------|---------|--------|-------|--------------|
| #3331 | gabrielkoerich/orch | blocked | 3 | Service v0.80.20 lags latest v0.80.24 — **upgrade needed** |
| #3317 | gabrielkoerich/orch | blocked | 3 | codex `gpt-5.2` in config — waiting on human config edit |
| #3313 | gabrielkoerich/orch | blocked | 8 | codex `gpt-5.3` in config — waiting on human config edit |

Note: issue #3331 title says "lags v0.80.22" but the engine logs show `latest_version=0.80.24`. The gap is 4 patch versions, not 2. The fixes in #3327, #3329, #3333, #3334 have all been merged and should be in one of those releases.

---

## Service Version Gap

**Running**: v0.80.20 · **Latest**: v0.80.24

Four versions behind. Today's critical fixes (scheduler job quarantine, router billing-failure skip, normalize_status aliases, model success reset) are merged but not yet deployed. Recommend:

```bash
brew update && brew upgrade orch
brew services restart orch
orch -V
```

---

## Priorities for Tomorrow

1. **Upgrade service to v0.80.24** — four versions behind; today's critical fixes are sitting undeployed. Run the post-push cycle from CLAUDE.md.
2. **Config edit: remove gpt-5.2 and gpt-5.3 from codex model pool** — #3313 and #3317 remain blocked until `~/.orch/config.yml` is updated. The generic cooldown system works, but the models keep re-entering the pool when cooldowns expire.
3. **Monitor minimax recovery** — agent-level cooldown expires in ~2 days. Verify it re-routes cleanly when it clears.
4. **Watch 2 unrecorded claude/sonnet outcomes** — two runs have no outcome in `task_runs`. Could be the runner crash path or a timing edge case. Monitor whether the pattern recurs.

---
*Prepared by Orch automation (internal:154084)*

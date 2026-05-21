+++
title = "Morning Review — 2026-05-21"
date = 2026-05-21
description = "Daily morning review and operational priorities."
+++

# Morning Review — 2026-05-21

## Recent Commits (Last 24h)

| Commit | Description |
|--------|-------------|
| `5da54917` | docs(posts): add evening retrospective for 2026-05-20 |
| `45c46f2f` | bug(router): LLM routing budget fallback recurs throughout day, degrading to round-robin |
| `cc620eb7` | fix(router): prune unavailable opencode models from model_map at config load time |
| `f9a280c9` | bug(parser): status alias 'skipped' is not normalized, causing avoidable run failures |
| `86dc72e1` | feat(jobs): allow task body to be loaded from a prompt file |

A productive 2026-05-20: three bugs merged (router budget reset, `skipped` normalization gap, stale model_map entries) plus a new prompt-file feature. None of these are running yet — service is still at 0.73.0.

## Operational Health

**Service version**:

| Component | Version |
|-----------|---------|
| CLI | 0.72.0 |
| Service | 0.73.0 |
| Latest release | 0.73.3 |

All three fixes merged yesterday (PRs #3170, #3171, #3172) are in 0.73.1–0.73.3. The service upgrade is the top operational priority.

**Confirmed impact of version lag**: This morning's logs show all three scheduled tasks (internal:150088, 150089, 150090) hit `LLM routing budget exceeded — falling back to round-robin immediately` at `budget_secs=30`. This is the exact bug fixed by PR #3170. The fix works; it's just not deployed.

**Task run outcomes (last 24h)**:

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 34 |
| kimi | opus | success | 29 |
| codex | gpt-5.3-codex | success | 18 |
| opencode | github-copilot/gpt-5-mini | success | 17 |
| opencode | github-copilot/claude-sonnet-4.6 | success | 8 |
| opencode | opencode/nemotron-3-super-free | success | 4 |
| opencode | opencode/qwen3.6-plus-free | success | 4 |
| kimi | opus | failed | 4 |
| claude | sonnet | failed | 3 |
| kimi | opus | rate_limit | 1 |
| kimi | opus | aborted | 1 |
| codex | gpt-5.3-codex | failed | 1 |
| glm | opus | failed | 1 |
| glm | opus | rate_limit | 1 |
| minimax | opus | rate_limit | 1 |
| opencode | github-copilot/gpt-5-mini | blocked | 1 |
| opencode | github-copilot/gpt-5.4 | success | 1 |
| opencode | opencode/deepseek-v4-flash-free | success | 1 |
| opencode | opencode/minimax-m2.5-free | failed | 1 |

**Success rate**: ~116 successes vs ~10 failures + 3 rate limits — throughput is healthy.

**New model observed**: `github-copilot/gpt-5.4` appeared with 1 success. This is a new opencode model alias; no action needed (cooldown system handles it generically if it later fails).

**Log observations**:
- Stale model WARNs still firing on every dispatch for `github-copilot/gpt-5.3` and `github-copilot/claude-opus-4.6` — fixed by PR #3172, pending service upgrade.
- One slow tick at 60.6s (marginal, same pattern as yesterday).
- All other tick times healthy (1.6–2.9s).
- Error log is empty — no startup errors.

## Stuck / Blocked Tasks

| Task | Status | Age | Notes |
|------|--------|-----|-------|
| `internal:149337` | blocked | 11d | SSH agent communication error — environment issue, not orch |
| `internal:149970` | blocked | 1d | bean/Hyperliquid task — zero review cycles, no block_reason |
| `internal:149863` | blocked | 3d | bean/Hyperliquid task — zero review cycles, no block_reason |
| `internal:149855` | blocked | 3d | bean/trading task — zero review cycles, no block_reason |
| `internal:149675` | blocked | 5d | bean/HyperLend credential task — zero review cycles, no block_reason |
| `internal:149673` | blocked | 5d | bean/HyperLend task — zero review cycles, no block_reason |
| `internal:149038` | blocked | 16d | bean/Twitter research task — zero review cycles, no block_reason |
| `internal:148985` | blocked | 17d | bean/Twitter research task — zero review cycles, no block_reason |

The pattern of bean-project tasks blocked with 0 review_cycles and no block_reason has been noted in previous retros. These are likely agent failures (API credential or network) that hit an unclassified error path before entering the review cycle. The oldest (148985) is 17 days old. Operator should manually inspect these via `orch task show <id>` and either unblock or close stale ones.

## Retro Follow-ups

| Priority from 2026-05-20 retro | Status |
|-------------------------------|--------|
| Upgrade service to 0.73.3 | ❌ Still at 0.73.0 — top priority |
| Verify `skipped` normalization after upgrade | ⏳ Pending service upgrade |
| Investigate 4 blocked opencode/gpt-5-mini tasks | ⏳ Now 7 tasks; all bean-project agent failures |
| Kimi review is_hard_failure (#3159) | ✅ Issue closed (fix likely in 0.73.x) |
| internal:149337 SSH block | ❌ Still blocked, 11d |

#3159 (kimi review `is_hard_failure` misclassification) is now closed. Kimi still shows 4 failures today but that may reflect pre-fix dispatches. Confirm trend after service upgrade.

## Priorities For Today

1. **Upgrade service to 0.73.3** — `brew update && brew upgrade orch && brew services restart orch`. This is day 2 of this being the top priority. Three bug fixes are waiting to take effect.
2. **After upgrade, verify routing quality** — check that LLM routing budget no longer exhausts on every tick, and stale model WARNs stop appearing.
3. **Clean up stale bean-project blocked tasks** — manually inspect 148985 and 149038 (17d and 16d blocked). These are likely unrecoverable without operator intervention; consider closing them.
4. **internal:149337 SSH block (11d)** — operator should restart the SSH agent or re-add the key. This will not self-resolve.

---

Prepared by Orch automation (internal:150090).

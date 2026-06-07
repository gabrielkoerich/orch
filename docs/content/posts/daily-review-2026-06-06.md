+++
title = "Daily Review — 2026-06-06"
date = 2026-06-06
description = "End-of-day operational review: commits, issues, task health, and priorities."
+++

# Daily Review — 2026-06-06

## What Shipped (Last 24h)

**1 new commit landed** on `gabrielkoerich/orch`:

| Commit | Description |
|--------|-------------|
| `3f26c6f2` | fix(cooldown): sync in-memory map from KV every tick so external clears land |

**Service upgraded to v0.80.1** (was v0.79.1 in yesterday's review). This includes the CLI trim (#3270), `orch commit` LLM messages (#3269), the cooldown sync fix, and all prior fixes through the v0.79.x line.

**No new closed issues** since yesterday's review. The 3 bug fixes (#3272, #3273, #3274) remain open.

## Operational Health

### Task Run Summary (Last 24h)

| Agent | Model | Success | Failed | Rate Limit | Timeout | Parse Error | Other |
|-------|-------|---------|--------|------------|---------|-------------|-------|
| claude | sonnet | 113 | 7 | 1 | — | 3 | 1 push_failed |
| claude | opus | 41 | 5 | — | 2 | — | 1 aborted |
| opencode | deepseek-v4-flash-free | 20 | 9 | — | 6 | 1 | 1 empty |
| opencode | nemotron-3-ultra-free | 11 | 3 | 1 | 1 | 2 | — |
| kimi | opus | 1 | 10 | 7 | — | — | 1 aborted |
| codex | gpt-5.5 | 3 | 4 | — | — | — | — |
| codex | gpt-5.4 | 3 | — | 1 | — | — | — |
| codex | gpt-5.3 | 0 | 2 | — | — | — | — |
| minimax | opus | 0 | 5 | 2 | — | — | — |
| opencode | minimax-m3-free | 0 | 4 | — | — | — | — |
| opencode | mimo-v2.5-free | 0 | 1 | — | 1 | — | — |

**Total agent runs: ~270** (lower than yesterday's ~376 — cooldowns throttled capacity).

### Agent Pool Health

- **Active cooldowns:**
  - `codex` — 39m (agent-wide, persisted)
  - `kimi` — 1d21h (agent-wide, billing cycle exhaustion)
  - `kimi:opus` — 21h8m (model-specific)
  - `minimax` — 20h27m (agent-wide, persisted)
- **Degraded agents:** codex, kimi, minimax (3 degraded — same as yesterday)
- **Recovered agents:** opencode (cleared degradation during this tick)
- **Effective routing pool:** claude (sonnet/opus) — effectively single-agent operation

### Key Error Patterns

1. **kimi massive cooldown (1d21h)** — kimi hit its usage limit and is locked out for nearly 2 days. All 3 open bug fixes (#3272-#3274) are stuck behind kimi's forced `agent:kimi` label.
2. **minimax 429 persisted** (5 agent + 2 rate_limit failures) — agent on 20h cooldown.
3. **opencode empty-output-exit0** (4× deepseek-v4-flash-free) — agent exits with code 0 but no JSON output.
4. **Claude "session limit" misclassified** (sonnet 2×, opus 3×) — still classified as `failed` not `rate_limit`. #3272 filed but stuck on kimi 429.
5. **Codex gpt-5.3 account restriction** (2×) — "not supported when using Codex with a ChatGPT account".
6. **Router LLM pool timed out** at 02:39:04 — tried minimax/haiku (20h cooldown), wasted 45s before weighted round-robin fallback selected opencode. This is the same task running this review.
7. **Watchdog triggered** at 02:39:24 — tick stalled 79s (threshold 60s) during worktree creation + dispatch.
8. **Multi-agent degradation warning** persistent: codex=persisted, kimi=agent_error, minimax=persisted.

## Stuck / Blocked Tasks

| Task | Status | Agent/Model | Issue |
|------|--------|-------------|-------|
| internal:151442 | blocked | opencode/gpt-5-mini | Self-improvement (old, Jun 2). Children done but auto-unblock failed. |
| #3272 | new | — (was kimi) | claude session limit misclassification — 5 attempts, all kimi 429 |
| #3273 | blocked | — (was kimi/sonnet) | normalize_status missing aliases — waiting on PR #3275 contributor |
| #3274 | blocked | — (was kimi/opus) | opencode false-positive rate_limit — waiting on PR #3275 contributor |
| internal:151994 | blocked | claude/sonnet | Bean close daily — escalated after 6 retries |
| internal:152092 | new | — | Not yet routed (cooled pool) |

**Note:** #3273 and #3274 have PR #3275 from contributor @Jah-yee, but review requested splitting into separate PRs. #3276 was opened as an alternative with the split. Owner set ~24h hold for contributor response.

## Routing Accuracy

- LLM routing unavailable for most of the period — all agents in the routing LLM pool (kimi, minimax, codex) were cooled.
- Weighted round-robin fallback selected opencode (weight 0.2) when LLM pool timed out.
- Effecitve single-agent mode for execution: only claude sonnet/opus + opencode deepseek-v4-flash-free are available.
- **Router LLM selected minimax/haiku despite 20h cooldown** — wasted 45s before timeout. The pool index should skip cooled agents.
- The `agent:kimi` labels on #3272-#3274 are now blocking those tasks since kimi is on cooldown. The engine clears the label on failure, but the router keeps re-selecting kimi. Root cause likely the label override reapplied by issue sync.

## Performance

- **Watchdog triggered** at 02:39:24 — tick stalled 79s. Caused by worktree creation + opencode dispatch during routing cooldown recovery.
- **Router LLM timeout** (45s minimax/haiku) — contributed 45s of the 79s stall. Fallback to weighted round-robin succeeded.
- **GitHub GraphQL** operations appear healthy (no EOF errors observed today).
- **SQLite query latency** minimal across all operations (<1ms for rate limit queries).

## Evening Update

### What Shipped (Afternoon)

6 additional commits landed since the morning review, plus **orch upgraded to v0.80.2**:

| Commit | Description |
|--------|-------------|
| `1834788a` | fix(parser): normalize_status aliases + detect_rate_limit word-boundary guard (#3279) |
| `4cb7b176` | ci+review: trigger CI on pull_request, fix sandbox image, add review-pr-ci recipe |
| `6a748d09` | chore(review): sandboxed external-PR review workflow |
| `5f9f459d` | docs(agents): require clone+execute inside Docker for external PRs |
| `831a2289` | docs(agents): policy for reviewing external-contributor PRs |
| `b5afd534` | bug(runner): claude 'session limit' still classified as failed → #3278 |

**Issues closed this afternoon:** #3272 (claude session limit misclassification), #3273 (normalize_status missing aliases).

**PRs merged:** #3278 (maintainer fix for claude session limit), #3279 (contributor @Jah-yee parser fixes — merged after review split).

### External PR Review Security Workflow

A full secure review workflow was added for external-contributor PRs:
- `CLAUDE.md`: hard policy — no fork clones, no code execution outside Docker, new deps = immediate no-merge
- `scripts/review/Dockerfile.fetch` + `Dockerfile.run` — two-stage sandboxed execution (network-gated fetch → offline run)
- `just review-pr <N>` recipe — automated hooks-check + Docker spin-up + cleanup
- `scripts/review/hooks-check.sh` — tripwire for `.cargo/config.toml`, `build.rs`, CI workflows, shell scripts
- CI now triggers on `pull_request` for test validation (but not `pull_request_target` — fork secrets remain safe)

### Operational Issues Filed (Evening)

| Issue | Title | Severity |
|-------|-------|----------|
| [#3283](https://github.com/gabrielkoerich/orch/issues/3283) | bug(runner): claude "weekly limit" misclassified as failed — reset timestamp not parsed | high |

**#3283 root cause:** `detect_rate_limit()` in `src/engine/runner/agents/mod.rs` lacks `"weekly limit"` in its pattern list. Claude messages like `"You've hit your weekly limit · resets Jun 9 at 1am (America/Sao_Paulo)"` fall through to a generic `Failed` classification — no cooldown is set, the 3-day reset timestamp is discarded, and orch retries immediately instead of waiting. Fix: add `"weekly limit"` alongside the existing `"session limit"` entry (same `parse_retry_at` logic applies). Tasks affected: 152324, 152327, 152331.

### Current Cooldown State (Evening)

| Key | Remaining | Reason |
|-----|-----------|--------|
| `codex:gpt-5.3` | 17h | persisted |
| `kimi` | 22h13m | persisted (billing) |
| `minimax` | 1d21h | persisted |
| `minimax:opus` | 1d21h | persisted |
| `opencode/deepseek-v4-flash-free` | 1d3h | persisted |
| `opencode/mimo-v2.5-free` | 4h26m | persisted |
| `opencode/minimax-m3-free` | 4h58m | persisted |
| `opencode/nemotron-3-ultra-free` | 8h11m | persisted |

**ALL opencode models are cooled at time of this review** — all agents in the routing pool hit cooldowns simultaneously. Effective pool for next ~4h: claude sonnet/opus + codex (gpt-5.4/gpt-5.5 only).

### Remaining Stuck Tasks

| Task | Status | Age | Issue |
|------|--------|-----|-------|
| #3281 | blocked | 4h, 5 attempts | control: split oversized messages — opencode failing on all attempts |
| #3274 | blocked | 1d, 3 attempts | opencode false-positive rate_limit (#3279 partially addressed word-boundary fix) |
| internal:151442 | blocked | 4d | Self-improvement — children done but auto-unblock still stale |

### Task Run Summary (Full Day)

| Agent | Model | Success | Failed | Other |
|-------|-------|---------|--------|-------|
| claude | sonnet | 74 | 4 | — |
| opencode | nemotron-3-ultra-free | 43 | 4 | 1 parse_error, 1 timeout |
| codex | gpt-5.5 | 25 | 2 | 1 blocked |
| codex | gpt-5.4 | 18 | — | 1 rate_limit |
| claude | opus | 17 | 5 | 3 blocked |
| opencode | deepseek-v4-flash-free | 10 | 3 | — |
| opencode | mimo-v2.5-free | 10 | 3 | 2 aborted, 2 timeout |
| opencode | minimax-m3-free | 8 | 2 | 3 timeout |
| minimax | opus | 0 | 6 | — (all failed) |
| codex | gpt-5.3 | 0 | 4 | — (model restricted) |

**Activity totals (24h):** 315 dispatches · 222 pushes · 103 review starts · 89 review decisions · 86 PR creates · 40 errors · 9 timeouts · 3 auto-unblocks.

## Tomorrow's Priorities

1. **Fix #3283 (claude weekly limit misclassification)** — add `"weekly limit"` to `detect_rate_limit()` patterns alongside `"session limit"`. The `parse_retry_at` logic already handles the reset timestamp format; the fix is a one-line addition + regression test. Without it, 3-day cooldowns are discarded and orch retries immediately on weekly limit exhaustion.
2. **Fix #3274 (opencode rate_limit false-positive)** — the word-boundary guard in #3279 may not be sufficient; the root cause is nextest output containing test function names with `rate_limit`. Needs a smarter check (e.g., JSON output gate or test-output exclusion pattern).
3. **Fix #3281 (control oversized messages)** — 5 attempts, still blocked. Assign to claude when opencode remains cooled. Message chunking is a straightforward string-split task.
4. **Unblock internal:151442** — 4-day-old self-improvement task, children done, auto-unblock stale. Check `orch task unblock all`.
5. **Monitor kimi/minimax cooldown recovery** — kimi clears in ~22h, minimax in ~1d21h. When they recover, re-route any remaining backlog.
6. **Verify #3279 parser fix** — `detect_rate_limit` now uses word-boundary guard. Watch for any residual false-positives on `rate_limit` in test output over the next cycle.

---

*Morning section prepared by internal:152037 (attempt 4). Evening update by internal:152385 (attempt 3).*

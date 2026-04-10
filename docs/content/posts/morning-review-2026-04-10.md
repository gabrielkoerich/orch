+++
title = "Morning Review — 2026-04-10"
date = 2026-04-10
description = "Reliability push enters day 4: 48+ commits overnight, #2317 (opencode silence detection) closed, no open issues. Major CLI/service version gap: 0.60.159 vs 0.61.3 — service crossed a minor version boundary. Throughput up ~30% across the board."
+++

# Morning Review — 2026-04-10

## Recent Commits & Progress

The reliability push continued overnight with another strong batch. Since the Apr 9 evening retrospective, commits include:

- **`fb57b4b9`** fix(cli): handle multi-byte UTF-8 in `truncate_err` (#2355) — closes #2346
- **`fdb16b74`** fix: `graceful_shutdown_timeout` config is parsed but never enforced — closes #2350
- **`7c57cc76`** fix: chunk batch issue state queries (#2353) — closes #2349
- **`d8e046a7`** fix: accumulate token counts instead of overwriting on retries (#2352) — closes #2347
- **`5cd40434`** fix(security): scan markdown bullet lines instead of skipping them (#2351) — closes #2348
- **`5f11441b`** perf: batch `is_pull_request` checks in `scan_mentions` with `join_all` (#2342)
- **`f506c686`** fix(cli): fall back to rotated log files when primary log is empty (#2343)
- **`6781b31f`** fix: add allowlist validation inside SQL-building loop in `batch_set_fields`
- **`629bfd52`** fix: propagate git rev-list parse errors in `rebase_on_branch` (#2338)
- **`c6cdfc86`** fix: add allowlist validation inside SQL-building loops in `set_fields` (#2336)
- **`56eff734`** fix: recover push failures by rebasing on remote branch (#2335)
- **`63f2057f`** feat(chat): add control session cost tracking and stats (#2334)
- **`6748af50`** fix: discord_ws.rs tests use `unwrap()` on async `recv()` calls (#2327)
- **`1dbe5bf8`** fix: fail review flow when PR URL parsing fails (#2333)
- **`60915486`** fix: KV helper functions in `sync.rs` silently discard store failures (#2325)
- **`f07be5eb`** fix: capture.rs tests use `unwrap()` on HashMap lookups (#2324)
- **`0c577f0d`** fix: silence detection fires at exactly 600s for all opencode sessions (#2317)

Notable security fix: **`5cd40434`** — the markdown security scanner was skipping bullet lines entirely, meaning secrets embedded under bullet points (the most common documentation pattern) were invisible to the scanner. This was a meaningful leak surface.

---

## Operational Health

**Overall: healthy.** Pipeline throughput is up ~30% across all metrics versus yesterday. No open issues, no blocked tasks, no active cooldowns.

### Critical concern: CLI/service version boundary crossed

```
CLI:     0.60.159
Service: 0.61.3  ✗ mismatch — service crossed minor version boundary
```

The service is now on `0.61.x` while the CLI is on `0.60.x`. This is no longer a patch-level drift — a minor version bump signals new APIs or changed behavior that the CLI may not be compatible with. The `orch log 200` command returning "No log files found" this morning is consistent with this: the CLI-side fix (#2343) landed in the repo but the installed CLI is too old to use it.

**Action required:**
```bash
brew upgrade orch && brew services restart orch && orch version
```

### Apr 9 retro follow-ups

| Priority | Status |
|----------|--------|
| Fix #2317 (opencode silence detection at 600s) | **Done** — `0c577f0d` landed overnight. Highest-priority item resolved. |
| Upgrade CLI/service | **Still open** — now more urgent: 0.60.159 vs 0.61.3 (minor boundary). |
| Investigate olm/gemma4 | **Inconclusive** — absent from 24h run stats entirely. May not have been dispatched yet today, or was quietly removed from routing. |
| Verify JSON output improvement (#2287) | **No regression observed** — parse errors not flagged in this morning's review. |
| Watch for silence detection regressions (#2317) | **Monitor** — fix landed but too early to confirm behavior in production. |

### 24h run outcomes

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 97 |
| codex | gpt-5.3-codex | success | 55 |
| kimi | opus | success | 50 |
| minimax | opus | success | 39 |
| opencode | minimax-m2.5-free | success | 20 |
| opencode | github-copilot/gpt-5-mini | success | 13 |
| claude | opus | success | 10 |
| opencode | nemotron-3-super-free | success | 9 |
| claude | sonnet | failed | 8 |
| claude | sonnet | timeout | 5 |
| codex | gpt-5.3-codex | failed | 4 |
| kimi | sonnet | success | 4 |
| minimax | sonnet | success | 4 |
| opencode | github-copilot/gpt-5.4 | failed | 4 |
| opencode | nemotron-3-super-free | failed | 4 |
| claude | sonnet | rate_limit | 3 |
| minimax | opus | timeout | 3 |
| minimax | opus | rate_limit | 2 |
| kimi | opus | rate_limit | 3 |
| kimi | opus | timeout | 2 |

Notes:
- **opencode: minimax-m2.5-free is the top opencode model** (20 successes, up from previous sessions). The silence detection fix may already be showing results — opencode completions that were previously killed at 600s can now land.
- **olm/gemma4 absent** — was 2/3 success/failure in first 24h, now zero dispatches. Either routing changed or there's been no appropriate task type.
- **claude timeouts (5)** — new metric type appearing. These are distinct from `failed` — worth watching to see if this is a pattern or noise.
- **kimi/opus rate limits (3)** — same low level as yesterday. Generic backoff handling correctly.

### 12h task activity

| Event | Count |
|-------|-------|
| status_change | 1717 |
| dispatch | 506 |
| push | 408 |
| branch_delete | 320 |
| routed | 230 |
| review_start | 210 |
| review_decision | 197 |
| pr_create | 174 |
| error | 62 |
| rerouted | 57 |
| timeout | 11 |

Throughput is up ~30% versus yesterday's 12h window on every metric. The pipeline is processing significantly more work. Error count is down from 84 → 62 despite higher throughput, suggesting the quality of routing and execution continues to improve.

### Error log

`/opt/homebrew/var/log/orch.error.log` is empty (0 bytes). No current service-level errors.

---

## Open Issues

**None.** All recent bug reports are closed.

---

## Priorities for Today

1. **Upgrade CLI/service** — minor version boundary crossed (0.60.x → 0.61.x). The `orch log` command is broken due to this mismatch. Priority: high.
   ```bash
   brew upgrade orch && brew services restart orch && orch version
   ```

2. **Verify #2317 fix in production** — silence detection was firing at exactly 600s for opencode sessions. The fix landed overnight. After the CLI upgrade, run `orch log` and check whether opencode sessions are now completing normally rather than resetting. Watch for any sessions that hit exactly 600s.

3. **Investigate claude timeouts (5)** — a new `timeout` outcome appeared for claude/sonnet (5 cases). This wasn't present in yesterday's stats. Determine whether these are silence-detection timeouts, hard timeouts, or a new failure mode, and whether the count is growing.

4. **Investigate olm/gemma4 absence** — disappeared from routing after first 24h appearance. Confirm whether this is expected (routing removed, agent not configured) or a silent routing failure.

5. **Monitor throughput stability** — 30% jump in dispatch volume is a positive sign but also a stress test. Watch error rates to ensure they don't climb proportionally with load.

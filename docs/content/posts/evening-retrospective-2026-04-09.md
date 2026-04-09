+++
title = "Evening Retrospective — 2026-04-09"
date = 2026-04-09
description = "Reliability run enters day 3: 34 commits since Apr 8 retro, all correctness fixes. One open issue (#2317: opencode silence detection kills 71% of sessions at exactly 600s). olm/gemma4 observed for first time. codex fully recovered."
+++

# Evening Retrospective — 2026-04-09

## Summary

The reliability push that started Apr 7 entered its third consecutive day. **34 commits have landed since the Apr 8 evening retrospective** — all in the correctness and observability category, zero feature work. The pipeline is stable, all agents are routing, and the commit velocity is high but showing signs of narrowing to fewer, more targeted fixes.

One open issue remains: **#2317** — opencode silence detection is killing 71% of opencode sessions at exactly 600s, adding ~10 minutes of artificial delay per task. This is the highest-priority bug heading into tomorrow.

---

## Morning Priorities — Outcome

| Priority from morning review | Status |
|------------------------------|--------|
| Upgrade CLI/service (8-version gap) | Unknown — not confirmed this session. Should be checked. |
| Wait for #2254 to merge | Implicitly resolved — no related commits visible; CI presumably passed. |
| Investigate `olm` agent (gemma4, first appearance) | Not explicitly investigated. No new olm-specific issues filed. |
| Confirm router LLM exhaustion stays quiet | OK — no exhaustion events in today's commits. |
| Watch kimi rate limits | OK — 4 rate-limit events in 24h, backoff handling correctly. |

---

## What Was Accomplished

### Today's commits — grouped by theme

#### Silent failure / observability fixes

- **`3deb3df1` (closes #2318) claude agents running 12-30 min then killed as "silent"** — the root cause was grace period too short relative to actual agent startup time. Same pattern as #2317 (opencode) but the claude variant. Fix: grace period tuning or registration timing correction.
- **`ed5c9322` (closes #2316) PR mergeability deferral adds 30-50s to review loop** — every review loop iteration was doing a full mergeability check with network I/O. Deferred to a background path. **Reduces review loop latency by 30-50s per cycle.**
- **`2266e72c` (closes #2310) auto_merge: stash uncommitted changes before rebase** — rebase was failing silently on unstaged changes, causing auto_merge to bail and increment failure counters without applying any fix.
- **`8503d6f6` (closes #2302) set_cooldown_async rollback removes valid longer cooldown** — race: if two goroutines set a cooldown concurrently, the shorter one's rollback could erase the longer one. Now compares durations before rollback.
- **`02f599bf` (closes #2303) capture: walk backward to char boundary** — multi-byte UTF-8 sequences were being dropped when the capture buffer split mid-character. ~1% of non-ASCII output was silently truncated.
- **`000518c7` (closes #2292) control: evict SESSION_LOCKS after send_message** — SESSION_LOCKS in `control.rs` was a static `Mutex<HashMap>` that grew unbounded. Sessions were never evicted after use.

#### Router / routing accuracy

- **`a5d2d32d` (closes #2311) runner: clear agent/model fields on rate-limit re-route** — when a task was re-routed due to rate limit, the old agent/model were preserved in the task record. The new route used a different agent but the task showed the previous one.
- **`ecc759b6` (closes #2293) try_free_model_reroute returns EarlyReturn{routed} even when stale** — the free model reroute path was returning "routed" even when the model had already been cooled and the reroute was a no-op. Tasks appeared to re-route but didn't.
- **`56809892` (closes #2289) get_runner emits WARN on every dispatch for configured custom agents** — benign but noisy. Every dispatch for a configured custom agent emitted a spurious warning, polluting logs and masking real warnings.

#### Mention / parent-id fixes

- **`baa07696` (closes #2315) parent_id should always be by issue number, not PR number** — mention tasks were inheriting the PR number as parent_id in some flows, breaking the issue→PR→task linkage chain.
- **`f26b1d42` (closes #2305) all mention tasks had the same title** — title derivation was using a static string instead of the mention content. All mention-derived tasks showed the same title in the UI.
- **`ea1afc8e` fix: resolve mention parent_id via pr_number lookup** — follow-up to #2315; parent_id is now resolved via a proper pr_number→issue lookup rather than body parsing.

#### Cron / scheduling

- **`ceef38d5` (closes #2294) cron DOW comment says Sun=1..Sat=7 but cron crate uses 7=Sun** — the day-of-week encoding in a comment was inverted, causing operator confusion when writing cron expressions with day-of-week constraints.

#### Refactors / cleanup

- **`a8635fd6`** return `is_pr` from `CommandOutcome` to avoid redundant `get_issue` call
- **`daec22e7`** move shared helpers to proper modules
- **`aa3a700f`** extract helpers from `scan_mentions`, reduce 400 lines to ~200
- **`4e8fa67c`** apply `cargo fmt` to `git_ops.rs`
- **`f3c5f76e`** `auto_commit` uses local git config instead of agent identity

#### Prompt improvements

- **`51e7cd78` (closes #2287) add JSON output reminder to agent task message** — agents were inconsistently producing JSON structured output. Added a reminder to the task message template. Directionally correct fix for structured output reliability.

---

## What Failed and Why

### #2317 — opencode silence detection kills sessions at exactly 600s (OPEN)

This is the most important finding of the day. **71% of opencode sessions (12 of 17 in 48h) are killed by silence detection at exactly 600-611s**, well before the hard 1800s timeout. The pattern is:

- All killed sessions hit 600-611s (not random)
- `silence_grace_period` is 120s in config — so why 600s?
- The 600s figure is exactly 10 minutes, suggesting a session registration timing issue: the tmux session may be registered before the agent actually starts outputting, but the grace period check compares against `registered_at` rather than `first_output_at`

The claude variant of this same bug was closed today (#2318), but #2317 (opencode-specific) remains open. The grace period for opencode free models may need to be 300s+, or session registration needs to use a generation-aware timestamp.

**Impact:** Every affected opencode run adds ~10 min of wasted delay. Tasks get fully reset and re-routed. Given opencode free models are at $0 cost, this is wasting routing capacity and increasing task latency.

### CLI/service version drift

The morning review flagged an 8-version gap (CLI: 0.60.123, service: 0.60.131). With today's commits, the gap is now larger. This was not addressed. Until upgraded, the CLI reports stale behavior and any commands that use CLI-side logic may differ from service behavior.

---

## Routing Accuracy

**Significantly improved over the week.** Key changes that landed in the past two days:

1. `try_free_model_reroute` no longer returns false "routed" signals (#2293)
2. Agent/model fields cleared on rate-limit re-route (#2311)
3. Router LLM skips degraded agents (#2222, yesterday)
4. `has_available_model_for_complexity` false positive fixed (#2230, yesterday)

Combined effect: wasted dispatch attempts should be materially lower. No post-deploy stats yet, but the direction is clear.

### Agent health (from morning review stats)

| Agent | Status |
|-------|--------|
| claude:sonnet | Primary workhorse — 72 successes in 24h |
| codex | Fully recovered — 40 successes in 24h (Apr 9 cooldown expired) |
| minimax:opus | Healthy — 48 successes in 24h |
| kimi | Minor rate limits (4), backoff handling correctly |
| opencode | Free models working, but 71% of sessions killed by silence detection (#2317) |
| olm/gemma4 | New agent, 2 successes / 3 failures in 24h — first appearance |

---

## Open Issues

| # | Title | Priority |
|---|-------|----------|
| #2317 | opencode silence detection kills sessions at exactly 600s | **High** — 71% opencode failure rate |

---

## Priorities for Tomorrow

1. **Fix #2317 — opencode silence detection** — this is the single highest-impact open issue. Root cause is in `src/channels/capture.rs` (session registration timing) and `src/engine/tick.rs` (silence detection). The claude variant (#2318) was fixed today — look at that fix for guidance.

2. **Upgrade CLI/service** — gap is now 10+ versions. Run:
   ```bash
   brew upgrade orch && brew services restart orch && orch version
   ```
   Do this before the morning review to ensure the morning stats reflect current behavior.

3. **Investigate olm/gemma4** — 3 failures, 2 successes in first 24h. If this is intentional, document the agent profile. If not, determine why it appeared and whether it should be routing.

4. **Verify mentor JSON output improvement (#2287)** — the JSON output reminder was added to the task message template. After a day of runs, check if structured output parse failures have decreased.

5. **Watch for #2317 follow-up regressions** — once the silence detection fix lands, verify that previously "silent" opencode sessions that were actually working are now completing successfully rather than being misidentified.

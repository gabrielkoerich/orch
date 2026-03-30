+++
title = "Morning Review -- 2026-03-30"
date = 2026-03-30T11:30:00Z
[extra]
job = "morning-review"
+++

## Summary

Extremely high-output night and morning — **22+ commits landed in the last 24 hours**, addressing the full kimi billing-cycle cascade, error classification improvements, routing weight configuration, and migration safety. The pipeline is healthy: minimax is the reliable workhorse (72 success runs), routing weights are now configurable, and the router now gates on cooldown availability. Remaining focus: 6 open bugs currently being worked on, plus a stale worktree metadata issue in the error log.

---

## Recent Activity (Last 24h)

### Infrastructure / Architecture

- `feat: configurable per-agent routing weights` — operators can now tune per-agent weights in config.yml; replaces the fragile LLM "distribute evenly" heuristic
- `feat: gate router on cooldown availability (#1266)` — router no longer selects cooled agents/models; prevents wasted dispatch
- `feat: separate runners for MiniMax and Kimi (claude CLI wrappers)` — MiniMaxClaudeRunner and KimiClaudeRunner now handle non-`"type":"result"` stream parsing correctly
- `fix: restore migration 012 checksum and split new column to 014` + `test: add migration idempotency tests` — migration safety fix + idempotency tests to catch future checksum breaks

### Bug Fixes

- `fix: remove 24h billing-cycle cooldown fallback from parse_retry_at` — cooldown fallback was interfering with vendor-specified retry-at timestamps
- `fix: stash uncommitted changes before rebase on startup (#1277)` — prevents startup rebase destroying agent work (#1245 companion fix)
- `bug: transport last_output persists across retries, replaying stale output (#1279)` — stale output from prior attempt was being replayed to review agent
- `bug: auto_unblock_count should reset when block reason changes (#1276)` — counter sharing between CI recovery and auto-unblock mechanisms fixed
- `fix: restore ci_recovery_count usage in handle_review_changes (#1284)` — counter separation fix to avoid mixing auto_unblock vs CI recovery counts
- `github(http): request latest check-runs per name (filter=latest) (#1283)` — avoids picking oldest stale check-run when multiple exist per name
- `bug: incomplete task_runs with NULL outcome block auto-unblock (#1272)` — NULL-outcome runs no longer block the auto-unblock mechanism
- `fix: clear agent/model on auto-unblock (#1269)` — ensures re-routing picks fresh assignment after unblock
- `bug: has_commits_ahead returns false on git failures — skips push + PR creation (#1291)` — git errors were silently blocking PR creation
- `bug: model_for_complexity returns None when all pool models cooled (#1278)` — dispatch was proceeding with empty model; opencode exits 0 silently in this case
- `fix: re-route external tasks that complete without code changes` — tasks that reported success without a PR now get re-routed to a different agent
- `perf: single git fetch per project at startup (#1274)` — startup time reduced (was N sequential fetches)
- `fix: truncate stdout/stderr to 500 chars in router LLM failure log (#1273)` — very long stdout was polluting router failure logs

---

## Operational Health

### Agent Performance (last 24h task_runs)

| Agent | Model | Success | Failed | Rate-limit | NULL | Notes |
|-------|-------|---------|--------|------------|------|-------|
| minimax | opus | 72 | 3 | 2 | 4 | Primary workhorse |
| opencode | gpt-5-mini | 39 | 3 | 0 | 2 | Healthy |
| claude | sonnet | 12 | 3 | 2 | 6 | 6 NULL needs watch |
| opencode | (empty) | 2 | 7 | 0 | 0 | `model_for_complexity=None` bug — **now fixed in #1278** |
| kimi | opus/sonnet | 0 | 2 | 5 | 0 | Billing exhausted, cooldown active |
| codex | gpt-5.2-codex | 3 | 1 | 1 | 0 | Low volume, healthy |

**Notable:** The 7 `opencode/empty-model` failures (dispatch with null model) are explained by #1278 which was just fixed. These should disappear. Claude has 6 NULL-outcome runs — these typically indicate mid-session cleanup races; worth watching but not alarming given the high success count.

### Task Activity (last 12h)

- 1,356 status changes — high throughput day
- 401 dispatches
- 65 errors — low relative to activity
- 51 review decisions, 50 PR creates — review pipeline healthy
- 5 auto-unblocks (small, expected)

### Log Errors

Recurring in brew error log:
```
fatal: not a git repository: /Users/gb/Projects/orch/.git/worktrees/gh-issue-1005-...
fatal: not a git repository: /Users/gb/Projects/orch/.git/worktrees/gh-issue-1130-...
fatal: not a git repository: /Users/gb/Projects/orch/.git/worktrees/gh-issue-1251-...
```

These are stale git worktree metadata entries in the *main* project directory (not `~/.orch/worktrees`). The worktrees were cleaned up but `.git/worktrees/` metadata entries were never pruned. This is benign (doesn't block any task) but spams the log. A `git worktree prune` in the main project dir would clear them. Related to #1225 (startup reconciliation prune fix) — appears the prune runs in `~/.orch/worktrees` but not the user project's `.git/worktrees/`.

---

## Stuck / Blocked Tasks

| ID | Status | Agent | Tries | Title |
|----|--------|-------|-------|-------|
| #1245 | blocked | minimax | — | startup rebase destroys worktree with unstaged changes (15h blocked) |
| #1247 | in_progress | claude | 4 | silence detection spurious needs_review + review cycle |
| #1250 | in_progress | opencode | 3 | handle_failover no fallback path never records agent cooldown |
| #1267 | in_progress | claude | 3 | blocking std::fs calls in async code |
| #1271 | in_progress | claude | 2 | engine marks code tasks done when no PR |
| #1292 | in_progress | claude | 2 | detect_rate_limit false positive |
| #1244 | in_progress | claude | 3 | model cooldowns in-memory only (lost on restart) |

**#1245 is blocked** (15h) — the startup rebase worktree destruction bug. The companion fix (#1277, stash before rebase) landed yesterday, but #1245 itself is blocked and may need manual unblock. Worth checking if the fix would resolve the blocked state.

**#1247 at 4 tries** — silence detection race condition (kill vs exit-0). Claude is on attempt 4; if this fails again, the task will likely be escalated to blocked.

---

## Retro Follow-ups

From the 2026-03-29 evening retro:

| Item | Status |
|------|--------|
| opencode silent exit-0 (model=None path) | ✅ Fixed — #1278 merged |
| Stale copilot models (#1257) | Not in current open list — likely merged |
| #1244 in-memory cooldowns lost on restart | Still open, in_progress (claude) |
| #1227 + #1232 auto_unblock counter sharing | ✅ Fixed — #1276 and `fix: restore ci_recovery_count (#1284)` |
| #1241 channel thread bindings (4 opencode failures) | Not visible in open issues — likely resolved or blocked-then-closed |
| Stale KV cooldown entries | Not explicitly addressed — stale `cooldown:kimi:k2p5` may still be in KV |

---

## Today's Priorities

1. **Monitor #1245 (blocked, 15h)** — startup rebase fix (#1277) is deployed; manually unblock #1245 if the engine doesn't auto-recover it, since the fix is already in place.
2. **#1244 (in-memory cooldowns)** — SQLite persistence for cooldowns is still the most impactful durability fix remaining. Target closure today.
3. **#1247 (silence detection spurious review, 4 tries)** — high retry count; watch for resolution or escalation to blocked.
4. **Watch opencode/empty-model failures** — should drop to zero after #1278 is in production. If they persist, check whether the fix actually resolves the dispatch path.
5. **Stale git worktree metadata** — the `fatal: not a git repository` log spam. Investigate whether startup reconciliation should also run `git worktree prune` in user-managed project directories. Low urgency, but the log noise is misleading.
6. **Stale KV cooldown entries** — verify `cooldown:kimi:k2p5` and other expired entries are cleaned up. If not, the cooldown expiry check may not be evicting them.

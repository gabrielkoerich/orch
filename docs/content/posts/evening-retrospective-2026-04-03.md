+++
title = "Evening Retrospective -- 2026-04-03"
date = 2026-04-03T22:00:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

**Exceptional day** — 18 commits landed, 20 issues closed. Major reliability wave across routing, review, channels, and observability. Agent success rate improved to **86%** (179/209 runs), up from 83% this morning. Version sync resolved: CLI and service both at **0.58.2 ✓**. SQLite OOB panics remain the sole critical unresolved issue.

---

## What Was Accomplished

### Commits Landed (18 total, last 12h)

| SHA | Summary |
|-----|---------|
| `d29fb92` | test: add opencode to integration_review.rs test coverage (#1731) |
| `4aa56a1` | bug: review evaluates wrong diff/summary after reroutes or rebases (#1724) |
| `6341ee9` | fix: strengthen review agent self-fix for auto-fixable CI failures (#1729) |
| `2dc29bc` | fix: add topic_id to channel subscriptions for topic-aware delivery (#1726) |
| `3ccceb6` | docs: align control session prompt with supported commands (#1725) |
| `922d80b` | bug: race condition in capture service tick loop (#1718) |
| `0a21c53` | warn: log warning on /tmp/.orch fallback in TaskRunner::new (#1714) |
| `12746a0` | fix(engine): warn on invalid config values instead of silent failure (#1715) |
| `08b578d` | feat: job completion notifications via Telegram (#1638 / #1713) |
| `cac547b` | fix: change to COLLABORATOR in gh association |
| `69044c8` | check: only owner/contributor gh issues and comments sync (#1711) |
| `b5a5bca` | bug: Router::from_config() on async executor in error fallback (#1707) |
| `e7ff044` | bug: session continuation flag silently dropped on shell format change (#1706) |
| `b979f29` | fix: prefer skipped uncooled agent over cooled fallback in round-robin (#1705) |
| `45bd96e` | refactor: deduplicate free-model retry logic (3 copies → 1 helper) (#1702) |
| `c54d9f8` | fix: suppress expected tmux kill-session warnings in logs (#1701) |
| `448fb3c` | feat: allow reroute/retry with specific agent and model (#1699) |

### Issues Closed (20)

Key closures: #1728 (review CI exhaustion), #1722 (wrong review diff), #1721 (topic context dropped in channels), #1720 (stale control docs), #1716 (capture race), #1715 (silent config errors), #1714 (/tmp fallback silent), #1711 (non-contributor sync), #1707 (async executor in router), #1706 (session flag dropped), #1705 (cooled agent fallback), #1702 (retry dedup), #1701 (log noise), #1700 (oblivion router stuck), #1698 (reroute control), #1693 (token budget skip), #1692 (#1691 panics addressed), #1690 (token leak in logs).

### Morning Priorities — Resolution

| Priority | Status |
|----------|--------|
| SQLite OOB panics (9 active) | **NOT resolved** — still actively occurring (see below) |
| `brew upgrade orch` (version sync) | **Resolved** — 0.58.2 in sync ✓ |
| #1665 dispatch (channel transport collision) | **Resolved** — fixed via #1706, #1711, #1721 family of PRs |
| opencode:opus model assignment | **Partially resolved** — #1705/#1706 addressed routing; 0 opencode:opus failures today |
| internal:34220 (blocked code quality) | **Not resolved** — still blocked at max review cycles |

---

## Operational Health

### Agent Performance (last 12h, 209 runs)

| Agent | Model | Success | Failed | Rate | Notes |
|-------|-------|---------|--------|------|-------|
| minimax | opus | 47 | 0 | 100% | Best performer |
| opencode | qwen3.6-plus-free | 25 | 2 | 93% | Reliable free tier |
| codex | gpt-5.2-codex | 22 | 2 | 92% | Solid |
| opencode | copilot/gpt-5-mini | 21 | 0 | 100% | Primary fallback |
| claude | sonnet | 12 | 5 | 71% | 5 failures — see note |
| codex | gpt-5.3-codex | 10 | 0 | 100% | Clean |
| kimi | opus | 8 | 4 | 67% | 4 failures, billing resets |
| opencode | copilot/gpt-5.4 | 7 | 1 | 88% | Minor |
| opencode | copilot/claude-opus-4.6 | 6 | 0 | 100% | Stable |
| opencode | nemotron-3-super-free | 5 | 0+2 push_failed | 83% | Push scope issue |
| opencode | minimax-m2.5-free | 4 | 1 | 80% | Fine |

**Overall: 86%** (179 success / 209 runs). Improvement from 83% this morning.

**opencode with no model (5 failed, 4 success):** Empty model string in 9 runs — likely a routing edge case where model resolution fails silently. The 4 successes suggest the agent infers a default model in some configs. Root cause unclear.

**claude/sonnet 5 failures + 2 rate_limits:** Credits still running thin. `out_of_credits` and `org_level_disabled` events in the last 12h. Cooldown system handling correctly; other agents absorbing load.

**kimi 4 failures:** Billing cycle resets. The 24h cooldown is still too short for monthly billing cycles, but kimi's failure rate (67%) is improving vs prior days.

### Push Failures (3)

- `opencode/nemotron-3-super-free`: 2 failures in `bean` repo — OAuth App token lacks `workflow` scope, blocking push of `.github/workflows/` files. Persistent issue (#1681 class of bug).
- 1 failure: `git` not found in path during push (os error 2 — transient environment issue).

### Task Status

| Status | Count |
|--------|-------|
| done | 758 |
| in_progress | 6 |
| needs_review | 1 |
| blocked | 2 |

Active work: internal:40365 (code quality), internal:40366 (code dev improvements), internal:40367 (this retro), #1732/#1733 (token budget bugs), #1734 (budget tracking bug).

---

## Failures and Root Causes

### 1. SQLite OOB Panics — STILL UNRESOLVED (Critical)

```
thread 'sqlx-sqlite-worker-N' panicked at sqlx-sqlite-0.8.0/src/row.rs:43:46:
index out of bounds: the len is 56 but the index is 56
```

All 10 SQLx workers (0–9) are panicking. Error log mtime confirms this is **current** (Apr 4 01:52 UTC). The panics survive via worker pool auto-recovery, but they indicate a column index mismatch in a SQL query — likely a `SELECT *` against a table that has been extended by a migration, while an old struct maps fewer columns than the table now has.

**This is the #1 operational risk.** When all 10 workers panic simultaneously, any concurrent query will fail until workers restart. This could cause task dispatch gaps, lost metric writes, or missed review dispatches.

Related to #1623 (panics in production paths), but the root cause is different — this is in the SQLx row deserialization layer, not in `unwrap()`/`expect()` calls.

### 2. Stale Worktree Metadata (Low Severity)

```
fatal: not a git repository: /Users/gb/Projects/orch/.git/worktrees/gh-issue-1005-*
fatal: not a git repository: /Users/gb/Projects/orch/.git/worktrees/gh-issue-1130-*
fatal: not a git repository: /Users/gb/Projects/orch/.git/worktrees/gh-issue-1251-*
```

Stale `.git/worktrees/` metadata entries for deleted worktrees (issues 1005, 1130, 1251). Git complains on each startup reconciliation. Benign but noisy — can be cleaned with `git worktree prune`.

### 3. Token Budget Issues (#1733, #1734 — New)

Two new bugs filed today around token budget enforcement:
- #1733: Pre-run gate sends over-budget tasks into review instead of blocking them
- #1734: `task_runs` records budget-exceeded runs as `success`, hiding them from analysis

These explain the persistent budget overruns flagged in prior days (2.5M token tasks). The detection works but the enforcement path is wrong.

### 4. Router LLM Response Parsing (#1732 — New)

The LLM router (haiku) is producing 22KB responses that include hook event JSON mixed with structured routing output. The parser discards non-zero exit structured output. When the router LLM fails, the error diagnosis is lost. Bug filed for fixing.

---

## Routing Analysis

Routing worked well today. No oblivion-class failures (last one was #1700, closed). The #1705 fix (prefer uncooled agents over cooled fallbacks in round-robin) is working — opencode:opus disappeared from failure reports entirely.

Issue #1723 (review re-dispatch assigns incompatible model) has a PR open (#1730, needs_review). This was causing review agents to get `opencode:opus` which then fails. Once merged, review dispatch should be cleaner.

**Router LLM producing very long responses:** The haiku router is capturing full SDK hook output (22826 chars) alongside the routing JSON. This is likely a shell output capture issue — the router invocation is capturing hook payloads meant for the SDK transport layer. Not yet a routing failure but indicates the router parse path needs to be more robust (related to #1732).

---

## Open Issues (5 active)

| # | Title | Priority |
|---|-------|----------|
| #1734 | bug: task_runs records budget-exceeded runs as success | High — hides failures |
| #1733 | bug: pre-run token budget gate sends over-budget into review | High — wrong enforcement |
| #1732 | bug: router LLM non-zero exits discard structured stdout | Medium — diagnosis lost |
| #1723 | bug: review re-dispatch assigns incompatible model | High — PR #1730 in review |
| #1623 | Panics from unwrap()/expect() in production paths | High — related to OOB panics |

---

## Blocked Tasks

| Task | PR | Agent | Cycles | Notes |
|------|----|-------|--------|-------|
| internal:34220 | #1660 | opencode/opus | max | Code quality review — max cycles exceeded |
| #1623 | #1626 | opencode/opus | max | Panics fix — max cycles exceeded |

Both blocked at max review cycles. Both have PRs. May need human review or cycle reset.

---

## Tomorrow's Priorities

1. **SQLite OOB panics** — Investigate `row.rs:43:46` — find the query with a column index mismatch (likely a `tasks` table column added in migration without updating the corresponding Rust struct). This is the #1 risk: all 10 workers panicking simultaneously means brief windows where all DB queries fail.

2. **Merge #1730** (review re-dispatch model fix) — currently in `needs_review`. Once merged, should eliminate the incompatible model issue during review cycles.

3. **Token budget enforcement fixes** (#1733, #1734) — Agents are processing these now. Watch for PRs and review quickly. The budget enforcement path is broken end-to-end.

4. **Stale worktree metadata** — Run `git worktree prune` in `/Users/gb/Projects/orch` to clean entries for issues 1005, 1130, 1251. This is a 30-second cleanup that reduces log noise permanently.

5. **Review internal:34220 and #1623** — Both blocked at max cycles. Check if PRs are close to mergeable and either reset cycles or close the tasks.

6. **OAuth `workflow` scope** — bean repo tasks keep failing to push `.github/workflows/` files. Either add the scope to the token or configure agents not to create workflow files in that repo.

+++
title = "Evening Retrospective — 2026-04-24"
date = 2026-04-24
description = "Daily evening retrospective: cooldown backoff correctness fix, merge-conflict rerouting, auto_close removal, dead Copilot model discovery, and audit data quality issues filed."
+++

# Evening Retrospective — 2026-04-24

Focused correctness day. Four commits landed addressing cooldown backoff, merge-conflict handling,
task lifecycle semantics, and test reliability. Three new bugs were discovered and filed against
audit data quality and model validation.

## What Was Accomplished

### Commits (last 12 hours)

| Commit | Description |
|--------|-------------|
| `dc88e160` | **fix(cooldown)**: past `retry_at` timestamp skips exponential backoff (`#3004`) |
| `c4c96688` | **fix**: remove `auto_close_task_on_approval` — tasks done only when PR merges |
| `0a7bf86a` | **fix**: route merge conflicts to task agent instead of blocking or looping |
| `9c41cb02` | **fix(tests)**: eliminate real network calls to Discord and GitHub APIs |

**Most impactful fix:** `dc88e160` — `record_agent_failure_with_message()` was comparing a stale
`retry_at` timestamp against the current time after every failure. When `retry_at` was in the past
(e.g., an expired vendor hint), the function returned early before applying exponential backoff.
Net effect: after a rate-limited agent recovered, its *next* failure would reset the backoff clock
to zero rather than accumulating. Correct behavior now: `retry_at` only applies when it's in the
future; otherwise, normal `record_agent_failure()` logic runs.

**Merge-conflict rerouting (`0a7bf86a`):** Previously, merge conflicts detected in `review_poll`
either blocked tasks outright or looped. They now re-route to the original task agent for
resolution, which is the correct operational behavior.

**`auto_close_task_on_approval` removal (`c4c96688`):** Approval of a PR was closing tasks before
the PR actually merged, creating false `done` states. The engine now marks tasks done only on
actual PR merge.

**Test network isolation (`9c41cb02`):** Tests that made live calls to Discord and GitHub APIs
introduced flakiness tied to external availability. All replaced with deterministic stubs.

## What Failed (and Why)

### Task runs (last 12 hours)

| Agent | Model | Success | Failed | Other |
|-------|-------|---------|--------|-------|
| claude | opus | 14 | 1 | 1 parse_error |
| claude | sonnet | 9 | 1 | — |
| codex | gpt-5.3-codex | 8 | — | 1 parse_error |
| minimax | opus | 8 | — | — |
| glm | opus | 5 | 1 | 1 timeout |
| kimi | opus | 1 | — | — |
| opencode | github-copilot/claude-opus-4.6 | — | 1 | — |
| opencode | github-copilot/gpt-5.4 | — | 1 | — |

Overall failure rate is low (~10%). The two `opencode` failures are on dead Copilot model identifiers
(`claude-opus-4.6`, `gpt-5.4`) — see issue #3010 below.

GLM is recovering slowly (5 successes today after yesterday's suppression from extended backoff).
One timeout and one failure remain but the trend is positive.

## Routing Accuracy

Routing decisions today were generally sound:
- High-volume success lanes: `claude/opus` (14/16), `codex/gpt-5.3-codex` (8/9), `minimax/opus` (8/8)
- Degraded lanes: `opencode` on Copilot models wasting dispatch slots; `glm` improving but still unreliable

The dead Copilot model problem (#3010) is the main routing accuracy drag: the router dispatches
`github-copilot/gpt-5.4` and `github-copilot/claude-opus-4.6` which fail immediately with
`Model not found`. There are **52** such failures in the database.

## Morning Review Priority Check-in

| Priority | Status |
|----------|--------|
| Monitor GLM recovery | ✅ Improving — 5 successes today, extended backoff working |
| Investigate bean SSH failures | ❌ No code change — root cause still open |
| Fix #2881 | Already resolved (yesterday's work) |
| Evening retro gap | ✅ This retro addresses it |
| Tune LLM routing budget | ❌ No change yet — watchdog stall risk persists |

## Blocked Tasks

- `#2789` — GLM artifact collection. Still waiting.
- `internal:148540` — Self-improvement task. Hit dead Copilot model (#3010), then review exceeded threshold.
- `internal:148556` — Twitter bookmarks research. Agent returned `blocked` due to inaccessible data sources.
- `internal:148569` — Trading update. Blocked after review.
- Oblivion/Solana/keeper backlog (48 blocked total) — old tasks requiring human intervention.

## New Issues Filed Today

**#3010 — bug(router): model_map accepts invalid opencode model identifiers**  
`github-copilot/gpt-5.3`, `github-copilot/gpt-5.4`, `github-copilot/claude-opus-4.6` are in the
live model_map but return `Model not found` on every dispatch. 52 failed runs attributable to this.
Root cause: `RouterConfig::from_config()` loads model identifiers verbatim with no validation.

**#3011 — bug(audit): blocked task runs recorded as `success`**  
`classify_run_outcome()` maps `status == "blocked"` to `"success"`. This makes retrospective and
alerting queries that filter on `outcome != 'success'` silently miss agent-blocked tasks.

**#3012 — bug(runner): agent-returned blocked reasons not persisted**  
When an agent responds with `status=blocked` and a human-readable `summary`, the response handler
does not write the summary to `tasks.last_error` or `tasks.block_reason`. Operators see a blocked
task with empty error fields; the explanation is buried in `task_runs.parsed_response`.

Issues #3011 and #3012 are closely related (both affect how blocked outcomes are represented) but
have distinct code paths and can be fixed independently.

## Performance and Operational Notes

- No watchdog stalls observed in this window (unlike yesterday's 80s stall).
- 4 tasks in progress at time of writing; 2664 done overall.
- `orch.error.log` is 0B — no fatal errors.

## Priorities for Tomorrow's Morning Review

1. **Fix #3010 first**: dead Copilot model identifiers are actively burning dispatch slots. Either
   add model-pool validation to `RouterConfig::from_config()` or remove the dead entries from the
   live config. 52 wasted runs is a concrete cost.
2. **Fix #3011 + #3012 together**: classify blocked runs as `"blocked"` in `task_runs.outcome` and
   persist the agent-provided summary into `last_error`. These are straightforward response-handler
   patches with clear test coverage paths.
3. **Investigate LLM routing budget**: the 45s budget caused an 80s tick yesterday. Monitor if
   today's tick timing improved. If watchdog stalls continue, lower `llm_budget_secs` to 30s.
4. **Unblock `internal:148540`** (self-improvement): it was routed to a dead Copilot model before
   doing meaningful analysis. Once #3010 is fixed, requeue manually.
5. **SSH auto-merge failures for bean project**: `sign_and_send_pubkey` ED25519 errors need
   investigation. `GH_TOKEN` fix won't help SSH — separate auth path.

---

Prepared by Orch automation (internal task internal:148580).

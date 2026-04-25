+++
title = "Morning Review — 2026-04-25"
date = 2026-04-25
description = "Daily operational check-in: 4 fixes landed (cooldown backoff, merge-conflict reroute, auto_close removal, test isolation), watchdog stalls continue, new GHA billing failures on bean, two of three retro issues fixed."
+++

# Morning Review — 2026-04-25

## Recent Commits (since 2026-04-24 morning)

Four substantive fixes landed plus the evening retro post:

- `07f6817f` docs: evening retrospective 2026-04-24 (`#3013`)
- `9c41cb02` fix(tests): eliminate real network calls to Discord and GitHub APIs
- `0a7bf86a` fix: route merge conflicts to task agent instead of blocking or looping
- `c4c96688` fix: remove `auto_close_task_on_approval` — tasks done only when PR merges
- `dc88e160` bug(cooldown): past `retry_at` timestamp skips exponential backoff (`#3004`)

Notable:

- **`dc88e160` cooldown correctness:** `record_agent_failure_with_message()` was returning early
  when a stale (past) `retry_at` was set, skipping exponential backoff entirely. Now `retry_at`
  only short-circuits when it's in the future. This was the most impactful fix of the day —
  recovering rate-limited agents will now correctly accumulate backoff on subsequent failures.
- **`0a7bf86a` merge-conflict reroute:** merge conflicts in `review_poll` now re-route to the
  original task agent instead of blocking or looping. Closes a real operational hole.
- **`c4c96688` lifecycle correctness:** removing `auto_close_task_on_approval` means tasks are
  now `done` only on actual PR merge, not on PR approval. Fewer false-positive `done` states.

## Operational Health

### Service and logs

- **Watchdog stalls continue.** At 10:01 UTC: 90.4s slow tick (`stale_secs=99`), then 45.4s
  slow tick at 10:02 UTC. Same pattern as yesterday — `LLM routing budget exceeded — falling
  back to round-robin`, full tick over the 60s watchdog threshold. `router.llm_budget_secs`
  is still 45s; no config change has been made since the recommendation in yesterday's
  evening retro.
- **`/opt/homebrew/var/log/orch.error.log` is 0B** — no fatal errors this session.
- **New: GitHub Actions billing failures on bean.** `auto_merge::is_billing_failure()`
  triggered twice for the bean repo today: PR 855 at 08:36 UTC (`internal:148613`) and PR 856
  at 10:07 UTC (`internal:148618`). Both blocked for human intervention. The detector is
  matching legitimately on check-run annotations containing "account payments have failed" or
  "spending limit" — this is a real billing issue on the bean repo, not an orch bug.
- **Bean SSH ED25519 still failing.** `git fetch` during review at 10:06 UTC:
  `sign_and_send_pubkey: signing failed for ED25519 ".../default_id_ed25519.pub" from agent:
  agent refused operation`. Carryover from yesterday — `GH_TOKEN` fix won't help here, this
  is the SSH agent path.

### Task/run health (24h)

| Agent | Model | Success | Failed | Other |
|-------|-------|---------|--------|-------|
| minimax | opus | 23 | — | — |
| codex | gpt-5.3-codex | 17 | — | 1 aborted, 1 parse_error |
| claude | sonnet | 16 | 2 | — |
| claude | opus | 15 | 1 | 1 parse_error |
| glm | opus | 10 | 1 | 1 timeout |
| kimi | opus | 4 | — | 2 (other) |
| codex | gpt-5.4 | 2 | — | — |
| opencode | github-copilot/claude-opus-4.6 | — | 1 | — |
| opencode | github-copilot/gpt-5.4 | — | 1 | — |

**Healthy throughput**, ~93 successes vs. ~9 non-success outcomes in 24h. GLM is **clearly
recovering**: 10 successes today after yesterday's 5 — extended-backoff tier (`#2944`) plus
`dc88e160` are doing their job.

The two opencode failures are still on the dead Copilot model identifiers
(`claude-opus-4.6`, `gpt-5.4`) — see "Stuck / Blocked Work" below.

### `task_activity` (12h)

`status_change` 1058 · `push` 285 · `dispatch` 274 · `review_start` 240 · `review_decision` 234
· `error` 183 · `branch_delete` 60 · `pr_create` 49 · `routed` 38 · `rerouted` 3 · `timeout` 1.

### Cooldown / failure-count state

`failure_count:opencode=8` (highest), `kimi=7`, `glm:haiku=7`, then `kimi:opus=3`,
`opencode:github-copilot/gpt-5.4=3`, `opencode:github-copilot/gpt-5.3=3`,
`codex:gpt-5.2-codex=3`. Generic backoff is functioning — opencode is heavily cooled, which
explains the low dispatch count for that agent today.

## Stuck / Blocked Work

- **`internal:148540`** — self-improvement task, blocked because review agent exceeded failure
  threshold. Hit a dead Copilot model (`#3010`) on its first attempt, then the review loop
  itself failed. Needs human requeue once the dead-model situation is resolved.
- **`#2789`** — GLM artifact collection. Now 6 days blocked.

No other tasks currently blocked (Oblivion/keeper backlog from yesterday is still in the DB
but isn't actively contending).

## Retro Follow-up Status (from 2026-04-24 evening)

| Priority from retro | Status |
|---|---|
| Fix `#3010` (dead Copilot models) | **Closed without code fix** — config still contains the dead identifiers. See below. |
| Fix `#3011` + `#3012` (blocked-outcome misclassification) | ✅ **Both fixed and closed today** by codex agent (PRs landed, regression tests added) |
| Investigate LLM routing budget | ❌ No change — 90s tick recurred today |
| Unblock `internal:148540` | ❌ Still blocked |
| Investigate bean SSH ED25519 | ❌ Still failing (10:06 UTC) |

**On `#3010`:** the issue was closed yesterday (2026-04-24 23:08 UTC) but the live config
(`model_map.complex.opencode`, `model_map.medium.opencode`) still lists
`github-copilot/claude-opus-4.6`, `github-copilot/gpt-5.4`, `github-copilot/gpt-5.3` —
all three return `Model not found` on dispatch. Two more failures occurred in the 24h
window. Per CLAUDE.md, agents cannot edit config — this requires manual operator action
(remove the dead entries from `~/.orch/config.yml`) or a code fix that validates model
identifiers at config-load time. Filing a duplicate issue would be noise; flagging here
for visibility instead.

## Tasks Waiting on Owner Feedback

No open issues currently labeled `needs-feedback`.

## Priorities for Today

1. **Operator action: remove dead Copilot model identifiers from `~/.orch/config.yml`.**
   `github-copilot/claude-opus-4.6`, `github-copilot/gpt-5.4`, `github-copilot/gpt-5.3` are
   in `model_map.{complex,medium}.opencode` and continue to fail every dispatch. `#3010`
   was closed but the config wasn't updated. This is a one-line config edit; can't be done
   by an agent.
2. **Operator action: tune `router.llm_budget_secs` down from 45s.** Watchdog stalls have
   recurred for at least three consecutive days. Lowering to 30s falls back to round-robin
   sooner and keeps the tick under 60s. Also a config edit.
3. **Operator action: investigate bean SSH ED25519 agent failure.** The SSH agent is
   refusing operations for `default_id_ed25519.pub` during `git fetch`. Check
   `ssh-add -l` / agent forwarding / key permissions. Separate from `GH_TOKEN`.
4. **Operator action: investigate bean repo GHA billing.** Two PRs blocked for
   "account payments have failed" / "spending limit" — likely real billing on the bean
   repo's GitHub account, unrelated to orch.
5. **Requeue `internal:148540`** once #1 above is done.

## Issue Creation

**No new operational issues filed.** All actionable patterns observed today either:

- Already have an open/closed issue (`#3010` for dead Copilot, `#2789` for GLM artifacts)
- Require operator config changes that agents cannot make (LLM budget tuning, dead model
  removal)
- Are external/non-orch issues (bean billing, bean SSH agent)
- Are already addressed (`#3011`/`#3012` fixed today)

Filing additional issues would duplicate or be config-change noise.

---
Prepared by Orch automation (internal task internal:148617).

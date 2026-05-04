+++
title = "Morning Review — 2026-05-04"
date = 2026-05-04
description = "Daily operational check-in: SSH auth failures blocking push/review, opencode gpt-5.3-codex routing leak, throughput healthy, two open bug issues."
+++

# Morning Review — 2026-05-04

## Recent Commits (last 24h)

No new commits landed in the last 24 hours.

## Operational Summary

- Orch v0.70.26 is running. Overall throughput is healthy: ~139 task_runs in the last 24h with ~92% success rate.
- **SSH auth failure is the active operational issue** — `sign_and_send_pubkey` is failing for `/Users/gb/.ssh/default_id_ed25519.pub`, causing `git fetch` / `git push` to fail during review and auto-merge phases.
- LLM routing budget consistently exceeded on internal tasks (>45s), falling back to round-robin for all recent internal task dispatches.

## Task and Pipeline Snapshot

| Status | Tasks |
|--------|-------|
| `in_progress` | internal:148989 (this review) |
| `blocked` | 3052, 3051, internal:148850, internal:148540 |

**Open issues filed:**

- **#3052** `bug(runner): SSH auth failure in push permanently blocks tasks — should retry with backoff`
- **#3051** `bug(router): gpt-5.3-codex not filtered for opencode agent — is_known_unavailable_model only covers github-copilot/gpt-5.3 variants`

Both were created yesterday (2026-05-03) and are currently blocked (2 attempts each).

## Agent/Model Failure Patterns (last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 38 |
| codex | gpt-5.3-codex | success | 26 |
| opencode | github-copilot/gpt-5-mini | success | 25 |
| kimi | opus | success | 20 |
| **opencode** | **gpt-5.3-codex** | **failed** | **6** |
| kimi | opus | failed | 3 |
| opencode | github-copilot/gpt-5-mini | failed | 2 |

Notable: `opencode/gpt-5.3-codex` accounts for 6 failures — this is the issue tracked in #3051. The router is still routing opencode tasks to `gpt-5.3-codex` despite the model being unavailable.

## Log Highlights

- **SSH agent refusing ED25519 key**: `sign_and_send_pubkey: signing failed for ED25519 "...default_id_ed25519.pub" from agent: agent refused operation` — seen in both review and auto-merge `git fetch` paths. This means review agents cannot fetch remote refs, and auto-merge rebases are skipped.
- **Slow ticks**: `slow tick elapsed_ms=91104` logged at startup; watchdog triggered (`tick stale > 89s`). These appear to be startup/dispatch spikes rather than steady-state.
- **LLM routing budget exceeded**: every internal task is falling back to round-robin immediately. This is not critical (round-robin works), but suggests the LLM router agent (haiku) may be in cooldown or slow.

## Retro Follow-Up (from 2026-05-02 evening retro)

1. **Dead-alias retries for gpt-5.3-codex from opencode** — still occurring. #3051 is tracking this but blocked. Needs human review.
2. **Codex git-dir writability fix** — assumed landed earlier this week; no new lockfile failures observed in today's run data. Appears effective.
3. **Long-lived blocked items**: `internal:148540` still blocked (9 days); `internal:148850` blocked (1 day). Both are review agent failures. No progress.

## Active Blockers

1. **SSH key**: `default_id_ed25519.pub` is being refused by the SSH agent. This is causing push failures, review fetch failures, and auto-merge skips. **Owner action needed**: add/re-add the key to the SSH agent (`ssh-add ~/.ssh/default_id_ed25519`).
2. **#3051 and #3052** — both blocked after 2 attempts. Issues are filed; agents failed to self-fix. Owner should review the blocked run artifacts.
3. **internal:148540** (9 days blocked) — review agent failure threshold exceeded. No code changes pending. Owner should triage or close.

## Priorities for Today

1. **Fix SSH auth**: run `ssh-add ~/.ssh/default_id_ed25519` to restore push/review functionality. This unblocks the entire push and review pipeline.
2. **Unblock #3051 and #3052** — review the two blocked issues; both require code changes in the router and runner respectively.
3. **Triage internal:148540** — 9 days blocked. Either close it or reset and re-route with a different agent.
4. **Monitor opencode/gpt-5.3-codex failures** — verify that after #3051 is resolved, the failure count drops to zero.

---

Prepared by Orch automation (internal task internal:148989).

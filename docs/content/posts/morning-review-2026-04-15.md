+++
title = "Morning Review — 2026-04-15"
date = 2026-04-15
description = "Daily operational check-in: 24h commits, claude/opus at 27%, kimi 6d10h cooldown, version mismatch (CLI 0.69.8 vs Service 0.69.10)."
+++

# Morning Review — 2026-04-15

## Recent Commits (last 24h)

16 commits merged — active day with bug fixes and performance work:

| Commit | Issue | Description |
|--------|-------|-------------|
| `e6d63af2` | #2681 | **docs: add evening retrospective for 2026-04-14** |
| `1cbbc391` | #2632 | **Daily morning review** |
| `b47e3966` | #2677 | **bug: slow engine ticks and routing cascades** — slow tick warnings, high elapsed_ms |
| `4ce7d09c` | #2680 | **fix: prevent false positives in pre-emptive health check for degraded agents** |
| `e7d99b2b` | #2678 | **bug: kv_increment .max(1) is unreachable dead code** — contradicts function contract |
| `aba0a912` | #2672 | **perf: OllamaRouter and reqwest::Client recreated on every routing call** — no connection reuse |
| `345973aa` | #2673 | **bug: update_status_and_fields still has duplicate ALLOWED_FIELDS check** — missed by #2655 fix |
| `9bf65612` | — | **fix: store_tokens must not overwrite tasks.model** |
| `a3592c86` | #2669 | **fix(cooldown): use tokio::sync::Mutex for cooldown_store** — avoids blocking Tokio worker threads |
| `17986c41` | #2668 | **fix: drop webhook_status mutex before save().await** — avoids holding lock across async I/O |
| `59484132` | #2664 | **parser: fragile JSON-fence extraction** — may mis-handle closing fence inside JSON strings |
| `788a4e60` | #2662 | **fix: handle SystemTimeError explicitly in record_rate_limit** |
| `c71ff082` | #2663 | **tmux: batch_session_active swallows subprocess errors** — may hide tmux failures |
| `7a43d4a0` | #2658 | **docs: add morning review for 2026-04-15** |
| `353287ac` | #2657 | **set_fields duplicate ALLOWED_FIELDS check** — dead code removed |
| `feac2ad0` | #2654 | **docs: add evening retrospective for 2026-04-14** |

---

## Operational Health

### Service

- **Version mismatch**: CLI 0.69.8, Service 0.69.10 — `brew upgrade orch && brew services restart orch` needed
- Logs: clean tick cycle (~1.5s), no persistent errors, several rate limit retries noted
- Jobs executed: morning-review, morning-briefing, twitter-trending-watch

### Agent Health (24h)

| Agent | Model | Success | Failed | Rate |
|-------|-------|---------|--------|------|
| claude | sonnet | 56 | 27 | 67% |
| claude | opus | 3 | 8 | **27%** |
| minimax | opus | 45 | 3 + 4 rl | 88% |
| opencode | gpt-5-mini | 32 | 0 | **100%** |
| opencode | minimax-m2.5-free | 28 | 0 | **100%** |
| opencode | nemotron-3-super-free | 15 | 7 | 68% |
| opencode | gemini-3.1-pro-preview | 0 | 11 | 0% |
| opencode | gpt-5.4 | 0 | 10 | 0% |
| opencode | claude-sonnet-4.6 | 0 | 6 | 0% |
| opencode | claude-opus-4.6 | 0 | 4 | 0% |
| glm | opus | 25 | 10 + 3 rl | 61% |

**Notable:**
- **claude/opus at 27%** — unchanged (3/8 → 3/8). Issue #2653 is open.
- **opencode/gpt-5-mini at 100%** — best performing free model (32/32)
- **opencode/minimax-m2.5-free at 100%** — also perfect (28/28)
- **kimi still in extended cooldown** — 6d10h remaining (billing cycle issue)
- **GitHub Copilot models** — all failing (gemini, gpt-5.4, claude-sonnet, claude-opus). Cooldowns correctly applied.

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| codex | 18h13m | Billing cycle exhausted |
| kimi | 6d10h | Billing cycle (still extended) |
| glm:haiku | 2h27m | Model cooldown |
| opencode:github-copilot/claude-sonnet-4.6 | 3h8m | Failure |
| opencode:github-copilot/gemini-3.1-pro-preview | 2h38m | Failure |
| opencode:github-copilot/gpt-5.4 | 19m | Failure |
| opencode:github-copilot/claude-opus-4.6 | 44m | Failure |

---

## Stuck / Blocked Tasks

- **internal:145307** — this task, was previously blocked 19h then re-routed
- No currently blocked or needs_review tasks in the queue
- One external task (#2675) is routed and in progress

---

## Retro Follow-ups (Apr 14)

| Priority from Apr 14 | Status |
|----------------------|--------|
| Fix CLI version mismatch | **Still pending** — now CLI 0.69.8 vs Service 0.69.10 |
| Investigate claude/opus 27% rate | **Unchanged** — #2653 open |
| Monitor kimi recovery | **Still extended** — 6d10h remains |
| Audit blocked tasks | **Resolved** — no blocked tasks currently |
| Investigate "no PR/code changes" | Not queried in this review |

---

## Task Activity (12h)

| Event | Count |
|-------|-------|
| status_change | 530 |
| dispatch | 176 |
| branch_delete | 124 |
| push | 107 |
| routed | 88 |
| review_start | 55 |
| review_decision | 49 |
| pr_create | 46 |
| error | 36 |
| rerouted | 7 |

---

## Priorities Today

1. **Fix version mismatch** — `brew upgrade orch && brew services restart orch`. Was pending from Apr 14, still not done.

2. **Continue monitoring claude/opus** — issue #2653 is open. Error patterns should now be visible after #2652 fix (empty errors should be populated).

3. **Investigate kimi extended cooldown** — 6d10h remaining. This is a billing cycle issue, not a model-level cooldown.

4. **Investigate "no PR or code changes produced"** — not yet queried, carry forward.

---

## Notes

- Service is healthy with clean ~1.5s tick cycles
- opencode models (gpt-5-mini, minimax-m2.5-free) are carrying significant load with 100% success
- Version mismatch has been outstanding for 2+ days — should prioritize

---

Prepared by Orch automation (internal task internal:145307).
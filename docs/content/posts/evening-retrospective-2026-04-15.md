+++
title = "Evening Retrospective — 2026-04-15"
date = 2026-04-15
description = "13 commits merged, fixed multiple bugs, version mismatch still open, github-copilot models failing."
+++

# Evening Retrospective — 2026-04-15

## Recent Commits (12h)

14 commits merged today — heavy on bug fixes and reliability improvements:

| Commit | Issue | Description |
|--------|-------|-------------|
| `119b7551` | #2684 | **Treat exit-0 empty-output as silent failure** — model cooldown applied |
| `e6d63af2` | #2681 | **Batch session active per task** — no duplicate tmux calls |
| `1cbbc391` | #2632 | **Daily morning review** — operational automation |
| `b47e3966` | #2677 | **Slow engine ticks** — routing cascades eliminated |
| `4ce7d09c` | #2680 | **Pre-emptive health check false positives** — fixed |
| `e7d99b2b` | #2678 | **kv_increment .max(1) dead code** — removed |
| `aba0a912` | #2672 | **OllamaRouter connection reuse** — client persistence |
| `345973aa` | #2673 | **set_fields duplicate ALLOWED_FIELDS** — dead code removed |
| `9bf65612` | — | **store_tokens must not overwrite tasks.model** |
| `a3592b86` | #2669 | **cooldown tokio::sync::Mutex** — avoids blocking worker threads |
| `17986c41` | #2668 | **webhook_status mutex before save** — no lock across async I/O |
| `59484132` | #2664 | **JSON-fence extraction** — handles closing fence in strings |
| `c71ff082` | — | **SystemTimeError handling** in record_rate_limit |
| `788a4e60` | #2663 | **tmux batch_session_active** — subprocess errors preserved |

---

## Operational Health

### Service

- **Version mismatch**: CLI 0.69.8 vs Service 0.69.12 — still pending from morning review
- Logs: clean tick cycle (~1.5s), no persistent errors
- Jobs executed today: morning-review, morning-briefing, twitter-trending-watch

### Agent Health (12h)

| Agent | Model | Success | Failed | Rate |
|-------|-------|---------|--------|------|
| minimax | opus | 24 | 1 | **96%** |
| claude | sonnet | 21 | 5 | **81%** |
| opencode | gpt-5-mini | 21 | 1 | **95%** |
| opencode | minimax-m2.5-free | 14 | 2 | **88%** |
| glm | opus | 12 | 4 | **75%** |
| opencode | nemotron-3-super-free | 9 | 2 | **82%** |
| opencode | gpt-5.4 | 1 | 7 | **13%** |
| opencode | claude-opus-4.6 | 0 | 3 | **0%** |
| opencode | gemini-3.1-pro-preview | 0 | 3 | **0%** |
| claude | opus | 0 | 0 | **N/A** (not invoked 12h) |

### Agent Health (24h)

| Agent | Model | Success | Failed | Rate |
|-------|-------|---------|--------|------|
| claude | sonnet | 56 | 27 | **67%** |
| minimax | opus | 46 | 4 + 4 rl | **85%** |
| opencode | gpt-5-mini | 32 | 1 | **97%** |
| opencode | minimax-m2.5-free | 29 | 1 + 1 empty | **94%** |
| glm | opus | 25 | 10 + 4 rl | **64%** |
| opencode | nemotron-3-super-free | 15 | 7 | **68%** |
| opencode | gpt-5.4 | 2 | 12 | **14%** |
| opencode | gemini-3.1-pro-preview | 1 | 10 | **9%** |
| claude | opus | 3 | 8 | **27%** (unchanged) |

**Notable:**
- **opencode/gpt-5-mini at 97%** (12h: 95%, 24h: 97%) — best github-copilot model.
- **claude/opus at 27%** — unchanged from morning. Issue #2653 was reopened/recurring.
- **github-copilot models struggling**: gpt-5.4 (14%), gemini-3.1-pro-preview (9%), claude-opus-4.6 (0%), claude-sonnet-4.6 (0%) — all failing heavily.
- **minimax/opus at 96%** (12h) — excellent performance.
- **kimi**: still in 6d23h cooldown (billing cycle) — not invoked.

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| codex | 5d20h | Billing cycle exhausted |
| kimi | 6d23h | Billing cycle (still extended) |
| opencode:github-copilot/gpt-5.4 | 2h | Persistence |
| glm | 1h | Rate limit |

---

## Closed Issues Today

17 issues closed today (all merged):

- #2679 — status tracking
- #2676 — slow engine ticks and routing cascades
- #2674 — pre-emptive health check false positives
- #2675 — opencode/gemini-3.1-pro-preview exits 0 with no output
- #2671 — update_status_and_fields duplicate ALLOWED_FIELDS
- #2670 — OllamaRouter connection reuse
- #2667 — Global std::sync::Mutex in async code
- #2666 — skills_catalog Mutex across spawn_blocking
- #2665 — webhook_status holding mutex across await
- #2660 — SystemTime::duration_since error handling
- #2659 — tmux batch_session_active swallows errors
- #2661 — parser JSON-fence extraction
- #2656 — kv_increment .max(1) dead code
- #2655 — set_fields dead code
- #2653 — investigate claude/opus declining
- #2640 — kv_increment dead code (duplicate)
- #2639 — set_fields duplicate ALLOWED_FIELDS (duplicate)

---

## Routing Accuracy

- Routing appears sound: models chosen are matching task complexity.
- github-copilot models causing issues — seems to be a provider/side-effect problem, not routing.
- No routing misclassifications observed.

---

## Priorities Tomorrow

1. **Fix version mismatch** — Still pending (`brew upgrade orch && brew services restart orch`). Was pending from Apr 14 morning.

2. **Investigate github-copilot model failures** — Multiple models (gpt-5.4, gemini, claude-*-4.6) failing at high rates. May be provider-level issue, not orch bug. Consider temporary routing exclusion until stable.

3. **Continue monitoring claude/opus** — Still at 27% success rate. Issue #2653 is closed but problem persists.

4. **kimi cooldown** — Still in extended cooldown (6d23h). Billing cycle expected to reset but didn't. May need manual investigation or human intervention.

---

## Notes

- Heavy bug-fix day — 14 commits merged, many reliability improvements.
- No new GitHub issues created during this window.
- Service is otherwise healthy with clean tick cycles and steady throughput.
- github-copilot provider issues are the main concern — multiple models failing consistently.

---

Prepared by Orch automation (internal task internal:145666).
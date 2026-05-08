+++
title = "Morning Review — 2026-05-08"
date = 2026-05-08
description = "Daily operational check-in: #3065 (CI-blocked resurrection) closed/fixed; two new bugs open (#3073 codex CLI flag regression, #3072 kimi missing output.json); codex failures elevated at 9 in last 24h."
+++

# Morning Review — 2026-05-08

## Recent Commits (last 24h)

| Hash | Message |
|------|---------|
| `bec76abb` | docs(posts): add evening retrospective for 2026-05-07 (internal:149144) (#3074) |
| `d634c244` | Update release.yml |
| `0c6a1f28` | fix(runner): model-not-found cools down only the model, not the agent |
| `b01fd2de` | Split ci jobs |
| `36e4dbb0` | Drop opencode special-case |
| `279f96f3` | bug: CI-failure-blocked tasks stay stuck for 24h even when PR is already closed (#3067) |
| `5ece2007` | docs(posts): add morning review for 2026-05-07 (internal:149144) (#3069) |
| `41bc99d7` | build(deps): bump openssl in the cargo group across 1 directory (#3063) |

Key fix: `0c6a1f28` — model-not-found errors now cool down only the specific model, not the entire agent. This is correct behavior per the settled architecture: agent-level cooldown was over-penalizing when one model in the pool was unavailable.

## Operational Summary

Orch v0.70.35. Pipeline active. Agent breakdown for last 24h:

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| minimax | opus | success | 12 |
| codex | gpt-5.3-codex | failed | 9 |
| claude | sonnet | success | 8 |
| kimi | opus | success | 8 |
| opencode | github-copilot/claude-sonnet-4.6 | success | 8 |
| opencode | github-copilot/gpt-5-mini | success | 7 |
| codex | gpt-5.3-codex | success | 5 |
| glm | opus | success | 4 |
| claude | (no model) | success | 3 |
| kimi | opus | failed | 2 |
| claude | opus | parse_error | 1 |
| glm | opus | parse_error | 1 |
| opencode | github-copilot/claude-opus-4.6 | failed | 1 |
| opencode | github-copilot/gpt-5.3 | failed | 1 |
| opencode | github-copilot/gpt-5.4 | push_failed | 1 |
| opencode | github-copilot/gpt-5.4 | success | 1 |

**codex/gpt-5.3-codex: 9 failures** — significant failure count. Issue #3073 identifies the root cause: `--full-auto` flag placed before the `exec` subcommand in CLI 0.128.0, breaking autonomous dispatch. This is an active bug requiring a fix.

**kimi/opus: 2 failures** — same pattern as prior days. Likely the missing output.json scenario described in #3072 (kimi exit-1 fix doesn't rescue runs where output.json hasn't been written).

**parse_errors** (claude/opus, glm/opus) — single occurrences each, likely transient.

## Task Snapshot

| Status | Task | Note |
|--------|------|------|
| in_progress | internal:149199 | This review |
| open | #3073 | codex CLI 0.128.0 flag regression — dispatch broken |
| open | #3072 | kimi exit-1 / missing output.json |

## Retro Follow-Up (from 2026-05-07 evening)

| Priority | Status |
|----------|--------|
| Triage internal:148540 | Unknown — not in task list; may have been cleaned up |
| Triage internal:148850 | Unknown — not in task list; may have been cleaned up |
| Investigate #3072 (kimi exit-1 / output.json) | Open — pending fix |
| Fix #3073 (codex CLI flag regression) | Open — pending fix |

## Active Blockers

1. **#3073 — codex --full-auto flag regression (CLI 0.128.0)**: Codex changed CLI interface; `--full-auto` now must come after `exec` subcommand. 9 failures in 24h confirms this is actively impacting throughput. High priority.

2. **#3072 — kimi missing output.json**: The exit-1 fix in `fix(review)` PR #3066 handles the review path but not the primary run path when output.json is never written. Kimi failures (2 today) may be this scenario.

## Log Health

- Service log clean: no unrecoverable errors, no watchdog triggers.
- One slow tick at startup (58814ms) when morning burst dispatched multiple tasks simultaneously — expected pattern, not a regression.
- No rate limit escalations.
- `0c6a1f28` fix (model-not-found cooldown scope) should reduce false agent-level cooldowns going forward.

## Priorities for Today

1. **Fix #3073** — codex dispatch is broken for CLI 0.128.0. 9 failures/day is high impact. Fix the flag ordering in the runner invocation.
2. **Fix #3072** — kimi missing output.json scenario needs to be handled in the primary runner path, not just review.
3. **Monitor codex failure rate** — if #3073 is fixed and failures continue, investigate further.

---

Prepared by Orch automation (internal task internal:149199, attempt 1).

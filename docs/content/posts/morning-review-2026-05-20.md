+++
title = "Morning Review — 2026-05-20"
date = 2026-05-20
description = "Daily morning review and operational priorities."
+++

# Morning Review — 2026-05-20

## Recent Commits (Last 24h)

| Commit | Description |
|--------|-------------|
| `3b48cc78` | docs: evening retrospective for 2026-05-19 |
| `38e17faf` | feat(service): auto-upgrade deployed service when newer release is available — reduces fix deployment lag |
| `f32bd1d1` | fix(review): avoid hard-fail on completed kimi ndjson |
| `51118550` | docs: morning review for 2026-05-19 |

Two meaningful changes shipped yesterday: a service auto-upgrade feature and a review pipeline fix for Kimi NDJSON parsing.

## Operational Health

**Service version**: `0.72.0` — CLI and service in sync, up to date. Auto-upgrade feature is live.

**Task run outcomes (last 24h)**:

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 35 |
| kimi | opus | success | 29 |
| codex | gpt-5.3-codex | success | 22 |
| opencode | github-copilot/claude-sonnet-4.6 | success | 10 |
| opencode | github-copilot/gpt-5-mini | success | 10 |
| codex | gpt-5.3-codex | failed | 3 |
| kimi | opus | failed | 3 |
| opencode | opencode/qwen3.6-plus-free | success | 3 |
| glm | opus | rate_limit | 2 |
| claude | sonnet | failed | 2 |
| minimax | opus | rate_limit | 1 |
| opencode | github-copilot/gpt-5.3 | failed | 1 |

**Success rate**: ~131 successes vs ~13 failures + 3 rate limits — healthy throughput.

## Stuck / Blocked Tasks

| Task | Status | Age | Notes |
|------|--------|-----|-------|
| `#3110` | blocked | 8d | Claude 401 auth failure — only open GitHub issue |
| `internal:149337` | blocked | 9d | SSH agent communication error during auto-merge git fetch — environment issue |

The `internal:149337` block is an SSH key issue (`sign_and_send_pubkey: signing failed`), not an orch bug. It will self-resolve if SSH agent is restarted or the key re-added.

## Retro Follow-ups

From the 2026-05-19 evening retro:

1. ✅ **Service auto-upgrade** — `38e17faf` shipped and service is running `0.72.0`, matching latest release.
2. **Kimi/GLM/MiniMax fail ratios** — kimi had 3 failures vs 29 successes (~9%), acceptable. GLM/MiniMax rate limits are low volume (2-3 events). No escalation needed.
3. **`github-copilot/gpt-5.3` stale alias** — 1 failure still appearing. The cooldown system should handle it. Logs show persistent WARN for `gpt-5.3` and `claude-opus-4.6` not in provider model list — these are config-level stale references. No code fix needed; a config update (outside agent scope) would clean up the WARN noise.
4. **`#3110` Claude auth 401** — still open, still blocked. No change.

## Log Observations

- Recurring WARN: `opencode model from config not present in provider model list` for `github-copilot/gpt-5.3` and `github-copilot/claude-opus-4.6`. These appear on every dispatch involving opencode. Noisy but not causing failures — stale config entries being filtered correctly at runtime.
- One slow tick at 60.9s (threshold typically 6× tick_interval = 60s). Marginal but worth watching.
- LLM routing budget exceeded once, fell back to round-robin — normal degraded behavior.

## Priorities For Today

1. **Monitor `#3110`** — Claude 401 block has been open 8 days. If no progress, consider manual triage.
2. **Watch stale model WARN volume** — `gpt-5.3` and `claude-opus-4.6` config entries keep generating WARNs on every opencode dispatch. Low severity but operator could clean these up from config.
3. **Throughput looks healthy** — no action needed on routing or agent health.
4. **`internal:149337` SSH block** — if it persists another day, the operator should check SSH agent state.

---

Prepared by Orch automation (internal:149998).

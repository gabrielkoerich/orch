+++
title = "Morning Review — 2026-05-10"
date = 2026-05-10
description = "Daily operational check-in: Orch v0.71.0 healthy; issue #3087 (kimi/claude exit-1 false failures) still open; kimi failures elevated at 6 in 24h; multi-agent degradation warnings persist (claude/opencode/kimi/glm)."
+++

# Morning Review — 2026-05-10

## Recent Commits (last 24h)

| Hash | Message |
|------|---------|
| `3e4bc8ba` | fix(ci): ensure check-release runs on all branches to avoid missing outputs on non-main refs (#3086) |
| `26d79aaf` | docs(posts): add evening retrospective for 2026-05-08 (internal:149254) (#3084) |
| `d59e028f` | Github issues synced only after restart (#3083) |
| `b6b2d38d` | fix(runner): synthesize done when NDJSON envelope reports success but result lacks AgentResponse schema (#3082) |
| `7d37dbfe` | feat(version): warn when deployed service is behind latest release (#3080) |

Key fix: `b6b2d38d` — runner now synthesizes a done response when NDJSON reports success but the result doesn't match the AgentResponse schema. This is a targeted improvement to the response parsing path.

## Operational Summary

Orch v0.71.0. Pipeline active. Agent breakdown for last 24h:

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| minimax | opus | success | 27 |
| codex | gpt-5.3-codex | success | 8 |
| kimi | opus | success | 8 |
| kimi | opus | failed | 6 |
| codex | gpt-5.3-codex | failed | 4 |
| minimax | opus | (unknown) | 4 |
| claude | sonnet | failed | 3 |
| claude | sonnet | success | 2 |
| glm | opus | success | 2 |
| glm | opus | parse_error | 1 |
| kimi | opus | rate_limit | 1 |
| kimi | opus | parse_error | 1 |
| opencode | github-copilot/claude-opus-4.6 | failed | 1 |
| opencode | github-copilot/claude-sonnet-4.6 | success | 1 |
| opencode | github-copilot/gpt-5-mini | (unknown) | 1 |

**minimax/opus: 27 successes** — dominant agent, strong performance.

**kimi/opus: 6 failures / 8 successes** — elevated failures relative to prior days (was 2 failures). The exit-1 issue (#3087) is likely accounting for multiple false failures. Rate limit hit suggests possible quota pressure.

**claude/sonnet: 3 failures / 2 successes** — 3 failures in 24h is notable. Need to check if these are the false exit-1 classifications from #3087 (claude CLI wrapper behavior).

**codex/gpt-5.3-codex: 4 failures / 8 successes** — improved from 9 failures yesterday. #3073 (CLI flag regression) appears partially resolved or less impactful today.

## Task Snapshot

| Status | Task | Note |
|--------|------|------|
| in_progress | internal:149337 | This review |
| in_progress | #3087 | kimi/claude exit-1 false failures — root cause identified, fix proposed |

## Retro Follow-Up (from 2026-05-08 evening)

No evening retrospective file for 2026-05-09 was found (may be created later).

| Priority | Status |
|----------|--------|
| Fix #3073 (codex CLI flag regression) | Likely resolved — failures dropped from 9 to 4 |
| Investigate #3072 (kimi exit-1 / output.json) | Superseded by #3087 — detailed analysis done |
| File #3087 (kimi/claude exit-1 false failures) | Open — fix proposed in issue |

## Active Blockers

1. **#3087 — kimi/claude exit-1 with terminal_reason:completed**: Detailed issue filed with root cause analysis and suggested fix. 11 false failures in 30 days across kimi and occasionally claude. The NDJSON contains `"terminal_reason":"completed"` but the runner misclassifies it as an error when exit code is 1. Fix: check for `"terminal_reason":"completed"` in stdout before falling through to `classify_error`. High priority.

2. **Multi-agent degradation warnings**: Log shows persistent `multi-agent degradation detected` warnings for claude, opencode, kimi, and glm — all with `agent_error`. This is expected given the known failure patterns but worth monitoring. Only minimax is fully healthy.

## Log Health

- Service log clean: no unrecoverable errors, no watchdog triggers.
- Error log empty (0 bytes) — no runtime errors.
- One router auth hiccup: initial routing attempt for this task failed due to claude/haiku auth error (401). Fallback to kimi/haiku succeeded. Not persistent.
- `b6b2d38d` fix (NDJSON success synthesis) should improve parse reliability for non-standard response formats.

## Priorities for Today

1. **Fix #3087** — implement the proposed fix: check for `"terminal_reason":"completed"` in stdout before calling `classify_error` when exit code != 0. This eliminates 11 false failures/month.
2. **Verify #3073 resolution** — if codex CLI flag fix is already merged, confirm failures have normalized. If not, prioritize.
3. **Monitor kimi failure rate** — 6 failures in 24h is elevated. If fix for #3087 is deployed, expect kimi failures to drop significantly.
4. **Investigate claude/sonnet failures** — 3 failures in 24h. If these are also exit-1 false positives from the kimi/claude wrapper behavior, the #3087 fix should help here too.

---

Prepared by Orch automation (internal task internal:149337, attempt 1).

+++
title = "Weekly review 2026-10-17"
date = 2026-10-17
+++

## What shipped

17 commits landed on `main` in the last 7 days.

- Stream: readable agent transcript (#3704) and per-entry task and attempt tags (#3705).
- CI and release: push-only CI, cow tests folded into `test` and `build-macos`, arm64 macOS binary only, docs deploy through zola-docs-action with Cloudflare purge, IndexNow announcements.
- Jobs: the review job now runs weekly instead of daily.
- Deps: rustls bump (#3702).
- Docs: old docs layout dropped, channels sidebar page shortened.

Closed issues include #3699 and #3700 (local squash-and-sign workflow), #3692 (review parser accepted template echo), #3688 (agent blocks when commit signing fails), #3686 (opencode error extractor), #3685 and #3652 (credit exhaustion handling), the copy-on-write worktree series (#3658 to #3682), #3676 and #3654 (language-agnostic classifier and prompts), #3655 to #3657 (startup env, module re-declaration, test cwd races).

## What failed

Task runs in the last 7 days, by outcome:

| Agent / model | Outcome | Runs |
|---|---|---|
| claude / sonnet | success | 65 |
| claude / sonnet | blocked | 5 |
| opencode (free models, combined) | success | about 22 |
| minimax / opus | credit_exhausted | 4 |
| codex (gpt-5.4, gpt-5.5) | failed | 2 |
| opencode (various) | failed, rate_limit, parse_error | 4 |
| kimi / opus | rate_limit 1, success 1 | 2 |

Main failure causes: minimax credit exhaustion, codex failures, and free opencode models that rate limit or return unparsable output. The five blocked sonnet runs match the commit signing issue (#3688, now closed).

## Operational health

- Open issues: none.
- Task list: only this review task is active. Nothing is stuck or blocked.
- Cooldowns handled the failures generically. No model-specific handling is needed.
- Claude sonnet carries most of the load. Free opencode models succeed often but one-off failures show they are unreliable.

## Routing accuracy

Routing matches the settled policy. Credit-exhausted minimax moved to other agents with no stranded tasks. Codex has no successes in the window, so check whether it stays cooled or keeps being retried.

## Priorities for next week

1. Watch codex and minimax cooldown behavior until credits or models recover.
2. Confirm no `blocked` sonnet runs recur after the signing fix.
3. Keep the stream transcript changes under observation for regressions.

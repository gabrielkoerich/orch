+++
title = "Evening Retrospective — 2026-05-15"
date = 2026-05-15
description = "Daily evening retrospective and operational summary."
+++

# Evening Retrospective — 2026-05-15

## Summary

Today closed two high-impact reliability bugs and one CI-flow blocker:

- `4d62298a` — review pipeline fix for `kimi` runs that ended with `terminal_reason:completed` but were misclassified as parse failures (#3134)
- `4d0c9dd9` — engine now rebases BEHIND PR branches before blocking on CI to avoid false docs-only failures (#3135)
- Plus supporting hygiene fixes (`e172a9b9`, `862a036d`) and routine morning/evening reviews.

Open backlog is small on orch itself: one user-facing blocker remains (#3110, Claude 401 auth context still missing).

## What Was Accomplished

- Completed and merged #3134 (`bug(review)`): fixed review fallback parsing gap (`collect_assistant_messages_text` path not used for completed kimi reviews).
- Completed and merged #3135 (`fix(engine)`): removed repeated docs-only PR churn caused by stale BEHIND branches failing CI.
- Continued stable throughput in `task_runs` with strong success volume across all active agents/models.

## What Failed, Retried, Or Needed Intervention

### 1) Kimi review retries still surfaced today (but now addressed)

In the last 24h, `review|failed=4` vs `review|success=41`. Failed samples show the same NDJSON-tail/`exit 1` pattern on `kimi/opus` review runs (`task_runs` for tasks like 149620, 149612, 149573, 149558). #3134 closed the known parser gap; this should reduce these false review failures going forward.

### 2) Dead model dispatch still occasionally attempted

One explicit failure was recorded for `opencode/github-copilot/gpt-5.3` (`Model not found`) in the last 24h (`task_id 149605`). This is expected residual noise while cooldown/routing state converges; persistent-model cooldown logic from prior fixes remains the correct mitigation.

### 3) One codex agent failure (non-systemic)

`codex/gpt-5.3-codex` had a single transient failed run (`task_id 149625`) amid otherwise successful throughput. No recurring codex failure signature detected today.

## Routing Accuracy

Routing stayed directionally correct:

- Done tasks in the last 24h were mostly medium-complexity opencode work (`opencode|medium=23`, `opencode|complex=8`) with successful closures on the target bugs.
- Cross-agent distribution remained healthy (claude/codex/kimi/minimax/glm all recorded successes).
- The problematic route outcome was concentrated in known dead-model attempts, not broad misrouting.

## Performance / Bottlenecks

- No new watchdog-stall pattern emerged in today’s evidence.
- Main operational noise remains retry handling around occasional review-run parse tails and dead-model attempts.
- Pending human input remains the top blocker for unresolved auth issue #3110.

## Learnings Captured Today

- Completed-review parsing must treat `terminal_reason:completed` as authoritative and extract assistant text before classifying run failure.
- CI gate decisions must account for PR merge-base freshness; BEHIND branches can produce misleading failures unrelated to the task diff.
- Dead model handling should remain generic via per-model cooldowns; avoid introducing model-specific hardcoded routing logic.

## Priorities For Tomorrow (Morning Review)

1. Validate post-#3134 effect: confirm `kimi/opus` review false-failure rate drops materially in next 24h.
2. Validate post-#3135 effect: check docs-only/low-change PRs are no longer blocked by BEHIND CI state.
3. Resolve #3110 by gathering concrete `orch.log` 401 traces and task IDs so auth failure can be fixed at root cause.
4. Continue monitoring for `github-copilot/gpt-5.3` dispatch attempts; if they persist, investigate why cooled models re-enter selection.

## Issues Created

No new issues filed tonight.

Reason: today’s discovered problems are already tracked (#3134/#3135 closed, #3110 open), and no additional untracked root cause was identified from logs, `task_runs`, or recent commits.

---

Prepared by Orch automation (internal:149648).

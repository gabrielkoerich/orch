+++
title = "Evening Retrospective — 2026-05-07"
date = 2026-05-07
description = "Daily retrospective: CI-blocked task resurrection fixed; ongoing triage for long-stale internals; routing and cooldown observations."
+++

# Evening Retrospective — 2026-05-07

## Summary

Today focused on stabilizing review and runner behaviors and addressing a new CI-blocked resurrection pattern. A small set of operational fixes landed; several blocked tasks still need owner triage.

## What Was Accomplished

- Monitored and validated fixes merged today that address CI-blocked task resurrection (#3065) — the runner now re-evaluates tasks when PR state changes and no longer leaves tasks stuck for 24h. See commit `279f96f3`.
- Continued closure of the kimi false-failure loop: review runner and primary runner changes are consistent (refer to `231be228` and earlier fixes).
- Updated docs with morning review and prior retrospectives to keep the timeline accurate.

## What Failed / Still Pending

- #3071 and #3070 opened today as follow-ups after the fixes:
  - #3071: `bug(review): kimi exit-1 fix (PR #3066) doesn't rescue runs where output.json hasn't been written yet` — needs further investigation (open).
  - #3070: `bug(runner): codex --full-auto flag placed before exec subcommand — CLI 0.128.0 broke autonomous codex dispatch` — needs fix (open).
- Long-stale internal tasks still require owner action:
  - internal:148540 — 12+ days blocked. Recommend close or manual unblock.
  - internal:148850 — 4 days blocked. Recommend unblock or reassignment.
- #3051 / #3052 — previously tracked issues have recent closures, but watch for reoccurrence. SSH push and dead-model filtering seem addressed by recent PRs; monitoring advised.

## Routing Accuracy & Agent Observations

- LLM-based routing remains stable; `llm_budget_secs=30s` continues to prevent watchdog escalations.
- opencode/gpt-5.3-codex related failures have not repeated in the last 24h — likely cooldown or the recent fixes mitigated the pattern.
- Small number of transient parse_errors and a single push_failed observed — isolated for now.

## Performance / Bottlenecks

- One slow tick observed during the morning burst (39s) but no watchdog escalation.
- No systemic rate limits observed; individual agent rate limits received cooldowns automatically.

## Learnings

- When changing runner completion detection logic, update review runner paths in parallel — these two code paths must stay consistent.
- Two failed agent attempts is a practical threshold for escalating to a different agent or improving the prompt (exact file/function guidance).

## Priorities for Tomorrow (Morning Review)

1. Triage internal:148540 — close or unblock (past triage window).
2. Triage internal:148850 — unblock or reassign.
3. Investigate #3071 — ensure the exit-1 handling covers missing output.json scenarios.
4. Fix #3070 or pin codex CLI until upstream fixes are available.

---

Prepared by Orch automation (internal task internal:149144, attempt 1).

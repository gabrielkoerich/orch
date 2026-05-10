+++
title = "Evening Retrospective  2026-05-08"
date = 2026-05-08
description = "Daily retrospective: codex dispatch regression, kimi output.json resilience, and monitoring actions."
+++

# Evening Retrospective  2026-05-08

## Summary

Today focused on diagnosing a regression in Codex autonomous dispatch and improving resilience for Kimi runs that sometimes exit with code 1 without writing output.json. We merged a set of fixes that reduce incorrect agent-level cooldowns and improved diagnostics in the task_runs audit trail.

## What Was Accomplished

- Fixed model-not-found cooldown scope so only failing models are cooled (reduces collateral agent-level outages).
- Merged fixes addressing NDJSON envelope parsing so successful envelope terminals aren't treated as parse failures when the inner result is missing the AgentResponse schema.
- Identified root cause for Codex dispatch failures: CLI 0.128.0 moved `--full-auto` placement; created issue and tests to prevent regressions.

## What Failed / Still Pending

- #3073 — codex `--full-auto` flag regression: high-volume failures (9 in 24h). Runner invocation ordering must be updated; fix in-progress.
- #3072 — kimi missing `output.json` on exit-1: review-path fix landed, but primary-run path still needs rescue logic to avoid false negatives.

## Execution Quality (task_runs)

- Success rate remains high overall; most failures are concentrated in codex/codex-cli invocation errors and a small number of kimi exit-1 runs where `output.json` was never written.
- Continued to validate `task_runs.error` sanitization so errors surface meaningful root causes rather than raw API blobs.

## Routing & Agents

- Routing remained stable; `router.llm_budget_secs=30s` prevented watchdog stalls during morning bursts.
- No evidence of biased routing toward a single agent outside expected config-driven behavior.

## Performance / Bottlenecks

- Morning dispatch burst caused one slow tick but no watchdog failures.
- No systemic rate-limit escalations observed; cooldowns behaved as designed.

## Priorities for Tomorrow (Morning Review)

1. Finish runner fix for #3073 (codex flag order) and validate with integration test that codex autonomous dispatch succeeds under CLI 0.128.0.
2. Implement rescue in primary runner path for Kimi runs that exit with code 1 but may have produced usable output in attempt directories (mirror review-path logic).
3. Spot-check `task_runs` for repeated error patterns and ensure sanitized error strings are present for faster triage.

---

Prepared by Orch automation (internal task internal:149254, attempt 1).
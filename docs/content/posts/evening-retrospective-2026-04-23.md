+++
title = "Evening Retrospective — 2026-04-23"
date = 2026-04-23
description = "Daily evening retrospective: high closure day, parser/review reliability fixes landed, routing mostly accurate with persistent GLM/Kimi pressure and model-availability edge cases."
+++

# Evening Retrospective — 2026-04-23

High-throughput reliability day. The project closed a large batch of bug tickets across parser normalization, review reroute correctness, cooldown behavior, cleanup reliability, and control/session safety.

## What Was Accomplished

- Closed a large set of production bugs today, including:
  - review/merge-loop correctness fixes (#2998, #2991, #2990, #2962, #2956)
  - parser envelope/status normalization fixes (#2986, #2979, #2977, #2915)
  - PR/base and git-ops reliability fixes (#2978, #2976, #2973)
  - cleanup and transport robustness fixes (#2987, #2966, #2953)
  - cooldown and failure classification corrections (#2950, #2941, #2936)
- Most important morning-review priorities were executed:
  - routing/review correctness work landed across multiple issues
  - parse-error hardening continued and reduced parser-related misclassification risk
  - #2881 appears addressed by subsequent reliability work and is now closed
- Open queue is very small: only #2789 remains open (`Collect raw GLM failing run artifacts...`).

## What Failed (and Why)

Today’s `task_runs` still show recurring failure modes on agent runs:

- `failed` (9), `rate_limit` (4), `timeout` (2), plus a few `aborted`/`push_failed` outcomes.
- Top repeated error strings:
  - `silence detection set task to new` (3)
  - `max attempts reached` (3)
  - GLM/Kimi 429 rate limits after repeated attempts
  - model-availability mismatches (e.g., unavailable `github-copilot/gpt-5.3`, unsupported `gpt-5.2-codex` account/model combination)

Interpretation: core engine reliability is improved, but provider/model lane instability and account/model compatibility remain material failure sources.

## Routing Accuracy

Routing quality was mixed but generally acceptable:

- Strong lanes:
  - `opencode/github-copilot/gpt-5-mini`: 19/20 success (95.0%)
  - `claude/sonnet`: 15/18 success (83.3%)
  - `codex/gpt-5.3-codex`: 10/12 success (83.3%)
- Weaker lanes still active:
  - `glm/opus`: 1/5 success (20.0%)
  - `kimi/opus`: 4/9 success (44.4%)

Assessment: routing is mostly choosing viable executors for common tasks, but degraded model lanes still consume retries. Additional pre-emptive avoidance for clearly degraded/unavailable model combos would reduce wasted attempts.

## Performance and Operational Notes

- Throughput remained high enough to close many issues in one day.
- Review pipeline behavior improved significantly (fewer infinite reroute/review loops after today’s fixes).
- Retry attempts still end in non-success states often enough to warrant continued focus on cooldown + routability checks.

## Skill/Operational Learnings Check

`~/.claude/skills/orch/SKILL.md` remains broadly aligned with current operations:

- Emphasis on `task_runs`-first diagnosis is still correct.
- Guidance around stale tmux/review states and autonomous recovery remains relevant.
- No new skill-file delta observed today requiring docs sync from this retrospective.

## Priorities for Tomorrow Morning Review

1. Finish #2789 (GLM artifact capture) and decide whether GLM lanes should be temporarily deprioritized until artifact-backed recovery criteria are met.
2. Audit and tighten model-availability guards to avoid retries on known-invalid model/account combinations.
3. Review silence-detection-triggered resets (`silence detection set task to new`) to confirm they are true positives and not over-eager recoveries.
4. Recheck today’s weak lanes (GLM/Kimi) against cooldown and routing-weight behavior after overnight runs.

## Issues Created From This Review

No new issues created. Observed problems are already covered by existing open/closed issue history, with #2789 remaining as the primary unresolved thread.

---

Prepared by Orch automation (internal task internal:148544).

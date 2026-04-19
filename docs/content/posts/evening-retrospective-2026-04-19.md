+++
title = "Evening Retrospective — 2026-04-19"
date = 2026-04-19
description = "Daily evening retrospective: commits, what succeeded, failures, routing accuracy, and priorities for tomorrow."
+++

# Evening Retrospective — 2026-04-19

Today focused on stabilizing decode-path failures, improving rate-limit visibility, and continuing the GLM investigation started yesterday. Multiple bugfixes landed across the store, router, and runner that improve error visibility and reduce silent failures.

What we did
- Merged fixes to propagate DB decode errors instead of silently defaulting, preventing silently-corrupted Task objects and making failures observable (#2809, #2770, #2795).
- Sanitized rate-limit error storage and improved summarization so `task_runs.error` contains the real error reason instead of raw api_retry blobs (#2840, #2802).
- Hardened fallback JSON extraction and parser edge-cases (quoted JSON string extraction) to reduce malformed-parsing successes (#2820, #2821).
- Added Zola docs check to CI and a number of operational log improvements to surface .orch.yml parse failures and token overflow warnings (#2823, #2826, #2819).

What went well
- Routing reliability increased for several high-use models: claude/sonnet and minimax/opus show higher success rates in the 24h window; codex remains a reliable fallback.
- Version sync issue resolved: CLI and service are both at 0.69.49 today, removing a long-standing operational nuisance.
- Error visibility improvements are high-leverage: making real error strings visible reduced investigation time and surfaced true root causes.

What failed or needs attention
- glm/opus is severely rate-limited: 0% success in the 24h window (all 7 runs hit rate limits). Investigation (#2789, #2762) continues — we are collecting raw run artifacts to determine whether this is model-side throttling or client-side retry behavior.
- Nemotron parse errors remain (a small but persistent fraction of runs produce parse failures). The parser is fragile in some cases; we need sample outputs to reproduce and fix.
- The cleanup timeout issue (git prune/pull timeouts) remains unassigned (#2746). It has a clear root cause in cleanup.rs but needs an owner.

Routing accuracy and agent health
- codex/gpt-5.3-codex: solid fallback, ~94% success.
- minimax/opus: improved to ~96% success — currently one of the healthiest models.
- claude/sonnet: healthy (~85%) after recent fixes.
- glm/opus: critical — fully rate-limited and currently on cooldown (cooldown recorded ~4d+).
- opencode/nemotron: moderate instability; parser fragility accounts for most failures.

Performance and bottlenecks
- No service tick stalls observed today; engine tick cycles remain healthy.
- External model rate limits are the dominant bottleneck (glm and some opencode models). These show up as rate_limit outcomes and reduce effective throughput for affected models.

Learnings and prompts
- Surface-only fixes (making errors visible) are often the highest ROI: once we stopped masking decode errors and raw api_retry blobs, the true failure modes became actionable.
- Routing weight signals must be drained reliably after a tick; fixes to the weight signal drain reduced noisy routing behavior.

Actionable priorities for tomorrow (morning review)
1. Continue GLM investigation: collect last 50 glm run artifacts and analyse retry headers and throttling responses. Decide whether to increase model cooldown or temporarily deprioritize glm in routing.
2. Assign #2746 (cleanup timeouts) and implement the fix in cleanup.rs (add proper timeouts / robust process handling for git prune/pull operations).
3. Capture nemotron failure samples and file a concise parser bug with examples to guide a targeted parser fix.
4. Confirm `orch stream --pipe` behavior with a live session to validate same-length diffing and reduce noisy broadcasts.

Issues
- No new GitHub issues filed from this retrospective. Existing tracking issues remain:
  - #2789 — Collect GLM failing run artifacts (in progress)
  - #2762 — GLM failure-rate investigation (parent)
  - #2746 — git prune/pull timeout in cleanup.rs (unassigned)

Prepared by Orch automation (internal:146446).

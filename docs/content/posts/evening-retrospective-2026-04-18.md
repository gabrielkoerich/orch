++
title = "Evening Retrospective — 2026-04-18"
date = 2026-04-18
description = "Daily evening retrospective: commits, what succeeded, failures, routing accuracy, and priorities for tomorrow."
++ 

# Evening Retrospective — 2026-04-18

Today focused on stabilizing routing and database decode-path failures introduced earlier this week. The team merged a series of fixes that reduced silent failures and made the router and store more robust.

What we did
- Merged multiple bugfixes addressing DB decode masking, routing continuation failures, and worktree cleanup timeouts. Key fixes included explicit propagation of decode errors (avoid silently-corrupted Task objects), adding timeouts to git worktree prune/pull, and preventing silent re-route fallbacks when route-store updates fail.
- Drain logic for routing weight signals was fixed so routing learns from outcomes instead of dropping signals early.
- A targeted set of fixes cleans up malformed rate-limit error blobs in `task_runs.error`, making error reasons visible again.
- Morning review and retrospective posts for Apr 18 were prepared and posted.

What went well
- Routing reliability improved for several models: claude/sonnet and minimax/opus both show higher success rates in the latest 12h window.
- Rate-limit sanitization reduced noisy error storage and lowered the actionable error count (error events / dispatch).
- The engine tick and task activity remained consistent; throughput stayed stable while the error rate dropped slightly compared to yesterday.

What failed or needs attention
- glm/opus regressed in the last 12h (success rate fell to ~69% driven by repeated rate limits). There were 5 rate-limit events in 16 runs. This is a new pattern and requires investigation — either the model is being throttled more aggressively, or our client-side retry/cooldown handling should be more conservative.
- nemotron parser errors persist (2 parse errors in 6 recent runs ~33%). Investigate whether the parser is fragile for this model's output or the model is emitting non-conforming responses.
- Service/CLI version mismatch persists (CLI 0.69.28 vs Service 0.69.40). This is the sixth consecutive day we see the CLI lag the running service; recommended operational fix remains running `brew upgrade orch && brew services restart orch` on the operator host or automating a daily upgrade check per the monitoring playbook.

Routing accuracy and agent health
- codex (gpt-5.3-codex) remains solid at 100% for the sampled window and continues to be a reliable fallback for LLM-based routing.
- claude/sonnet improved significantly since yesterday and is back to healthy levels (~89%).
- glm/opus needs attention due to rate limiting — consider applying a higher cooldown for repeated rate limits or temporarily deprioritizing glm in the routing pool until the root cause is found.
- github-copilot non-gpt-5-mini models remain failing; they are correctly excluded by cooldown machinery.

Performance and bottlenecks
- No major service tick stalls were observed today; tick cycle logs are clean.
- The notable bottleneck is external model rate limits (glm and some opencode models). These manifest as rate_limit outcomes and reduce effective throughput for affected models.

Learnings and prompts
- Error visibility matters: obscured or raw api_retry blobs in `task_runs.error` hide root causes and slow debugging. The recent fix to sanitize and surface the real error string was high leverage.
- Same-length diffing for stream capture reduces noisy output broadcasts — continue to validate `orch stream --pipe` behavior in real use.

Actionable priorities for tomorrow (morning review)
1. Investigate glm/opus rate-limit regression: gather last 50 glm run artifacts (stdout/stderr/output.json) and look for consistent retry headers, throttling responses, or client-side misclassification. If it's model-side throttling, increase model cooldown on rate_limit events. If client-side, harden retry/backoff.
2. Triage nemotron parse errors: inspect `task_runs.output` for nemotron failures and compare against parser expectations. If parser fixes are needed, file a small parser bug with examples.
3. Fix the version mismatch operationally: run `brew upgrade orch && brew services restart orch` on the operator host, and evaluate automating a daily upgrade tick to avoid recurring mismatch.
4. Assign and complete the cleanup timeout issue (git prune/pull timeouts) if still unassigned — it has a clear root cause in cleanup.rs and is medium complexity.
5. Confirm `orch stream --pipe` behavior with one or two live sessions to ensure same-length diffing behaves as intended in real usage.

Issues filed
- No new GitHub issues were filed during this retrospective. Existing problems are tracked in #2762 (glm investigation) and #2746 (cleanup timeouts) and remain the source of truth.

Prepared by Orch automation (internal:146229).

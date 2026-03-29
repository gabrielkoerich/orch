+++
title = "Morning Review -- 2026-03-29"
date = 2026-03-29T10:50:00Z
[extra]
job = "morning-review"
+++

## Summary

Solid morning check-in. The service is running and routing tasks; several recent fixes landed overnight that improve silent-agent handling, worktree cleanup, and auto-unblock behaviour. Opencode model instability (github-copilot/*) and third-party quota errors (kimi) continue to cause repeated failovers; mitigations (silence detection, cooldown escalation, pool exhaustion handling) are active and keeping the pipeline moving.

---

## Recent Activity (Last 24h)

Key commits observed in the last 24 hours (high level):

- `fix: prune stale .git/worktrees metadata at startup in reconcile_startup_worktrees` (#1230)
- `bug: auto_merge marks task Done before GitHub actually merges after rebase` (#1239)
- `fix: include needs_review in run_with_context success weight signal` (#1231)
- `fix: remove references to non-existent tasks.last_response column` (#1237)
- `bug: is_rate_limited check in run_with_context treats any rerouted task as rate-limited` (#1229)
- `fix: tighten classify_run_error_type substring matches for rate_limit and ci_failure` (#1233)
- `fix: treat set_fields failure as increment failure in auto_unblock` (#1215)

These changes focus on robustness: correct error classification, safer auto-unblock semantics, and worktree lifecycle fixes.

---

## Operational Health

- Service logs: recent activity shows multiple opencode silent-exit (exit 0 with no stdout) events; the engine detects silence, kills the session, cools down the model (typically 1h) and re-routes where possible.
- Kimi agent is returning 403 quota errors in several runs — these exhaust failover and move tasks to `needs_review`.
- No systemic CI failures observed in the last 24h that block the pipeline; most errors are agent/model availability and parse/silence patterns.

Task-run summary (sampled from recent runs): opencode/github-copilot/* models account for the majority of failures and silent exits; claude/codex runs show a high success rate when used as fallback.

---

## Stuck / Impacted Tasks

- Three recently-created external tasks (router/parse related) were routed to `kimi` and moved to `needs_review` after quota failures: GH issues 1235, 1232, 1227 (see issue tracker). These are waiting for owner/operator follow-up or retry with a different agent.
- Multiple internal bean tasks were dispatched and are in progress or queued; watch `internal:23565`, `internal:23580`, `internal:23583` to ensure they complete under current silence/cooldown rules.

No large backlog or blocked cascade observed — the system is failing over and marking tasks `needs_review` as designed when no recovery is available.

---

## Retrospective Follow-ups (carried from evening retro)

- opencode silent exit-0 root cause: mitigated by silence detection, cooldown escalation, and pool exhaustion handling, but root cause (model/CLI behavior) remains unresolved — continue investigation.
- update SKILL.md / operational docs: add notes on silence detection, cooldown behavior, and how config maps to temporary model disabling (low priority but useful for on-call).
- continue work on auto_unblock correctness (counter resets and increment failures) — relevant fixes landed but a couple of counter/edge-case bugs remain open in issues.

---

## Today's Priorities

1. Monitor opencode failure rate (github-copilot/*). If failure rate does not drop after mitigations, consider disabling those models in the pool or reduce their routing weight.
2. Address Kimi quota failures: either wait for quota refresh, rotate to a different review agent, or update routing to avoid kimi until quota is restored.
3. Fix `auto_unblock_count` counter edge-cases and ensure increments/reset behaviour is correct across block reason transitions.
4. Keep an eye on the three external issues routed to `kimi` (1235, 1232, 1227) and re-route/retry once fallbacks are healthy.
5. Capture operational learnings in SKILL.md (silence detection and cooldown escalation) — low urgency but useful for on-call docs.

---

Files changed / touched for this post:

- `docs/content/posts/morning-review-2026-03-29.md`

If any of the items above need escalation, file an operational bug (up to 2–3 issues). Current GH open issues already cover the main operational problems (router/opencode/kimi/auto_unblock); no duplicate issues filed here.

+++
title = "Morning Review — 2026-03-06 (fourth pass, 01:11 UTC)"
date = 2026-03-06
description = "Colon bug still active, duplicate session error on internal:12, internal:14 at max attempts, Claude rate-limited"
+++

## Summary

No new code since the third-pass review (01:06 UTC). The service is still running v0.10.6 with three unresolved failure modes for internal tasks. All root causes have open PRs and issues — no new issues needed.

---

## Recent Changes (last 24h)

| Commit | Description |
|--------|-------------|
| `a128b73` | docs: add morning review 2026-03-06 third pass |
| `31a54d4` | docs: add morning review 2026-03-06 second pass |
| `1dd6bd3` | fix: recover internal tasks stuck in in_progress after engine restart (#440) |

No new code since third pass.

---

## Live Service Status (v0.10.6, 01:11 UTC)

### Active failure modes

**1. Colon bug (internal:11, internal:13)** — Root cause: tmux session name `orch-orch-internal:13` is parsed as `session:window` by tmux. The `set-environment` call finds "no such session" and fails. Agent exits -1, falls back to opencode, resets to `new`. Repeats every tick.

- Fix: PR #434 (this branch) and PR #432 — neither merged yet.

**2. Duplicate session (internal:12)** — Root cause: session `orch-orch-internal_12` already exists from a previous run. The underscore sanitization is working, but the stale session isn't cleaned up before re-dispatch. The engine tries `tmux new-session` and fails with "duplicate session."

- Fix: PR #442 (stuck in_progress recovery) — not merged yet.

**3. Max attempts exceeded (internal:14)** — Hit 10/10 attempts. Will not be dispatched further until reset.

- No PR needed: `orch task unblock internal:14` resets it, but PR #453 (`orch task unblock` for internal tasks) is required for the CLI to work.

### Rate limiting

- Claude weight: 0.05, 47 hits. Most tasks routing to opencode/kimi via fallback.
- Auto-recovers over time — no action needed.

### Review agent

- Task #443 review agent timed out, reset to NeedsReview for retry (rate limit likely cause).
- Tasks #448, #446, #431 review agents spawned at 01:11 UTC (codex/opencode).

---

## Open PRs Status

| PR | Fix | Status |
|----|-----|--------|
| #434 | Colon sanitization (this branch) | Open, not merged |
| #432 | Colon sanitization (internal:9 branch) | Open, not merged |
| #456 | WeightSignal::Blocked for delegated tasks | Open |
| #454 | Engine health checks include internal tasks | Open, in review |
| #453 | orch task unblock for internal tasks | Open, in review |
| #451 | orch task status includes internal tasks | Open, in review |
| #447 | Docs: PLAN.md portable-pty cleanup | Open |
| #445 | External task NeedsReview on infra failure | Open |
| #444 | Bidirectional channel interaction | Open, in review |
| #442 | Stuck in_progress recovery | Open |
| #428 | Harden internal task dispatch | Open |

---

## Checklist

1. **Stuck/failing tasks** — Yes: internal:11, internal:12, internal:13, internal:14 all failing. Root causes tracked (colon bug, duplicate session, max attempts). PRs #434/#432/#442 address them.

2. **Test gaps** — No new test gaps identified beyond what's tracked in existing issues.

3. **Log patterns** — New pattern vs. third pass: `duplicate session: orch-orch-internal_12`. Previous passes only showed the colon-based "no such session." This confirms two separate bugs (colon + stale session), both addressed by existing PRs.

4. **Script simplification** — Nothing to simplify; no scripts changed.

5. **GitHub issues** — All 6 open issues are actively being worked (PRs exist for each).

---

## No New Issues Filed

All active failure modes have existing issues and open PRs. The service will recover once PRs #434/#432 (colon fix) and #442 (stuck session recovery) are merged and deployed.

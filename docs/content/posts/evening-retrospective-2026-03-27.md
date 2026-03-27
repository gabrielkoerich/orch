+++
title = "Evening Retrospective -- 2026-03-27"
date = 2026-03-27T23:59:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

High-volume day: 24 commits, 10 issues closed. The dominant theme is **operational robustness** — fixing silent failures, race conditions, and edge-case corruption in the dispatch, review, and worktree subsystems. The new **task activity log table** provides debugging visibility. A critical bug in opencode silent exit-0 handling was mitigated but not eliminated.

---

## Accomplished Today

### Silent Agent Detection & Recovery (4 fixes)

- **Silence detection via tmux capture** (#1136) — detect agents that exit 0 with no output; cooldown model and re-route after a grace period.
- **Silence detection loops within same agent** (#1144) — short agent cooldown to force re-route to a different agent.
- **Retry opencode free model on silent exit-0** (#1135) — before falling through to claude, retry the same model once; reduces false failovers.
- **Rate limit context extraction** (#1105) — cooldown model on silent exit 0 when rate limit is detected.

### Worktree & Branch Management (5 fixes)

- **Propagate remove_worktree_and_branch failure** (#1143) — prevents orphaned worktrees that remain marked as cleaned.
- **Restore worktree from origin/<branch> when local branch deleted** (#1137) — recovers tasks after branch deletion.
- **Preserve remote branch on startup rebase failure when task has open PR** (#1138) — avoids closing PRs prematurely.
- **Clear stale push failures and add has_commits guard** (#1145) — prevents false push_failed detection.
- **Cleanup fallback and JSON decode retries** (#1121) — robustness in worktree cleanup path.

### Review Pipeline Hardening (5 fixes)

- **Warn on git fetch failure before building review diff/log** (#1134) — prevents silent stale refs (follow-up to #1039).
- **Push review branch before comment** (#1122) — ensures review comments are posted on up-to-date branch.
- **Scan all JSON blobs and prefer best AgentResponse match** (#1118) — parser picks the most complete response, reducing parse errors.
- **Reset review_cycles to 0 when task transitions to needs_review** — prevents stale counter blocking future reviews.
- **Include internal tasks in check_merged_prs** (#1103) — merged PRs now unblock NeedsReview internal tasks.

### Cron & Job System (3 fixes)

- **Support aliases with optional parameters** (#1109) — cron aliases like `@daily` can take optional args.
- **Split 0-N DOW ranges to include Sunday correctly** (#1104) — cron `0-5` now includes Sunday.
- **Normalize DOW mapping** (#1094) — "0-5" previously mapped to "1-5", dropping Sunday.

### New Feature

- **Task activity log table** (#1133) — tracks all events per task in SQLite for debugging; enables timeline reconstruction.

### Bug Fixes & Improvements

- **Dispatchable tasks log fires before dispatch guard check** (#1131) — fixes misleading count in logs.
- **Early return in runner skips tmux cleanup** (#1142) — prevents session leaks and secret exposure (still in progress).
- **Malformed delegation JSON in store never cleared** (#1141) — silently fails on re-dispatch (needs review).
- **Self‑improvement: debug agent errors and fix root causes** (#1148) — meta‑issue for improving error diagnostics.

### Documentation

- **Align sync interval defaults** (#1132) — updated stale 120s references to 45s.
- **Fix task status semantics in AGENTS.md** — clarified `needs_review` is automatic, `blocked` needs human.
- **Audit and update documentation** (#1108) — comprehensive pass over recent changes.

---

## What Failed / Needed Escalation

### Still Open

| ID | Status | Title |
|----|--------|-------|
| #1142 | in_progress | early return in runner skips tmux cleanup — leaks sessions and leaves secrets in tmux global env |
| #1141 | needs_review | malformed delegation JSON in store is never cleared — silently fails on every re-dispatch |
| #1149 | in_review | persistent chat sessions + research cross-agent session handoff |

#1142 is an opencode task that is still in progress — likely stuck due to the opencode silent exit‑0 pattern. #1141 is a delegation JSON corruption bug awaiting review. #1149 is a feature in review (chat sessions).

### Recurring Pattern: opencode Silent Exit‑0

The morning review flagged opencode agents exiting with code 0 and no output. Today’s fixes (#1135, #1144, #1136) mitigate but do not eliminate the issue. The root cause appears to be opencode streaming non‑JSON lines or terminating silently. The mitigation is to cooldown the model and re‑route to a different agent (usually claude).

---

## Routing Accuracy

Today’s closed issues used a mix of agents:

| Agent | Issues Closed |
|-------|--------------|
| claude | ~7 |
| opencode | ~3 |

Complexity routing: mostly `simple` and `medium`. One `complex` task (#1148) — a meta‑issue about debugging agent errors.

Routing appears accurate — opencode failures were caught by silence detection and re‑routed to claude. However, the router itself failed to parse a routing response from `opencode/minimax‑m2.5‑free` (streaming JSON before result), causing a cooldown and fallback. This suggests opencode’s output format may be inconsistent.

---

## Patterns & Health

**Positive:**

- **High throughput**: 24 commits, 10 issues closed — sustained pace of fixes.
- **Systematic approach**: Silent detection, worktree recovery, and review safety are being addressed in a coordinated way.
- **New debugging tooling**: Task activity log table enables post‑mortem analysis of task timelines.

**Concerning:**

- **opencode reliability**: The silent exit‑0 pattern is recurring and not fully resolved. Internal tasks are failing over to claude, increasing cost and latency.
- **Three open issues**: Two are bugs (#1142, #1141) that could affect dispatch reliability. One is a feature in review (#1149).
- **No changes to SKILL.md**: Operational learnings about opencode failures and silence detection are not yet reflected in the skill documentation.

---

## Open at End of Day

| ID | Status | Title |
|----|--------|-------|
| #1149 | in_review | persistent chat sessions + research cross‑agent session handoff |
| #1142 | in_progress | early return in runner skips tmux cleanup — leaks sessions and leaves secrets in tmux global env |
| #1141 | needs_review | malformed delegation JSON in store is never cleared — silently fails on every re‑dispatch |

---

## Tomorrow's Priorities

1. **Monitor #1142 and #1141** — both are dispatch‑critical bugs. #1142 may need an opencode agent restart or timeout.
2. **Follow up on opencode silent exit‑0** — consider a dedicated investigation: is it a model issue (github‑copilot/*) or an opencode CLI bug? If persistent, route internal tasks away from opencode.
3. **Update SKILL.md** — add notes about silence detection, cooldown behavior, and opencode reliability patterns discovered today.
4. **Review #1149** — persistent chat sessions feature is in review; ensure it aligns with control session architecture.
5. **CLI version drift** — still unresolved (0.37.6 vs 0.37.11). Run `brew upgrade orch && brew services restart orch`.
+++
title = "Evening Retrospective — 2026-03-03"
date = 2026-03-03
description = "Agent workflow hardening landed, streaming fixed, and routing loops cleaned up"
+++

## Summary

Strong ops day focused on **agent reliability**. The biggest wins were eliminating git-fetch failures via pre-fetching, fixing `orch stream` output delivery, and breaking a `needs_review` re-route loop. Several resilience improvements landed around GitHub rate limits, webhook deduplication, and re-routing failed agents with no commits. Service health looks stable.

---

## Morning Review Recap

| Priority | Outcome |
|----------|---------|
| Monitor status-based review workflow | No regressions observed; review gate loop with no PR fixed. |
| Consider reducing stuck threshold | Not addressed today. |
| Keep an eye on review edge cases | Review gate loop fixed; no new stuck `in_review` reports. |

---

## Tasks Completed Today

| Area | Changes | Notes |
|------|---------|-------|
| Agent workflow | pre-fetch branches + remove git fetch from agent prompt; re-route on agent failure with no commits; break `needs_review` re-route loop | Eliminates sandbox git-fetch failures and prevents infinite reroute loops. |
| Streaming & sessions | `orch stream` output fixed; duplicate tmux session creation fixed | Restores live output observability and avoids duplicate sessions. |
| GitHub sync reliability | proactive API rate limit handling; webhook delivery dedupe; review gate loop when no PR fixed | Fewer stuck tasks during API blips and no-PR review loops. |
| Performance | N+1 GitHub API calls removed in dashboard/status | CLI should be noticeably faster. |
| Opencode integration | permissions blocked by global config fixed; NDJSON review response parsing fixed | Reduces opencode-specific failures. |

---

## What Didn't Go Well

- **GitHub API instability** continues to surface (rate limits, transient failures). Mitigation landed today, but this remains a recurring risk.
- **Routing loop risk**: the `needs_review` re-route loop fix suggests earlier logic allowed repeated re-dispatches without progress. Fixed, but worth monitoring.

---

## Prompt Effectiveness

| Prompt | Assessment |
|--------|-----------|
| `prompts/agent_system.md` | Clear and strict; workflow checklist and sandbox constraints are explicit. |
| `prompts/route.md` | Good; label-bias guidance is explicit and should reduce misrouting. |
| `prompts/review_task.md` | Strong structure, but still instructs `git fetch` which conflicts with sandbox guidance. Recommend aligning with “no fetch” workflow. |

No prompt edits were made today; alignment between review workflow and sandbox constraints is the main candidate.

---

## Routing Accuracy

- No obvious misroutes today. The fixes were about **failure recovery**, not incorrect executor selection.
- Re-route logic is now more conservative when agents fail without commits, which should reduce wasted cycles.

---

## Performance & Bottlenecks

- **GitHub API**: rate-limit handling improved; still a bottleneck during spikes.
- **Webhook delivery**: deduping should reduce redundant sync work.
- **CLI performance**: N+1 API calls removed in dashboard/status.
- **No lock contention** observed in the commit set; live streaming now works again.

---

## New Issues Filed

None. `gh issue list --state open` failed due to GitHub API connectivity, so no new issues were created to avoid duplication.

---

## Tomorrow's Priorities

1. **Align review prompt with sandbox constraints** (remove `git fetch` step, rely on pre-fetched refs). Confirm no open issue already exists before filing.
2. **Monitor API rate limit behavior** with the new proactive handling under real load.
3. **Watch re-route behavior** for `needs_review` tasks to ensure the loop fix sticks under concurrent task traffic.

---

## Evening Update — 2026-03-03 (Late)

### Morning Review Follow-ups

| Priority | Status | Notes |
|----------|--------|-------|
| Align review prompt | ✅ Done | `prompts/review_task.md` now uses `git rebase origin/main` instead of `git fetch`; matches agent_system.md guidance. |
| Reduce stuck thresholds | ❌ Not done | Still pending; low priority but would improve responsiveness. |
| Monitor review edge cases | ✅ Done | Review gate loop fix holding; no new stuck `in_review` reports. |

### Today's Commits (Major Security & Reliability Push)

The following significant changes landed today:

| Commit | Description | Impact |
|--------|-------------|--------|
| `8c630f0` | Add tmux session env helpers, avoid embedding GH_TOKEN in runner scripts | Security: tokens no longer in per-task scripts |
| `6a467ee` | Integrate async GitHub App token resolution in GhHttp | Reliability: native auth without gh CLI |
| `a78c955` | CI: scope contents:write permission to release job only | Security: least privilege |
| `6601d25` | fix: improve capture diffing | Reliability |
| `843e1c5` | Drop runtime gh dependency from Homebrew formula; prefer native GhHttp | Simplicity: fewer runtime deps |
| `e89b234` | Secure GH_TOKEN handling | Security |
| `0157f63` | Make GhHttp primary for PR creation, remove gh CLI fallback | Reliability |
| `22bca09` | Tests: prevent secret leakage in task artifacts | Security |
| `21f0396` | Consolidate gh CLI usage, add safe wrapper | Reliability |
| `f4a16e3` | Eliminate on-disk runner scripts — PTY-based agent runner | Simplicity |
| `c27a1b2` | Add GitHub App auth and robust token resolver | Reliability |

**Theme**: Major hardening around token handling, removing gh CLI dependency, and improving auth robustness.

### Current Open Issues

| Issue | Status | Agent | Priority |
|-------|--------|-------|----------|
| #404 | in_progress | minimax | Evening retrospective (this task) |
| #395 | needs_review | claude | Discord Gateway WebSocket |
| #386 | needs_review | claude | PTY-based runner (done, needs merge) |
| #378 | in_progress | kimi | Centralize token resolver |
| #372 | needs_review | opencode | Code development |

### Observations

1. **Prompt alignment complete**: Both `agent_system.md` and `review_task.md` now consistently instruct agents to use `git rebase` instead of `git fetch`, matching orch's pre-fetch workflow.
2. **Security posture improved**: GH_TOKEN no longer embedded in per-task runner scripts; GitHub App auth integrated natively.
3. **gh CLI dependency reduced**: Native `GhHttp` now handles PR creation and other GitHub API operations.
4. **Test failures observed**: Some CI test failures occurred (see recent `gh run list`); may need investigation but review gate passed.

### Tomorrow's Priorities (Updated)

1. **Investigate test failures** — check recent CI failures for root cause
2. **Merge #386** — PTY runner change is `needs_review`, should be ready
3. **Stuck threshold reduction** — revisit lowering timeouts if not done
4. **Monitor #378** — token resolver centralization in progress

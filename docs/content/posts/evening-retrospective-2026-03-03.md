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

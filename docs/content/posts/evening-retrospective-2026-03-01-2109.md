+++
title = "Evening Retrospective — 2026-03-01 (21:09 UTC)"
date = 2026-03-01
description = "Record productivity day: 25+ commits, 9 PRs merged, but auto-merge pipeline stalling on approved PRs"
[taxonomies]
tags = ["evening-retrospective", "maintenance"]
+++

## Summary

Hugely productive day. 25+ commits landed, 9 PRs merged, and multiple critical bugs fixed across the sidecar system, git operations, agent prompts, and CI handling. The second half of the day focused on CI failure recovery and agent prompt hardening. However, a clear bottleneck emerged: **4 approved PRs are stuck because the auto-merge pipeline doesn't retry after CI passes**.

---

## Morning Review Follow-Up

The morning review (#235) identified three priorities:

| Priority | Outcome |
|----------|---------|
| Deploy PR #233 (crash-loop backoff) | **Not done** — PR still open, CI failing (`cargo fmt`), no review dispatched |
| Prioritize #231 (build_full_context bypass) | **Done** — task completed, memory/context loading now works |
| Monitor #234 merge (detect_default_branch) | **Partial** — PR #238 approved and CI green, but not auto-merged |

---

## What Went Well

### 9 PRs Merged Today

| PR | Fix | Impact |
|----|-----|--------|
| #245 | File locking for sidecar JSON read-modify-write | Prevents state corruption under concurrency |
| #240 | Empty branch name guard in git_ops | Stops `[branch ""]` git config corruption |
| #239 | GhHttp::list_comments pagination | Review lifecycle works on busy PRs |
| #236 | PLAN.md update | Documentation current |
| #229 | Previous evening retrospective | Context preserved |
| #222 | Check for open PR before spawning review | Prevents duplicate review agents |
| #221 | Dead code cleanup | Cleaner codebase |
| #217 | Backoff only on actual rate limits | Fewer false 403 retries |
| #216 | Prompts from .rs to .md files | Prompts are now editable without recompile |

### Key Fixes in Second Half

- `3c4b354` — Agent prompt pulls remote branch before rebasing on main
- `a7f9174` — Re-dispatch agent on CI failure instead of posting useless comments
- `ef20e3d` — Rebase PR on main when CI fails
- `2fd899a` — Fix codex `--instructions` flag (pipe sys prompt via stdin)
- `98f919a` — Allow `dead_code` on CI helper methods

### Agent Performance

- **11 tasks processed** — 5 done, 6 in review, 0 permanently failed
- **Claude agents**: 100% success rate across all model tiers
- **Cost**: ~$8 total for 11 tasks; complexity→model routing working well (opus for complex, haiku for simple)
- **Recovery**: codex failures (5/5) all auto-rerouted to claude successfully

---

## What Failed

### 1. Auto-Merge Pipeline Stall (4 PRs Stuck)

Four PRs are approved by the review agent but not merging:

| PR | Status | Issue |
|----|--------|-------|
| #233 | No review dispatched, CI failing | `cargo fmt` needed; task #225 stuck in `in_review` |
| #238 | Approved, CI **passing** | Auto-merge posted "CI failing" on an older run, never retried |
| #243 | Approved, CI failing | `cargo fmt` — needs rebase on main |
| #246 | Approved, CI failing | `cargo fmt` — needs rebase on main |

**Root cause**: The `review_open_prs` sync tick checks CI status and posts a comment, but doesn't retry when CI later passes. Once it posts "CI checks are failing", the PR is effectively abandoned unless the engine re-dispatches the agent. PRs #243 and #246 also need rebase on main to pick up recent formatting fixes — the re-dispatch mechanism (`a7f9174`) should handle this but hasn't triggered for already-approved PRs.

### 2. Codex Agent: 100% Failure Rate

All 5 codex routing attempts failed with `error: unexpected argument '--instructions' found`. This was fixed by commit `2fd899a` (pipe sys prompt via stdin instead), but the fix hasn't been deployed to the brew service yet. Every codex routing wastes an attempt cycle + reroute latency.

### 3. PR #233 Never Reviewed

Task #225 is `in_review` but PR #233 has 0 comments and 0 reviews. The review agent was never dispatched for this PR, or dispatched but failed silently. This is the crash-loop backoff fix — a critical deployment blocker.

---

## Prompt Effectiveness

| Prompt | Assessment | Action |
|--------|-----------|--------|
| `agent_system.md` | **Strong** — clear workflow, retry guidance, output format all effective. Updated twice today. | None needed |
| `agent_message.md` | **Good** — well-structured template with conditional sections. Memory was dead code until #231 fix today. | Will improve with #231 merge |
| `review_system.md` | **Weak** — only 16 lines. No guidance on CI checks, base branch, performance regressions, or large diffs. | Needs strengthening |
| `review_task.md` | **Good** — clear JSON format, binary decision model. Missing test command guidance. | Minor improvement |
| `route.md` | **Good** — complexity routing works well. But static agent descriptions don't reflect dynamic reliability. | Consider health-aware routing |

---

## Routing Accuracy

Complexity-based model routing is working correctly:
- Simple tasks → haiku ($0.01-0.02/task)
- Medium tasks → sonnet ($0.01-0.25/task)
- Complex tasks → opus ($1.01-3.88/task)

No routing mismatches observed. The cost profile is reasonable.

The only routing issue is codex: the router still sends tasks to codex based on static descriptions despite 100% failure rate. The reroute mechanism handles this correctly, but it adds latency.

---

## Performance Bottlenecks

1. **Auto-merge doesn't retry**: Once `review_open_prs` sees a CI failure and comments, it doesn't re-check. PRs that pass CI later are orphaned.
2. **Codex waste cycle**: Every task routed to codex burns ~30s before failing and rerouting to claude. Fixed in code but not deployed.
3. **`cargo fmt` drift**: PRs created before formatting fixes on main fail CI. The re-dispatch mechanism should handle this but isn't triggering for already-approved PRs.

---

## Tomorrow's Priorities

1. **Fix auto-merge retry logic**: The `review_open_prs` function should re-attempt merge when CI passes, not give up after one "CI failing" comment. This is blocking 3+ PRs right now.
2. **Deploy latest fixes to brew**: Commits `2fd899a` (codex fix), `a7f9174` (CI re-dispatch), `3c4b354` (prompt rebase fix) need to reach the brew binary. Update the Homebrew formula and `brew upgrade orch`.
3. **Unblock PR #233**: The crash-loop backoff fix has been stuck all day. Rebase on main, run `cargo fmt`, push.
4. **Merge PR #238**: Already approved and CI passing — just needs the auto-merge to retry, or manual merge.

+++
title = "Evening Retrospective — 2026-02-21"
date = "2026-02-21"
+++

## Summary

Today (2026-02-21) the orchestrator was restarted at 18:41 UTC running v0.44.0 with the new GitHub Issues native backend. Two development tasks completed successfully with PRs approved and merged. However, several systemic issues were identified: the morning-review job has never fired due to a cron catch-up bug, nine tasks crashed immediately at startup due to the backend incorrectly picking up `needs_review` issues as runnable, and agents cannot run the test suite from worktrees due to sandbox restrictions.

---

## Morning Review

**Morning review did NOT run today.** Issue #140 (`Daily morning review`) was in `status:needs_review` state and crashed at startup rather than producing a useful review.

### Root Cause: jobs_tick.sh catch-up bug

`jobs.yml` shows `morning-review` job with `last_run: null`. Looking at `scripts/jobs_tick.sh` lines 53–71:

```bash
if [ -n "$LAST_RUN" ] && [ "$LAST_RUN" != "null" ]; then
    # catch-up since last_run — WORKS
elif ! python3 cron_match.py "$SCHEDULE"; then
    continue  # skip if current minute doesn't match
fi
```

When `last_run` is null, the code **only fires at the exact cron minute** (8:00 AM). There is no catch-up. Since the orchestrator was not running at 8:00 AM UTC, the morning-review job never fired. The `evening-retrospective` job works because it had a `last_run` value set, enabling catch-up detection.

**Fix needed**: When `last_run` is null, fall back to a 24h catch-up window (e.g. treat as "since 24h ago") rather than skipping entirely.

---

## Tasks Completed Today

| # | Title | Agent | Duration | Outcome |
|---|-------|-------|----------|---------|
| #157 | Add --dry-run flag to add_task.sh | codex/gpt-5.2 | 13m | PR #160 open, approved |
| #158 | Add task count to orch status output | codex/gpt-5.2 | 15m (timeout) | PR #162 merged |

### Task #157 — Add --dry-run flag to add_task.sh
- Completed in 13 minutes. PR #160 created.
- **Noteworthy**: Agent tried to run `bats tests/orchestrator.bats` twice but was denied by sandbox. Tests could not be validated before PR.
- PR first got `request_changes` from a `/` project review, then `approve` from the orchestrator project review — inconsistent review behavior.
- PR #160 is open, auto-merge enabled.

### Task #158 — Add task count to orch status output
- Hit the 900s task timeout, exiting with code 124.
- Despite the timeout, the agent completed its work and created PR #162.
- PR #162 was approved by review agent and auto-merged. ✅
- Issue #158 closed.
- **Issue**: The 15-minute timeout is too tight for complex codex tasks that include reading, editing, testing, committing, and pushing.

---

## Failed / Crashing Tasks

Nine tasks crashed immediately at startup with `exit=1 at line 1`:

| Issue # | Title | Previous Status |
|---------|-------|----------------|
| #140 | Daily morning review | needs_review |
| #142 | Add CONTRIBUTING.md | needs_review |
| #126 | Deduplicate system.md prompt instructions | needs_review |
| #125 | Prevent retry loops on env failures | needs_review |
| #124 | Add environment validation | needs_review |
| #53 | Configurable status column names | needs_review |
| #51 | orch project list | needs_review |
| #50 | Owner feedback commands | needs_review |
| #49 | @orchestrator mentions | needs_review |

**Pattern**: All crashing tasks have `status:needs_review` label. The orchestrator appears to be picking these up as runnable at startup and attempting to run them, but they crash immediately. This wastes resources on every restart and produces noisy log output.

**Likely cause**: The GitHub Issues backend may be reading `needs_review` issues as runnable (e.g., treating any non-`done`, non-`blocked` open issue as a candidate). Alternatively, there may be a stale lock or missing worktree condition causing the crash.

---

## PRs and Reviews

| PR | Title | Outcome |
|----|-------|---------|
| #162 | feat(status): show open task count | Approved → Merged ✅ |
| #160 | chore: add PR helper scripts (dry-run) | request_changes → approve → open |
| #159 | feat: label validation, model visibility | Merged ✅ |
| #141 | docs: pin CI badge to main | Merged ✅ |
| #139 | feat: beads integration, project-local worktrees, tmux | Merged ✅ |

### Review Inconsistency on PR #160
PR #160 received two different verdicts:
1. First review (from `/` project context): `request_changes`
2. Second review (from orchestrator project context): `approve`

This inconsistency likely stems from different review contexts. The first review may have used a different prompt template or project scope. Should investigate if the `review_prs.sh` script is running reviews from the correct project context.

---

## Routing Analysis

| Task | Routed To | Complexity | Decision Quality |
|------|-----------|------------|-----------------|
| #157 | codex | medium | Correct — shell script feature |
| #158 | codex | medium | Correct — multi-file status feature |
| #161 (this) | claude | medium | Correct — analysis/synthesis |
| #142 | codex | simple | Correct — single doc file |

**Router performance**: ~10–15 seconds per route using claude/haiku. Routing decisions are accurate. Model selection is appropriate (gpt-5.2 for medium codex tasks, sonnet for claude tasks).

**No routing errors observed.** The route.md prompt is clear and well-structured.

---

## System Prompt Analysis

### system.md
- Clear and concise. No obvious issues.
- The `{{BRANCH_NAME}}` and `{{GH_ISSUE_NUMBER}}` placeholders are not being substituted in the system prompt — they appear literally in the prompt. This means agents see `git push -u origin {{BRANCH_NAME}}` which they must interpret contextually. Consider making these substitutions explicit.

### route.md
- Clean and focused. Complexity guidance is helpful.
- Router correctly ignores historical labels when making decisions.

### agent.md
- Well-structured context injection.
- Skills docs are included when relevant.

---

## Performance Bottlenecks

1. **Morning-review never fires** — `last_run: null` catch-up bug blocks the job indefinitely unless the orchestrator is running at exactly 8:00 AM UTC.

2. **Startup crash loop** — 9 needs_review tasks crash on every orchestrator restart. Each crash takes ~2 seconds and generates error log entries. On a busy day with multiple restarts, this is significant noise.

3. **Test suite blocked** — Agents cannot validate changes before committing because `bats tests/orchestrator.bats` is blocked by the sandbox. This reduces code quality confidence.

4. **Task timeout at 900s** — Complex codex tasks that include read → edit → test → commit → push → PR can exceed 15 minutes. Task #158 hit this limit but still succeeded. Should increase to 1200–1800s or make configurable by complexity.

5. **Duplicate improvement issues** — Previous retrospectives created duplicate tasks (#124=#154, #125=#145, #123=#144) because tasks failed and new retrospectives didn't check for existing open issues on the same topic. Need deduplication logic.

---

## Prior Retrospective Follow-up

Issues created by 2026-02-19 retrospective (#123–127):

| Issue | Title | Status |
|-------|-------|--------|
| #123 | Investigate morning-review job | CLOSED — but job still broken! |
| #124 | Add environment validation | OPEN, needs_review, crashing |
| #125 | Prevent retry loops | OPEN, needs_review, crashing |
| #126 | Deduplicate system.md | OPEN, needs_review, crashing |
| #127 | Enhance GIT_DIFF for retries | CLOSED ✅ (PR #138 merged) |

PRs created for #123–126 were **closed without merging** (PRs #134–137). The improvements were lost. Then new duplicate issues were created (#144, #145, #148, #154) by subsequent retrospectives.

---

## New Improvement Tasks Created

1. **[P0] Fix morning-review job catch-up when last_run is null** — jobs_tick.sh treats null last_run as "only fire at exact minute", blocking catch-up. Should fall back to 24h lookback.

2. **[P0] Stop orchestrator from re-running needs_review tasks** — At every startup, 9 needs_review tasks crash. The GitHub backend needs to exclude needs_review issues from the runnable pool.

3. **[P1] Allow bats tests in agent sandbox** — `bats tests/orchestrator.bats` is blocked. Add bats to the allowed commands or document the workaround.

4. **[P1] Increase default task timeout** — 900s is too short for medium-complexity codex tasks. Increase to 1800s or make complexity-based.

5. **[P2] Deduplicate open improvement issues** — Close older duplicate issues (#124, #125, #126) in favor of their newer equivalents (#154, #145) that are currently in_review.

---

## Tomorrow's Morning Check-in Priorities

1. **Morning review should fire** — if the jobs_tick fix was applied. Verify `last_run` is being set correctly in jobs.yml.
2. **Check PR #160** (dry-run flag) — still open, verify CI passes and merge.
3. **Review in_review tasks** (#144, #145, #148, #154, #157) — check if any are ready to merge.
4. **Verify startup crash loop is resolved** — no more needs_review tasks crashing at line 1.
5. **Confirm token budget** — check if any agents hit usage limits today (no evidence today, but worth monitoring).

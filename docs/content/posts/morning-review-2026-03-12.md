+++
title = "Morning Review — 2026-03-12"
date = 2026-03-12T00:00:00Z
[extra]
job = "morning-review"
+++

## Summary

Productive past few days since the last morning review (2026-03-03). The orchestrator gained major
reliability improvements: merge-conflict rebase flow, review agent throttling, internal task PR
support, NDJSON format fixes, and a new CLI agent/live-view feature. Two agent-authored PRs are
currently in review (#534, #535) and a new unescaped bug was filed (#536). Three open issues
remain, all actionable with PRs in flight.

---

## Recent Changes (last cycle since 2026-03-07)

| Commit | Description |
|--------|-------------|
| `97b07e9` | chore: remove .opencode/ from git tracking |
| `322f73f` | feat(cli): show agent in task list and live views (#531) |
| `6db6773` | fix: correct SCHEMA_V4 backfill order and classify needs_review as success (#530) |
| `e1e872d` | test: skip integration agent env-specific failures (#528) |
| `92a82f5` | fix: use specific stash ref in rebase_on_default to avoid cross-worktree contamination (#527) |
| `63dc4db` | perf: replace 7 sequential SQLite queries with one in scan_mentions (#526) |
| `d35aeac` | fix: mark task done when merged PR has no open PR (#525) |
| `bdb169d` | fix: include internal tasks in review_open_prs PR check (#522) |
| `1c3eaa6` | fix: use format_task_ref in auto_commit and missing-PR body (#521) |
| `b4a8964` | feat(cli): show agent output in orch task logs with safe UTF-8 truncation (#516) |
| `1b8bd68` | fix: throttle review agents in sync tick (#515) |
| `bc6837d` | fix: skip cooled-down router LLM agent and detect rate limits |
| `407cd6d` | fix: use configured default branch in merge-conflict rebase (#511) |
| `c62ce12` | fix: rebase worktree on merge conflict instead of re-triggering review |
| `3553c5f` | fix: handle codex NDJSON format in review response parser |
| `4c12134` | fix: block tasks at max attempts instead of marking done; fix Zola taxonomy |

Notable highlights:
- **SCHEMA_V4 backfill fix** (#530): `needs_review` was incorrectly classified as failure in metrics — now corrected.
- **Cross-worktree stash contamination** (#527): Rebase was using wrong stash ref when multiple worktrees existed simultaneously — now uses specific stash ref.
- **SQLite scan_mentions perf** (#526): 7 sequential queries replaced with a single query.
- **Codex NDJSON format** (#3553c5f): Review parser now handles both opencode and codex NDJSON formats.

---

## Health Check

**CI status**: Two CI runs are in progress on agent-authored branches (`gh-task-532` and `gh-task-533`). All recent main-branch CI runs passed.

**Open PRs**:
- PR #534 — `fix: cleanup_task_worktree always no-ops due to TTL=24h` (task #532, in review)
- PR #535 — `fix: inconsistent merge_conflict_retries limit` (task #533, in review)

Both PRs have agent-authored commits; CI runs are in progress. Review gate workflow is running.

**Open Issues (3 total)**:
| # | Title | Status |
|---|-------|--------|
| #536 | fix: review gate loops indefinitely when auto-PR creation fails | Open (bug, unassigned) |
| #533 | fix: inconsistent merge_conflict_retries limit | In review (PR #535) |
| #532 | fix: cleanup_task_worktree always no-ops due to TTL=24h | In review (PR #534) |

**Service logs**: `~/.orch/state/orch.log` not present in worktree; brew service logs available at
`/opt/homebrew/var/log/orch.log` and `/opt/homebrew/var/log/orch.error.log`.

**No stuck tasks observed** via open issue list or recent commit messages. The stash contamination
fix (#527) and the no_session recovery logic should keep stuck tasks minimal.

---

## Issues Filed

No new issues filed this session. Issue #536 (review gate infinite loop on PR creation failure) was
filed yesterday and remains open and unassigned. It is a high-priority reliability bug — the
`needs_review → in_review → Failed → needs_review` loop consumes review quota and pollutes logs
until human intervention.

---

## Action Items

1. **Issue #536 — review gate infinite loop**: Highest priority. No counter/escape hatch exists when
   `auto-PR creation` fails. The fix described in the issue is straightforward:
   - Add `pr_create_failures` counter in sidecar
   - After 3 consecutive failures, return `ReviewDecision::Blocked` variant
   - Handle in `tick.rs` by setting `Status::Blocked`
   This should be dispatched to an agent today.

2. **Monitor PRs #534 and #535**: Both are in review. If CI passes and review gate approves, they
   will merge automatically. Verify both land on main before end of day.

3. **Evening retrospective job**: Last filed retrospective was 2026-03-06 (issue #413, closed). The
   job scheduler should be creating these nightly. If no retrospective task has run since then,
   investigate the cron expression in `.orch.yml`.

---

## Notes

The last morning review post in this repo is dated 2026-03-03. This is the first morning review
since then — a 9-day gap. The morning review job cron should be validated to confirm it is firing
daily. The internal task #43 that triggered this review suggests the job scheduler is working,
but the post gap indicates the docs commit step may have been failing silently on prior runs.

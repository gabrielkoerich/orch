+++
title = "Evening Retrospective - 2026-04-16"
date = 2026-04-16
description = "Daily retrospective: 13 reliability commits landed, service version is in sync, no open issues remain, and most observed failures were already explained by fixes merged today."
+++

# Evening Retrospective - 2026-04-16

## Recent Commits (12h)

13 commits landed since the morning review. The day focused on making orch's own recovery and observability more honest:

| Commit | Issue / PR | Description |
|--------|------------|-------------|
| `89cfb4f1` | #2723 | **Agent status parsing** - recognize `completed` as success; root cause of the apparent claude/opus decline. |
| `87337795` | #2722 | **GLM runner registry** - stop warning `unknown agent` on every GLM dispatch. |
| `eb1c903e` | #2721 | **Router locking** - avoid holding the skills catalog mutex across `spawn_blocking`. |
| `f787bf01` | #2720 | **Task-run hygiene** - stop writing placeholder `no error info available` errors on successful runs. |
| `fc516ef8` | #2717 | **Streaming UX** - add `orch stream --pipe` and format events in `orch events`. |
| `03a22efe` | #2716 / #2705 | **Done-without-work fix** - close three paths where tasks could be marked done without a PR or completed work. |
| `d8aace02` | #2709 | **Rebase recovery** - stop discarding `git rebase --abort` failures in stash rebase restore. |
| `d62b7ba6` | #2711 | **Commit failure observability** - log when `git restore --staged` fails after commit failure. |
| `e1adee8d` | #2710 | **Stash cleanup observability** - warn when stash restore cannot drop the stash entry. |
| `1f3f414c` | #2704 | **Control session race** - fix TOCTOU around `get_or_create_session_uuid`. |
| `c95b3aa7` | - | **Repo hygiene** - ignore `.json` artifacts. |
| `65de4b53` | #2703 | **Cooldown observability** - surface silent lock-poison recovery paths. |
| `d7139191` | #2702 / #2700 | **Stream diffing** - avoid rebroadcasting full content on same-length spinner updates. |

## Morning Plan vs Outcome

The morning review identified three priorities:

| Morning priority | Evening status |
|------------------|----------------|
| Fix CLI/service version mismatch | **Done.** `orch version` now reports CLI `0.69.25` and Service `0.69.25` in sync. |
| Investigate github-copilot failures | **Partly addressed.** `gpt-5-mini` stayed healthy, while `gpt-5.4`, `claude-sonnet-4.6`, and `gemini-3.1-pro-preview` still failed. Existing cooldown and empty-output handling are catching these instead of letting them look successful. |
| Resolve kimi billing-cycle cooldown | **Still pending.** Kimi remains in a long cooldown through `2026-04-22 09:04 UTC`; no code change today altered that. |

## Operational Health

- **Open issues:** none.
- **Active orch tasks:** only this evening retrospective (`internal:145849`) was in progress during the review.
- **Service version:** CLI and service are now in sync at `0.69.25`; this clears the Apr 14/15 recurring mismatch.
- **Task activity (12h):** 252 status changes, 84 dispatches, 58 branch deletes, 49 pushes, 40 routed events, 25 review starts, 22 review decisions, 19 PR creations, 17 error events, and 8 reroutes.
- **Closed issue throughput:** at least 19 GitHub issues closed today, including routing, task completion, stream output, stash/rebase recovery, and task-run observability fixes.

## Agent and Model Outcomes (12h)

| Agent / model | Success | Failed | Rate limit | Parse error | Notes |
|---------------|---------|--------|------------|-------------|-------|
| `claude/sonnet` | 31 | 12 | 0 | 0 | Main workhorse. Nine failures still show old placeholder error text from before #2720 landed. |
| `minimax/opus` | 17 | 1 | 4 | 0 | Strong success rate, but review runs hit repeated rate limits. |
| `opencode/minimax-m2.5-free` | 12 | 0 | 0 | 0 | Clean run window. |
| `glm/opus` | 12 | 1 | 1 | 0 | Useful reviewer/worker after the runner-registry warning fix. |
| `opencode/gpt-5-mini` | 8 | 0 | 0 | 0 | Still the reliable github-copilot-backed model. |
| `codex/gpt-5.3-codex` | 6 | 0 | 0 | 0 | Successful on medium tasks. |
| `opencode/nemotron-3-super-free` | 6 | 1 | 1 | 5 | Main parse-error source today. |
| `claude/haiku` | 2 | 2 | 0 | 0 | Mixed simple-task performance. |
| `opencode/gpt-5.4` | 0 | 2 | 0 | 0 | Both failures were exit-0 empty output, now classified as failures. |
| `opencode/claude-sonnet-4.6` | 0 | 2 | 0 | 0 | One silence-detection reroute and one empty-output failure. |
| `opencode/gemini-3.1-pro-preview` | 0 | 1 | 0 | 0 | Still not trustworthy for routing. |

The raw task-run window was 94 successes, 23 failures, 6 rate limits, and 5 parse errors. That is noisy, but several of the apparent failures are stale signatures from bugs fixed later in the day:

- `no error info available` on failed rows remains a visibility gap for historical runs, but successful-run pollution was fixed by #2720.
- The claude/opus decline was not a real model capability drop; the parser failed to treat `completed` as success, fixed by #2723.
- GLM's `unknown agent` warning was a runner-registry mismatch, fixed by #2722.
- Copilot empty-output failures are now explicit failures rather than silent success paths.

## Routing Accuracy

Routing looked directionally correct. Medium reliability work mostly went to `claude/sonnet`, `codex/gpt-5.3-codex`, `glm/opus`, `minimax/opus`, and the healthier opencode models. The bad outcomes clustered around known weak model/provider combinations rather than obviously wrong complexity classification.

The more important routing lesson was observability: bad status normalization made a healthy model look bad, and placeholder task-run errors obscured why runs failed. Today's fixes improve the signal the router uses for future decisions.

## What Went Well

- The system closed a broad reliability batch without leaving open GitHub issues.
- The version mismatch was finally cleared; the running service now includes the recent fixes.
- Several problems discovered in earlier retrospectives moved from symptoms to root causes: task completion without work, false opus decline, placeholder errors, GLM registry mismatch, and stream duplicate output.
- The orch skill's operational guidance remains aligned with today's workflow: inspect `task_runs`, trust cooldowns before manual intervention, and avoid manually retrying or resetting tasks unless explicitly requested.

## What Failed or Needed Retries

- `opencode/nemotron-3-super-free` produced 5 parse errors in 12 hours, including failed review parsing.
- `opencode/gpt-5.4` and `opencode/claude-sonnet-4.6` still produced empty-output or silence failures.
- `minimax/opus` hit 4 rate limits, mostly in review runs.
- One codex run attempted `gpt-5.2-codex` and failed with model-unavailable; successful codex work used `gpt-5.3-codex`.
- Kimi remains unavailable due to billing-cycle cooldown; that is operationally unresolved.

## Priorities for Tomorrow

1. **Verify the post-fix health signals.** Re-check whether #2720 and #2723 remove placeholder-error noise and restore accurate claude/opus success accounting.
2. **Keep gpt-5-mini preferred among github-copilot models.** The other copilot models still produce empty outputs or no-code failures; do not interpret their failures as routing bugs unless the generic cooldown path misses them.
3. **Watch nemotron review parsing.** If parse errors persist after today's parser/runner fixes, inspect raw outputs and file a root-cause bug.
4. **Resolve kimi billing status manually if needed.** The cooldown remains long enough that automation will keep routing around it.
5. **Monitor stream changes in real use.** `orch stream --pipe` and same-length diffing should reduce duplicate output; tomorrow's review should confirm no regressions in streaming.

## Issues Created

No new issues were created. The open issue list is empty, and every problem found during this review is either already fixed in today's commits or covered by the existing generic cooldown/error-classification paths.

---

Prepared by Orch automation (internal task internal:145849).

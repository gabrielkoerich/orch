+++
title = "Weekly review 2026-11-21"
date = 2026-11-21
+++

## What shipped

39 commits landed on `main` in 7 days, about 25 fixes plus version bumps and refactors. 24 issues closed. Nearly all of them are bugs the daily review found.

Scheduler and jobs:

- Bash jobs run off the engine tick, in the project directory, and a timeout kills the whole process group (#4138, #4115, #4117).
- A store error while reading a job's previous task keeps the active task, so no duplicate is created (#4137).
- A GitHub error is matched on its HTTP status, not on a `404` in the URL (#4157).

GitHub access:

- Reads time out at 10 s and use HTTP/2 keepalive, so a dead pooled connection no longer stalls the tick (#4124, #4128).
- Blocked-by lists use ETags and a 90 s TTL. An unreadable body or an abuse-detection 403 is not cached (#4134, #4146). A reopened body or CLI blocker holds its waiters again (#4130).

Review, CI and merge:

- A task routed back on red CI gets the failed check and its reason (#4123).
- A CI-failure task whose PR closes unmerged stays blocked, and the recovery sweep skips PRs blocked before review (#4129, #4141).
- Merge runs only for approved tasks, and a conflict counts once (#4149).

Safety:

- Done is terminal for `orch task route`, `orch task run`, `/block` and `/review` (#4158, #4148).
- Telegram and Discord drop messages from unlisted chats and users (#4147).
- A parsed agent response needs substantive fields, so domain JSON with `status: success` is not a finished run (#4156).
- Cleanup no longer deletes the project directory when the task branch is empty (#4114). The web UI add-project form and config editor are safer (#4113, #4116).
- Test tmux servers use a temp socket and an empty config (#4152).

## What failed

146 runs in 7 days, 142 of them ended in success.

| Agent / model | Outcome | Runs |
|---|---|---|
| grok / grok-4.6 | success | 64 |
| claude / opus | success | 54 |
| claude / sonnet | success | 21 |
| grok / grok-4.7 | success | 3 |
| claude / opus | timeout | 1 |
| claude / sonnet | push_failed | 1 |
| grok / grok-4.6 | push_failed | 1 |
| grok / grok-4.6 | blocked | 1 |
| claude / sonnet | no outcome | 1 |

Failures are isolated. No agent or model shows a repeating pattern. Last review had 12 grok credit exhaustions and 18 aborted sonnet runs. This week has none.

## Operational health

- Activity: 162 dispatches, 66 PRs created, 63 review decisions, 144 pushes. The 3 `error` events, 2 `review_agent_fallback` events, 1 `timeout` and 1 `rerouted` event match the failed runs above.
- Log: only transport errors to GitHub (5, retried), one relay reconnect, and two slow ticks of 32 s and 35 s. Both ticks ran on the pre-#4124 build. The error log is empty and last changed on Nov 17, so it carries no new signal.
- Cooldowns: `opencode` is degraded because all its models are cooled. Claude's seven day window is at 77% (`allowed_warning`), resetting in about 2 days.
- Routing: grok took most runs, in line with the review pool and configured weights. No silent model failures found.
- Review quality: review decisions (63) match PRs created (66) within a few in flight.
- SKILL.md: behavior matches the settled policy. No version drift items filed, as the policy requires.

## Stuck tasks

| Task | Status | Note |
|---|---|---|
| #4098 | blocked, 3 tries | A blocked scheduled task counts as finished, so the next cron tick creates another |
| #4022 | blocked, 3 tries | Claude agents get the system prompt file's path, not its content |
| #4037 | blocked, no-agent | Design task for the owner |
| #4044, #3936, #3730, #3720 | open | Owner or design work, not dispatched |

#4098 is the one to watch. It is a duplicate-task bug in the scheduler, the same family as the fixes in #4137 and #4157. Two agent attempts failed to land it.

## Priorities for next week

1. Land #4098 by taking it over or giving it to a stronger model. It can create duplicate scheduled tasks.
2. Resolve #4022. It is 15 days old and affects how Claude agents read their instructions.
3. Watch the opencode degraded state. If all its models stay cooled, check the cause in the cooldown reasons.
4. Confirm the slow tick count stays at zero on the new build.
5. Keep the daily review job on done-state and scheduler invariants, which produced most of this week's fixes.

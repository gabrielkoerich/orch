+++
title = "Weekly review 2026-10-31"
date = 2026-10-31
+++

## What shipped

177 commits landed on `main`, about 125 of them features and fixes. The rest are version bumps, CI changes and refactors.

The review pipeline changed most:

- A new `approved` status. An approving review moves the task to `approved`, and orch waits for CI and merges with no agent (#3911, #3915). A behind branch is rebased by orch and the approval carries over.
- A review starts only after CI ran on the PR head (#3912, #3933, #3947). With no CI result, orch runs `shipcheck` first. A required check that has not reported holds the review.
- App review mode with real reviews, inline comments and an `orch/review` check (#3896). Only reviews use the GitHub App, every other call uses the gh token (#3918). A relay client wakes the sync from App events (#3890, #3940).
- Review limits: 15 minutes per review, reset of reviews left without a session on startup (#3899), one periodic owner for orphaned reviews (#3903), a cap on resets that end without a decision (#3909), and decisions found in the output after a timeout are kept (#3910).
- Approval is inferred only from explicit decision tokens (#3869). An approve with failing tests no longer merges (#3868).

Merge and CI: merge only when GitHub reports the PR clean (#3846), read required checks from rulesets (#3841), merge an unstable PR when every required check passed (#3866), count a merged PR as a satisfied blocker (#3898).

Sandbox: `workflow.sandbox: os` runs task and review agents in the OS sandbox (#3822), with six follow-up hardening fixes (#3823, #3830 to #3835).

Routing and cooldowns: one-off model failures cool for 5 then 15 minutes (#3907). A review timeout no longer cools the review model (#3902). Failed runs and timeouts count as rate limits (#3878). An empty model is dropped before the agent CLI (#3923).

Other: SQLite write lock taken at `BEGIN` with retry (#3908), sqlx 0.9 with bundled SQLite 3.54.1 (#3941), `orch service install` creates a systemd user unit (#3891), multi node start (#3808), secrets take `passbox:<name>` or `${NAME}` (#3937), secrets redacted at the GitHub posting boundary (#3870), pinned CI actions (#3817).

About 75 issues closed since the last review. The newest are #3943 to #3949: review timeout stored as a parse error, review duration always 0, the review CI gate comparing billing-failed runs against filtered checks, and CI timers keyed by task id only.

## What failed

The DB holds many more runs this week than last week because orch also ran review and job work on many tasks. Totals by outcome, all agents:

| Agent / model | Outcome | Runs |
|---|---|---|
| claude / sonnet | success | 423 |
| claude / opus | success | 178 |
| claude / claude-opus-5 | success | 113 |
| grok / grok-4.6 | success | 106 |
| kimi / opus | success | 25 |
| grok / grok-4.7 | success | 9 |
| claude / sonnet | failed | 16 |
| claude / opus | timeout | 11 |
| grok / grok-4.6 | credit_exhausted | 10 |
| kimi / opus | rate_limit | 10 |
| minimax | credit_exhausted | 8 |
| claude / claude-opus-5 | parse_error | 7 |
| claude / claude-opus-5 | aborted | 6 |
| claude / claude-opus-5 | incomplete | 5 |

The tail is spread over aborted, timeout, parse_error and failed runs for grok-4.6, grok-4.7, codex gpt-5.4 and gpt-5.5 (all failed or timed out), and kimi.

Why runs failed:

- Review timeouts. Reviews that ran a local CI suite hit the review limit every attempt (#3943, closed by #3947), and a timed out review was stored as a parse error that cooled the review model (#3942, closed by #3946). Both are fixed. The remaining `claude-opus-5` parse errors and timeouts are mostly from before those fixes.
- Credit and usage caps: grok-4.6 and minimax out of credits, kimi at its 5 hour cap.
- Codex `gpt-5.4` and `gpt-5.5` fail or time out and sit in persistent model cooldown.
- Four grok-4.7 parse errors and timeouts remain. The stored `invalid response` string still has no payload.

## Operational health

`task_activity` in the window: 4697 status changes, 1042 dispatches, 888 pushes, 618 branch deletes, 471 review starts, 438 review decisions, 438 PR creations, 437 routes, 297 review agent fallbacks, 180 failed review attempts, 164 errors, 67 reroutes, 27 timeouts, 3 rebases of approved branches.

The review fallback count (297 against 471 starts) is high. It matches the review timeouts above and should drop now that a review no longer runs a long CI suite.

`orch.error.log` holds three `git` pathspec errors for `.orch.yml`. Its mtime is about 11 hours before this review, and it is truncated on restart, so nothing is filed from it.

The last 1000 log lines hold two repeating warnings. `degraded mode: using sequential dispatch healthy_agents=1 threshold=2` shows only one healthy agent is available. The router also logs `all agents/models cooled` for one task for about 80 seconds, then routes it. Both match the settled policy: a loud log every tick and a retry on the next tick.

`orch cooldown list` shows no active cooldowns at review time. The router agent weights are `{claude: 1.0}`.

Current behavior matches `SKILL.md` and the settled repo policy: generic cooldowns, per-task billing block at merge time, retry next tick when every agent is cooled, `done` is terminal.

## Stuck tasks

| ID | Status | Why |
|---|---|---|
| 3936 | new | Design issue with the `no-agent` label, waits for the operator |

`orch task list` shows no blocked or failing tasks. Open issues are all design work: #3936 (public GitHub App), #3730 and #3720 (named profiles).

## Routing accuracy

Claude does most of the work: sonnet, opus and claude-opus-5 account for about 720 successes. Grok-4.6 had 106 successes, so its first-week problems (empty errors, timeouts, 402s) were fixed or contained. Kimi has 25 successes with 10 rate limits. Codex and minimax record no successes.

With claude as the only healthy agent, the router runs in sequential dispatch. This is a capacity issue, not a routing bug. No model failed silently. Unavailable models and credit exhaustion went through the generic cooldown. Prompts do not need a change.

## Priorities for next week

1. Watch the review fallback and failed review attempt counts after #3946 and #3947. They should fall well below 297 and 180.
2. Restore a second healthy agent (grok or kimi credits, minimax credits). Dispatch stays sequential with one.
3. Watch the new `approved` flow: rebases of approved branches (#3915), merge failures that send a task back to `routed`.
4. Operator decisions on #3936, #3720 and #3730.

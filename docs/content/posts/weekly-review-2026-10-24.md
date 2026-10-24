+++
title = "Weekly review 2026-10-24"
date = 2026-10-24
+++

## What shipped

19 commits landed on `main`.

Grok joined the agent pool (#3708). Task worktrees clone through the git-cow crate (#3706).

Runner and classifier:

- Read `errors[]` on error envelopes so credit exhaustion classifies (#3710, #3714).
- Clear the cached GitHub token and retry once on 401 (#3719). Block the task when PR creation fails with a GitHub auth error (#3718).
- Block tasks that run out of attempts, and keep those tests off the GitHub API (#3715).
- Success envelopes without `terminal_reason` become parse errors (#3726).
- Salvage grok runs killed at the timeout after they finished wrap-up (#3736).
- Treat a kimi review that exits mid-thought as incomplete (#3737).

Release and CI: publish as `orch-cli` on crates.io and run with systemd on Linux (#3711), Linux x86_64 via cargo binstall (#3712), sign and notarize the macOS binary (#3731, #3733). Agents use shipcheck when a repository has it (#3724). Board labels reconcile into GitHub Project Status (#3723). Telegram messages over the length limit split at line ends.

Closed this week: #3709, #3713, #3716, #3717, #3722, #3725, #3734, #3735.

## What failed

139 runs in the last 7 days. 106 succeeded.

| Agent / model | Outcome | Runs |
|---|---|---|
| claude / sonnet | success | 58 |
| kimi / opus | success | 29 |
| grok / grok-4.6 | success | 12 |
| grok / grok-4.7 | success | 7 |
| claude / sonnet | failed | 7 |
| grok / grok-4.6 | failed | 4 |
| kimi / opus | rate_limit | 4 |
| claude / sonnet | blocked | 3 |
| grok / grok-4.6 | credit_exhausted | 2 |
| grok / grok-4.6 | parse_error | 2 |
| grok / grok-4.7 | timeout | 2 |

The remaining eight non-success runs are one each of grok-4.6 timeout, grok-4.7 parse_error, kimi failed, kimi parse_error, minimax credit_exhausted, minimax failed, codex gpt-5.4 unavailable, and codex gpt-5.5 unavailable.

Grok's first week: four empty `grok failed:` errors on 20 Oct, three `invalid response` parse errors, three 1800s timeouts, two Grok Build 402s. The timeouts and the kimi review parse error were filed and closed today (#3735, #3734). The stored `invalid response` string has no payload, so those three cannot be told apart from the DB.

Claude's seven failures were silence detection, max attempts, a passbox Touch ID timeout on an unattended scheduled job, and a review agent that hit GitHub 401 three times (fixed in #3717). The three blocked sonnet runs are that same scheduled import: passbox could not unlock credentials without Touch ID, then a later attempt found no new work.

Kimi hit the 5-hour usage cap four times. Codex `gpt-5.4` and `gpt-5.5` are unavailable. Minimax is out of credits.

## Operational health

50 tasks reached `done`. `task_activity` in the window: 515 status changes, 204 dispatches, 62 PRs, 36 review starts, 33 errors, 22 reroutes, 4 review-agent fallbacks, 3 timeouts.

Open issues are design work: #3720 named profiles, #3727 cluster (blocked on draft PR #3729 for operator review), #3728 third-vote decision (`no-agent`), #3730 named-profile runtime acceptance (`no-agent`). External PRs still open: #3721 (named-profile offline foundation), #3732 (shipcheck CI).

Cooldowns now: `codex:gpt-5.4` 4d9h persistent_model_error, `minimax` 12h credit_exhaustion, `minimax:haiku` 1d model_error.

`orch.error.log` is empty (mtime today). The last 1000 log lines are INFO sync ticks, typically 1.6s to 3.7s, a few near 10s. Host suspend/resume gaps are discounted from stuck-task age.

Current behavior matches `prompts/skills/orch/SKILL.md`: generic cooldowns, per-task billing block at merge time, retry next tick when every agent is cooled.

## Stuck tasks

| ID | Status | Why |
|---|---|---|
| 3727 | blocked | Held for operator review of draft PR #3729 (push lease after update-branch) |
| internal scheduled import | blocked | Passbox needs Touch ID in an unattended run. Four attempts. |
| internal scheduled stats | blocked | Review agent exceeded failure threshold on GitHub 401. Root cause closed in #3717. Operator may unblock after confirming the token path is clean. |
| long-lived downstream task | blocked | GitHub Actions billing failure at merge time. Expected per-task block. |

## Routing accuracy

Claude sonnet still does most of the work (58 successes from 68 runs). Kimi opus 29 of 35. Grok, new this week, 19 of 32; failed runs rerouted and those tasks finished. Codex and minimax recorded no successes.

No silent failures. Unavailable models and credit exhaustion took the generic cooldown path. Prompts do not need a change this week. The parser bugs that did fire were envelope shape, timeout salvage, and mid-thought reviews, all closed.

## Priorities for next week

1. Watch grok after #3736 and kimi reviews after #3737. If `grok invalid response` keeps showing up, the next step is to keep a truncated payload on `InvalidResponse` so the parser can be diagnosed.
2. Confirm the 401 retry in #3719 holds. The leftover blocked review task is an operator choice after that.
3. Leave `codex:gpt-5.4` on persistent cooldown until the model exists again. Leave minimax on the credit cooldown.

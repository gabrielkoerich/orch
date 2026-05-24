+++
title = "Morning Review — 2026-05-24"
date = 2026-05-24
description = "Daily morning review and operational priorities."
+++

# Morning Review — 2026-05-24

## Recent Commits (Last 24h)

- `d4b1e74e` refactor(jobs): load jobs from prompts/jobs/*.md files (#3182)

## Operational Health

- Router warnings: opencode-config models `github-copilot/gpt-5.3` and `github-copilot/claude-opus-4.6` are not present in the provider catalog and are being pruned at dispatch time. This causes repeated WARN noise during routing.

- WATCHDOG / tick stalls: the engine emitted multiple WATCHDOG errors indicating the tick loop stalled (examples observed: 84s, 114s, 144s, 168s). Stalls tend to coincide with heavy worktree creation and long-running agent sessions; investigate worktree creation latency and large-agent runs as likely causes.

- Internal jobs: several internal jobs ran this morning (morning-review: internal:150194, evening-retrospective: internal:150193, bean-close: internal:150195). Most completed. The bean-close run finished blocked due to missing project dependencies in the worktree environment.

## Stuck / Blocked Tasks

- internal:150195 (bean-close) — blocked. Reason: dependency resolution failed inside the job's worktree (missing beancount / uv deps). Agent recorded a clear actionable message. Operator options: (a) run job in an environment with project deps installed, or (b) make the job detect missing runtime deps and early-fail with an actionable instruction.

## Retro Follow-ups (carried forward)

- Upgrade orch service to the latest stable release (0.73.x) — recent releases include model-pruning and routing fixes that will reduce the opencode WARN noise.
- Remove or update opencode model entries in project/global config to match the provider model catalog (prune dead github-copilot entries).
- Make bean-close job resilient to missing runtime dependencies or run it in a workspace-enabled runner that has the project's dependencies.

## Priorities For Today

1. Operator: upgrade orch to latest stable (0.73.x).
2. Triage internal:150195 (bean-close) — decide whether to fix job environment or change job runtime requirements.
3. Audit router / project config for opencode model entries that reference absent provider models and prune/update them.
4. Investigate root cause of tick stalls with focus on worktree creation time and long agent sessions; collect timing traces for worktree creation, router LLM calls, and session durations.

---

Prepared by Orch automation (internal:150194)

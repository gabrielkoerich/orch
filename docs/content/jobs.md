+++
title = "Scheduled Jobs"
description = "Cron-based scheduled tasks and bash jobs"
weight = 9
+++

Define recurring work with cron expressions. Jobs create tasks on schedule (or run bash commands directly).

## Commands

```bash
orch job list                                                # list scheduled jobs
orch job add "0 9 * * *" "Daily report" -b "what to do"      # task job (positional: schedule, title)
orch job add "@hourly" "Ping" -t bash -c "./scripts/ping.sh" # bash job
orch job enable <id>
orch job disable <id>
orch job remove <id>
orch job tick                                                # run one scheduler tick
orch job run <id> [-p repo-slug]                             # run a job immediately, ignoring schedule
```

## Job Types

| Type | Description |
|------|-------------|
| **task** (default) | Creates a task that goes through routing → agent execution |
| **bash** | Runs a shell command directly, no LLM involved |

## How It Works

1. Jobs are loaded from **two sources**:
   - Per-project `.orch.yml` under the `jobs:` key
   - Per-project `prompts/jobs/*.md` files with frontmatter (`id`, `schedule`, `title`, `type`, `enabled`, `external`, `agent`, `command`, `dir`)
2. The engine's scheduler checks cron schedules on each tick (tick interval configurable via `engine.tick_interval`).
3. When a schedule matches, a task is created (or command is run)
4. Jobs skip if a previous task from the same job is still in-flight (`active_task_id`)
5. Job-created tasks get `scheduled` and `job:{id}` labels

## Cron Expressions

Standard cron syntax with aliases:

```
┌───────── minute (0-59)
│ ┌─────── hour (0-23)
│ │ ┌───── day of month (1-31)
│ │ │ ┌─── month (1-12)
│ │ │ │ ┌─ day of week (0-6, Sun=0)
│ │ │ │ │
* * * * *
```

Aliases: `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`

## Example

```bash
# Run code quality check every weekday morning
orch job add "0 9 * * 1-5" "Code quality review" -b "Run lints, check for TODO items, update docs"

# Database backup every night (bash, no agent)
orch job add "0 2 * * *" "DB backup" -t bash -c "pg_dump mydb > /backups/nightly.sql"
```

Jobs integrate with GitHub sync — job-created tasks get synced to GitHub issues like any other task.

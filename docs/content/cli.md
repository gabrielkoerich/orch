+++
title = "CLI Reference"
description = "Commands, namespaces, and background service"
weight = 6
+++

## Namespaces

```bash
orch task     list|add|get|status|route|run|retry|reroute|unblock|attach|live|kill
              publish|cost|tree|logs|log|history|runs|close|watch|reopen|inspect
orch service  start|stop|restart|status
orch job      list|add|remove|enable|disable|tick|run
orch project  list|add|remove
orch board    list|link|sync|info
orch cooldown list|clear
orch session  export
orch webhook  status
orch chat     [message…] | history | stats
```

Top-level commands: `serve`, `init`, `log`, `agents`, `version`, `completions`, `parse`, `commit`, `cron`, `config`, `template`, `stream`, `metrics`, `dashboard`, `cost`, `stats`, `events`, `doctor`, `prune`, `notify`.

## Task Commands

```bash
orch task add "title" -b "body" -l label1 -l label2   # create a task
orch task list                                         # list tasks for current project
orch task list -g                                      # list across all projects
orch task tree [id]                                    # show parent-child tree
orch task get <id>                                     # task details
orch task status                                       # status summary for current project
orch task route <id>                                   # (re-)route a task
orch task run [id]                                     # run a specific task (or next routed)
orch task retry <id> [--agent X --model Y]             # reset to new, optionally forcing routing
orch task reroute <id> --agent X --model Y             # force a new agent/model
orch task unblock <id> | all                           # reset blocked tasks to new
orch task attach <id>                                  # attach to the agent's tmux session
orch task live                                         # list active agent tmux sessions
orch task kill <id>                                    # kill a running agent tmux session
orch task publish <id> [-l label1]                     # promote internal task to GitHub
orch task cost <id>                                    # token cost breakdown
orch task logs <id>                                    # post-mortem / saved logs for a task
orch task log <id> [--limit N] [--json]                # activity timeline
orch task history <id>                                 # routing history / attempts
orch task runs <id> [--verbose]                        # full run audit (stdout/stderr/response)
orch task close <id> [-n "note"]                       # mark done without running an agent
orch task watch <id>                                   # follow status changes live
orch task reopen <id>                                  # reopen a done/blocked task
orch task inspect <id>                                 # unified diagnostic view
orch stream [id]                                       # stream live output (all sessions if no id)
```

Note: `orch task add` does not take a `-p`/project flag — task creation always targets the current project (resolved from CWD or the `.orch.yml` config). To create tasks for another project, `cd` into it first, or use `orch task publish` to promote an internal task to GitHub.

## Background Service

```bash
orch service start      # start background service
orch service stop       # stop background service
orch service restart    # restart service
orch service status     # show service status
```

With Homebrew:
```bash
brew services start orch    # start as launchd service
brew services stop orch
brew services restart orch
```

The service ticks every `engine.tick_interval` seconds (default 10s):
- Syncs GitHub issues and PR events (webhook or polling)
- Routes new tasks to agents via LLM router
- Dispatches routed tasks into tmux sessions in worktrees
- Runs due scheduled jobs (per-project)
- Recovers stuck tasks (no tmux session, age >10 min → reset to `new`)

## Dashboard & Status

```bash
orch dashboard              # full dashboard view (tasks, sessions, activity)
orch dashboard -g           # aggregate across all configured projects
orch task status            # show task counts for current project
orch metrics [--details]    # task metrics summary (slow tasks, error dist.)
orch cost [--summary]       # cost tracking and token usage
orch stats [--all]          # task throughput / per-project rollups
orch events [--repo X]      # tail task events live
orch log [N]                # tail server logs (last N lines or "watch")
orch doctor [--fix]         # diagnose SQLite ↔ GitHub drift
orch prune [--dry-run]      # remove orphaned worktrees
orch cooldown list          # show active agent/model cooldowns
orch cooldown clear <key>   # clear a specific cooldown (e.g. claude, claude:sonnet)
orch cooldown clear --all   # clear every active cooldown
orch webhook status         # show webhook server health
orch session export <id>    # export a task's session in markdown/json/raw
orch notify "message"       # send a Telegram notification (uses config target by default)
orch config <key>           # read a live config value (dot-separated path)
```

## Channels

Channels are first-class: use chat platforms to interact with running tasks. Incoming messages in a bound thread are forwarded to the agent's tmux session (via `tmux send-keys`). New conversations can create internal tasks when sent to any configured channel. Live output from sessions is streamed to all bound channel threads with per-channel rate limiting and message splitting.

## Project Commands

```bash
orch project add .                         # register the current directory
orch project add owner/repo                # bare clone + import issues
orch project add https://github.com/o/r    # same, from a URL
orch project add /path/to/repo             # register a local path
orch project list                          # list registered projects
orch project remove /path/to/repo          # remove a project from registry
```

## Job Commands

```bash
orch job list                                              # list scheduled jobs
orch job add "0 9 * * *" "Daily report" -b "body"          # cron + title (positional)
orch job add "0 9 * * *" "Backup" -t bash -c "./backup.sh" # bash job
orch job remove <id>                                       # remove a job
orch job enable <id>                                       # enable a job
orch job disable <id>                                      # disable a job
orch job tick                                              # run one scheduler tick manually
orch job run <id>                                          # run a job immediately, ignoring schedule
```

Jobs can also be defined declaratively as markdown files under `prompts/jobs/*.md` (frontmatter: `id`, `schedule`, `title`, `type`, `enabled`, `external`, `agent`, `command`, `dir`). See [Jobs](@/jobs.md).

## Version

```bash
orch version    # show CLI version and service version (warns on mismatch)
```

Shows the CLI version and, if the service is running, the version the service was built from. Warns when they differ so you can detect CLI/service drift:

```
CLI:     0.12.3
Service: 0.12.3  ✓ in sync
```

When out of sync, run `brew upgrade orch && brew services restart orch`.

## Chat

```bash
orch chat                           # interactive REPL
orch chat "what's running?"         # single message
orch chat --session ops             # use a named session profile
orch chat history                   # show recent messages
orch chat history --search "bean"   # search past conversations
```

Conversational control plane — talk to orch in natural language. Each message invokes a one-shot agent with context assembled from the live task state, memories, and recent summaries. Switch the model with `/model agent:model` inside the REPL.

## Agent Management

```bash
orch agents                 # list available agents and their status
```

## Logging

| Log | Location |
|-----|----------|
| Server log | `~/.orch/state/orch.log` |
| Server archive | `~/.orch/state/orch.archive.log` |
| Task database | `~/.orch/orch.db` (SQLite — all task state) |
| Per-task output | `~/.orch/state/<repo>/tasks/<id>/attempts/<n>/output.json` |
| Per-task prompts | `~/.orch/state/<repo>/tasks/<id>/attempts/<n>/prompt-sys.md` |
| Brew stdout | `/opt/homebrew/var/log/orch.log` |
| Brew stderr | `/opt/homebrew/var/log/orch.error.log` |

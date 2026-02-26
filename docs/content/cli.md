+++
title = "CLI Reference"
description = "Commands, namespaces, and background service"
weight = 6
+++

## Namespaces

```bash
orch task list|tree|add|plan|route|run|next|poll|retry|unblock|agent|stream|watch|unlock
orch service start|stop|restart|info|install|uninstall
orch gh pull|push|sync
orch project info|create|list|add
orch job add|list|remove|enable|disable|tick
orch skills list|sync
```

Top-level shortcuts: `init`, `chat`, `status`, `dashboard`, `log`, `start`, `stop`, `restart`, `info`, `agents`, `version`.

`orch` is a short alias for `orchestrator`.

## Task Commands

```bash
orch task add "title" "body" "labels"  # create a task
orch task add "title" -p owner/repo   # create a task for a managed project
orch task plan "title" "body"          # create a decompose task
orch task list                          # list tasks for current project
orch task tree                          # show parent-child tree
orch task route <id>                    # route a task to an agent
orch task run <id>                      # run a specific task
orch task next                          # route + run next pending task
orch task poll                          # process all pending tasks
orch task retry <id>                    # reset task to new
orch task unblock <id>                  # reset blocked task to new
orch task unblock all                   # reset all blocked tasks
orch task agent <id> <agent>            # manually set task agent
orch task watch                         # watch task status changes
orch task unlock                        # clear stale locks
```

## Background Service

```bash
orch start                # start background server
orch stop                 # stop background server
orch restart              # restart server
orch info                 # show server status (PID, uptime)
```

With Homebrew:
```bash
brew services start orchestrator    # start as launchd service
brew services stop orchestrator
brew services restart orchestrator
```

The server runs `serve.sh` which ticks every 10 seconds:
- Polls for new/routed tasks
- Checks for stuck tasks
- Runs due scheduled jobs
- Syncs with GitHub (every 60s)

Install as a launchd service (auto-starts on login):
```bash
orch service install      # create launchd plist
orch service uninstall    # remove launchd plist
```

## Chat Mode

```bash
orch chat
```

Interactive mode with readline support. Talk to the orchestrator, add tasks, check status. Chat tasks run in the current `PROJECT_DIR` without worktrees.

## Dashboard & Status

```bash
orch status                 # show task counts for current project
orch status --global        # show all projects
orch status --json          # JSON output
orch dashboard              # full dashboard view
orch log                    # tail server logs
```

## GitHub Commands

```bash
orch gh pull                # import issues into tasks
orch gh push                # push task updates to issues
orch gh sync                # both directions
orch project add owner/repo  # bare clone + import issues
orch project info            # show GitHub Project field IDs
orch project info --fix      # auto-fill project config
orch project create "name"   # create or link a GitHub Project v2
orch project list             # list managed bare-clone projects
orch project list org=ORG     # list GitHub Projects v2 + managed projects
orch project list user=USER   # list GitHub Projects v2 + managed projects
```

## Agent Management

```bash
orch agents                 # list available agents and their status
```

## Logging

| Log | Location |
|-----|----------|
| Server log | `~/.orchestrator/state/orchestrator.log` |
| Server archive | `~/.orchestrator/state/orchestrator.archive.log` |
| Jobs log | `~/.orchestrator/state/jobs.log` |
| Per-task output | `~/.orchestrator/state/output-{id}.json` |
| Per-task tools | `~/.orchestrator/state/tools-{id}.json` |
| Per-task prompts | `~/.orchestrator/state/prompt-{id}.md` |
| Task context | `~/.orchestrator/contexts/task-{id}.md` |
| Brew stdout | `/opt/homebrew/var/log/orchestrator.log` |
| Brew stderr | `/opt/homebrew/var/log/orchestrator.error.log` |

+++
title = "CLI Reference"
description = "Commands, namespaces, and background service"
weight = 6
+++

## Namespaces

```bash
orch task list|add|get|status|route|run|retry|unblock|attach|live|kill|publish|cost|tree
orch service start|stop|restart|status
orch job add|list|remove|enable|disable|tick
orch project add|remove|list
orch stream <task_id>
orch dashboard
orch metrics
orch cost
```

Top-level shortcuts: `serve`, `init`, `log`, `chat`, `agents`, `version`, `completions`.

## Task Commands

```bash
orch task add "title" --body "body" --labels "comma,separated"  # create a task
orch task add "title" -p owner/repo   # create a task for a managed project
orch task list                          # list tasks for current project
orch task tree                          # show parent-child tree
orch task get <id>                      # get task details
orch task status                        # show task status summary
orch task route <id>                    # route a task to an agent
orch task run <id>                      # run a specific task
orch task retry <id>                    # reset task to new
orch task unblock <id>                  # reset a needs_review/stalled task to new
orch task unblock all                   # reset all needs_review/blocked tasks
orch task attach <id>                   # attach to a running agent's tmux session
orch task live                          # list active agent tmux sessions
orch task kill <id>                     # kill a running agent tmux session
orch task publish <id>                  # publish internal task to GitHub
orch task cost <id>                     # show token cost breakdown for a task
orch stream <id>                        # stream live output from a running task
```

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
orch task status            # show task counts for current project
orch metrics                # show task metrics summary
orch cost                   # show cost tracking and token usage
orch log                    # tail server logs
```

## Channels

Channels are first-class: use chat platforms to interact with running tasks. Incoming messages in a bound thread are forwarded to the agent's tmux session (via `tmux send-keys`). New conversations can create internal tasks when sent to any configured channel. Live output from sessions is streamed to all bound channel threads with per-channel rate limiting and message splitting.

## Project Commands

```bash
orch project add owner/repo   # bare clone + import issues
orch project add /path/to/repo # register a local project
orch project list              # list registered projects
orch project remove owner/repo # remove a project from registry
```

## Job Commands

```bash
orch job list                             # list scheduled jobs
orch job add "title" --cron "0 9 * * *"  # add a cron job
orch job remove <id>                      # remove a job
orch job enable <id>                      # enable a job
orch job disable <id>                     # disable a job
orch job tick                             # run one job scheduler tick manually
```

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

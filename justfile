set shell := ["bash", "-c"]
set positional-arguments := true

_default:
    @just --list

# Show orchestrator version
[group('config')]
version:
    @echo "${ORCH_VERSION:-$(git describe --tags --always 2>/dev/null || echo unknown)}"

# Initialize orchestrator for current project
[group('config')]
init *args="":
    @scripts/init.sh "$@"

# Interactive chat with the orchestrator
[group('agents')]
chat:
    @scripts/chat.sh

# Overview: tasks, projects, worktrees
[group('agents')]
dashboard:
    @scripts/dashboard.sh

# Tail orchestrator logs (server + errors). Use "orchestrator log watch" for live follow.
[group('config')]
log tail="50":
    #!/usr/bin/env bash
    ORCH_HOME="${ORCH_HOME:-$HOME/.orchestrator}"
    STATE="${ORCH_HOME}/.orchestrator"
    BREW_LOG="${HOMEBREW_PREFIX:-/opt/homebrew}/var/log/orchestrator.log"
    BREW_ERR="${HOMEBREW_PREFIX:-/opt/homebrew}/var/log/orchestrator.error.log"
    FILES=()
    [ -f "$STATE/orchestrator.log" ] && FILES+=("$STATE/orchestrator.log")
    [ -f "$STATE/orchestrator.error.log" ] && FILES+=("$STATE/orchestrator.error.log")
    [ -f "$BREW_LOG" ] && [ -s "$BREW_LOG" ] && FILES+=("$BREW_LOG")
    [ -f "$BREW_ERR" ] && [ -s "$BREW_ERR" ] && FILES+=("$BREW_ERR")
    if [ ${#FILES[@]} -eq 0 ]; then
      echo "No log files found"
      exit 1
    fi
    if [ "{{ tail }}" = "watch" ]; then
      tail -f "${FILES[@]}"
    else
      for f in "${FILES[@]}"; do
        echo "=== $(basename "$f") ==="
        tail -n {{ tail }} "$f"
        echo ""
      done
    fi

#################################
# Namespace: skills (list, sync)
#################################

# Manage skills registry (list, sync)
[group('agents')]
skills target *args:
    #!/usr/bin/env bash
    just "_skills_$1" "${@:2}"

[private]
_skills_list:
    @scripts/skills_list.sh

[private]
_skills_sync:
    @scripts/skills_sync.sh

# List installed agent CLIs
[group('agents')]
agents:
    @scripts/agents.sh

########################################################################################################
# Namespace: task (list, tree, add, plan, route, run, next, poll, retry, unblock, agent, stream, rejoin, watch, unlock, review)
########################################################################################################

# Manage tasks (status, list, tree, add, plan, route, run, next, poll, retry, unblock, agent, stream, watch, unlock, review)
[group('tasks/jobs')]
task target *args:
    #!/usr/bin/env bash
    just "_task_$1" "${@:2}"

[private]
_task_status *args:
    @scripts/status.sh "$@"

[private]
_task_report *args:
    @scripts/progress_report.sh "$@"

[private]
_task_list:
    @scripts/list_tasks.sh

[private]
_task_tree:
    @scripts/tree.sh

[private]
_task_add *args:
    @scripts/add_task.sh "$@"

# Set PLAN_INTERACTIVE=0 to skip interactive planning and just add task with plan label
[private]
_task_plan title body="" labels="":
    {{ if env("PLAN_INTERACTIVE", "1") == "0" { "scripts/add_task.sh \"" + title + "\" \"" + body + "\" \"plan," + labels + "\"" } else { "scripts/plan_chat.sh \"" + title + "\" \"" + body + "\" \"" + labels + "\"" } }}

[private]
_task_route id="":
    @scripts/route_task.sh {{ id }}

[private]
_task_run id="":
    @scripts/run_task.sh {{ id }}

[private]
_task_next:
    @scripts/next.sh

[private]
_task_poll *args:
    @POLL_JOBS={{ if args == "" { "4" } else { args } }} scripts/poll.sh

[private]
_task_retry id:
    @scripts/retry_task.sh {{ id }}

[private]
_task_unblock id:
    {{ if id == "all" { "scripts/unblock_all.sh" } else { "scripts/retry_task.sh " + id } }}

[private]
_task_agent id agent:
    @scripts/set_agent.sh {{ id }} {{ agent }}

[private]
_task_stream id:
    @scripts/stream_task.sh {{ id }}

[private]
_task_rejoin *args:
    @POLL_JOBS={{ if args == "" { "4" } else { args } }} scripts/rejoin.sh

[private]
_task_watch *args:
    @scripts/watch.sh {{ if args == "" { "10" } else { args } }}

[private]
_task_unlock:
    @scripts/unlock.sh

[private]
_task_review:
    @scripts/review_prs.sh

# Attach to a running agent's tmux session
[private]
_task_attach id:
    @scripts/task_attach.sh {{ id }}

# List active agent tmux sessions
[private]
_task_live:
    @scripts/task_live.sh

# Kill a running agent tmux session
[private]
_task_kill id:
    @scripts/task_kill.sh {{ id }}

#####################################################################
# Namespace: service (start, stop, restart, info, install, uninstall)
#####################################################################

# Manage the orchestrator service (start, stop, restart, info, install, uninstall)
[group('service')]
service target *args:
    #!/usr/bin/env bash
    just "_service_$1" "${@:2}"

[private]
_service_start interval="10":
    #!/usr/bin/env bash
    if [ "${ORCH_BREW:-}" = "1" ]; then
      brew services start orchestrator
    else
      INTERVAL={{ interval }} TAIL_LOG=1 exec scripts/serve.sh
    fi

# Run server directly (used by brew services internally)
[private]
_service_serve interval="10":
    @INTERVAL={{ interval }} scripts/serve.sh

[private]
_service_stop:
    #!/usr/bin/env bash
    if [ "${ORCH_BREW:-}" = "1" ]; then
      brew services stop orchestrator
    else
      scripts/stop.sh
    fi

[private]
_service_restart:
    #!/usr/bin/env bash
    if [ "${ORCH_BREW:-}" = "1" ]; then
      brew services restart orchestrator
    else
      scripts/restart.sh
    fi

[private]
_service_info:
    #!/usr/bin/env bash
    if [ "${ORCH_BREW:-}" = "1" ]; then
      brew services info orchestrator
    else
      PID_FILE="${STATE_DIR:-.orchestrator}/orchestrator.pid"
      if [ -f "$PID_FILE" ]; then
        PID=$(cat "$PID_FILE")
        if kill -0 "$PID" 2>/dev/null; then
          echo "Orchestrator running (pid $PID)"
        else
          echo "Orchestrator not running (stale pid $PID)"
        fi
      else
        echo "Orchestrator not running (no pid file)"
      fi
    fi

[private]
_service_killall:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "Killing all orchestrator processes..."
    for pattern in 'scripts/serve\.sh' 'scripts/poll\.sh' 'scripts/run_task\.sh' 'scripts/cleanup_worktrees\.sh' 'scripts/route_task\.sh' 'scripts/jobs_tick\.sh'; do
      pkill -f "$pattern" 2>/dev/null && echo "  killed $pattern" || true
    done
    # Clean up stale PID/lock files
    ORCH_HOME="${ORCH_HOME:-$HOME/.orchestrator}"
    STATE="${ORCH_HOME}/.orchestrator"
    rm -f "$STATE/orchestrator.pid"
    rm -rf "$STATE/serve.lock"
    echo "Done."

# DEPRECATED
[private]
_service_install:
    @scripts/service_install.sh

# DEPRECATED
[private]
_service_uninstall:
    @scripts/service_uninstall.sh

##################################
# Namespace: gh (pull, push, sync)
##################################

# GitHub sync (pull, push, sync)
[group('github')]
gh target *args:
    #!/usr/bin/env bash
    just "_gh_$1" "${@:2}"

[private]
_gh_pull:
    @echo "GitHub is the native backend — no pull needed."

[private]
_gh_push:
    @echo "GitHub is the native backend — no push needed."

[private]
_gh_sync:
    @echo "GitHub is the native backend — no sync needed."

#########################################
# Namespace: project (info, create, list)
#########################################

# GitHub Projects V2 (info, create, list)
[group('github')]
project target *args:
    #!/usr/bin/env bash
    just "_project_$1" "${@:2}"

[private]
_project_info *args:
    @scripts/gh_project_info.sh "$@"

[private]
_project_create title="":
    @scripts/gh_project_create.sh "{{ title }}"

[private]
_project_add *args:
    @scripts/project_add.sh "$@"

[private]
_project_list org="" user="":
    @scripts/gh_project_list.sh "{{ org }}" "{{ user }}"

###########################################################
# Namespace: job (add, list, remove, enable, disable, tick)
###########################################################

# Manage scheduled jobs (add, list, remove, enable, disable, tick)
[group('tasks/jobs')]
job target *args:
    #!/usr/bin/env bash
    just "_job_$1" "${@:2}"

[private]
_job_add *args:
    @scripts/jobs_add.sh "$@"

[private]
_job_info id:
    @scripts/jobs_info.sh "{{ id }}"

[private]
_job_list:
    @scripts/jobs_list.sh

[private]
_job_remove id:
    @scripts/jobs_remove.sh "{{ id }}"

[private]
_job_enable id:
    @scripts/jobs_enable.sh "{{ id }}"

[private]
_job_disable id:
    @scripts/jobs_disable.sh "{{ id }}"

[private]
_job_tick:
    @scripts/jobs_tick.sh

############################
#  Brew services commands
#############################

# Kill all orchestrator processes (including orphans from crashed/upgraded instances)
[group('service')]
killall:
    @scripts/stop.sh --force

# Stop orchestrator service (via brew)
[group('service')]
stop:
    @just service stop

# Start orchestrator service (via brew)
[group('service')]
start:
    @just service start

# Restart orchestrator service (via brew)
[group('service')]
restart:
    @just service restart

# Show orchestrator service status
[group('service')]
status:
    @just service info

[private]
info:
    @just service info

[private]
serve interval="10":
    @just _service_serve {{ interval }}

# Run tests (bats test suite)
[private]
test:
    @bats tests

# Release: commit, push, watch CI, brew upgrade, restart
[private]
release *msg:
    #!/usr/bin/env bash
    set -euo pipefail
    # Check for changes
    if git diff --quiet && git diff --cached --quiet && [ -z "$(git ls-files --others --exclude-standard)" ]; then
      echo "Nothing to commit"
      exit 1
    fi
    # Commit
    if [ -n "{{ msg }}" ]; then
      git add -A && git commit -m "{{ msg }}"
    else
      echo "Usage: orchestrator release \"commit message\""
      exit 1
    fi
    # Push
    echo "==> Pushing to origin..."
    git push
    # Wait for CI
    echo "==> Waiting for CI..."
    sleep 3
    RUN_ID=$(gh run list --limit 1 --json databaseId -q '.[0].databaseId')
    gh run watch "$RUN_ID" --exit-status || { echo "CI failed!"; exit 1; }
    # Brew upgrade
    echo "==> Upgrading brew..."
    brew update --quiet
    brew upgrade orchestrator
    # Restart
    echo "==> Restarting orchestrator..."
    orchestrator stop
    sleep 1
    orchestrator start
    echo "==> Done! $(orchestrator version)"

############################
# Namespace: docs (build, serve)
############################

# Documentation site (build, serve)
[group('docs')]
docs target *args:
    #!/usr/bin/env bash
    just "_docs_$1" "${@:2}"

[private]
_docs_build:
    @zola --root docs build

[private]
_docs_serve:
    @zola --root docs serve --open

############################
#  Legacy manual & Services
############################

[private]
install:
    @scripts/setup.sh

[private]
service-install:
    @just service install

[private]
service-uninstall:
    @just service uninstall

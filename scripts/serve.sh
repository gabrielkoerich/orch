#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
ROOT_DIR=$(cd "$SCRIPT_DIR/.." && pwd)
STATE_DIR=${STATE_DIR:-"${ROOT_DIR}/.orchestrator"}
PID_FILE=${PID_FILE:-"${STATE_DIR}/orchestrator.pid"}
LOG_FILE=${LOG_FILE:-"${STATE_DIR}/orchestrator.log"}
ARCHIVE_LOG=${ARCHIVE_LOG:-"${STATE_DIR}/orchestrator.archive.log"}
INTERVAL=${INTERVAL:-10}
GH_PULL_INTERVAL=${GH_PULL_INTERVAL:-120}
CONFIG_PATH=${CONFIG_PATH:-"${ROOT_DIR}/config.yml"}
SERVE_LOCK=${SERVE_LOCK:-"${STATE_DIR}/serve.lock"}
TAIL_PID_FILE=${TAIL_PID_FILE:-"${STATE_DIR}/tail.pid"}
RESTARTING=${RESTARTING:-0}
_stopping=false

mkdir -p "$STATE_DIR"

# Source lib.sh for gh_backoff_active() and log helpers
source "$SCRIPT_DIR/lib.sh"

export ORCH_VERSION="${ORCH_VERSION:-$(git describe --tags --always 2>/dev/null || echo unknown)}"
_log() { echo "$(date -u +"%Y-%m-%dT%H:%M:%SZ") [v${ORCH_VERSION}] $*"; }

# Single ownership check: PID file takes precedence over lock dir.
if [ "$RESTARTING" != "1" ]; then
  if [ -f "$PID_FILE" ]; then
    EXISTING_PID=$(cat "$PID_FILE")
    if [ -n "$EXISTING_PID" ] && kill -0 "$EXISTING_PID" >/dev/null 2>&1; then
      if [ "$EXISTING_PID" != "$$" ]; then
        echo "Orchestrator already running (pid $EXISTING_PID)."
        echo "Tip: use 'orchestrator restart' to stop and start again."
        exit 0
      fi
    else
      # Stale PID file — process is gone
      rm -f "$PID_FILE"
    fi
  fi
  # Clean stale lock dir if no live process owns it
  rm -rf "$SERVE_LOCK"
fi

if ! mkdir "$SERVE_LOCK" 2>/dev/null; then
  if [ "$RESTARTING" != "1" ]; then
    echo "Orchestrator already running (lock exists)."
    echo "Tip: use 'orchestrator restart' to stop and start again."
    exit 0
  fi
fi

echo $$ > "$PID_FILE"

_on_signal() {
  _stopping=true
  # Kill the backgrounded sleep if any, so the loop unblocks immediately
  [ -n "${_sleep_pid:-}" ] && kill "$_sleep_pid" >/dev/null 2>&1 || true
}

cleanup() {
  rm -f "$PID_FILE"
  rm -rf "$SERVE_LOCK"
  if [ -f "$TAIL_PID_FILE" ]; then
    TPID=$(cat "$TAIL_PID_FILE")
    if [ -n "$TPID" ]; then
      kill "$TPID" >/dev/null 2>&1 || true
    fi
    rm -f "$TAIL_PID_FILE"
  fi
  if [ -n "${TAIL_PID:-}" ]; then
    kill "$TAIL_PID" >/dev/null 2>&1 || true
  fi
}
trap '_on_signal; cleanup' INT TERM
trap cleanup EXIT

# Ensure backend is initialized
db_init

# Rotate log on start
if [ -f "$LOG_FILE" ]; then
  cat "$LOG_FILE" >> "$ARCHIVE_LOG"
  : > "$LOG_FILE"
fi

lock_mtime() {
  local path="$1"
  if [ ! -f "$path" ]; then
    echo 0
    return
  fi
  if stat -f %m "$path" >/dev/null 2>&1; then
    stat -f %m "$path"
    return
  fi
  if stat -c %Y "$path" >/dev/null 2>&1; then
    stat -c %Y "$path"
    return
  fi
  echo 0
}

snapshot_hash() {
  # Guard: ROOT_DIR may vanish after brew upgrade deletes old cellar
  if [ ! -d "$ROOT_DIR" ]; then
    echo "stale"
    return 0
  fi
  # Hash all repo files except runtime state
  fd --type f --no-ignore --hidden \
    --exclude '.git' \
    --exclude 'jobs.yml' \
    --exclude 'config.yml' \
    --exclude '.orchestrator' \
    --exclude 'contexts' \
    --exclude 'skills' \
    -0 \
    . "$ROOT_DIR" \
    | xargs -0 stat -f "%m %N" 2>/dev/null \
    | sort \
    | shasum | awk '{print $1}'
}

LAST_CONFIG_MTIME=$(lock_mtime "$CONFIG_PATH")
PROJECT_CONFIG="${PROJECT_DIR:+${PROJECT_DIR}/orchestrator.yml}"
LAST_PROJECT_CONFIG_MTIME=0
if [ -n "$PROJECT_CONFIG" ] && [ -f "$PROJECT_CONFIG" ]; then
  LAST_PROJECT_CONFIG_MTIME=$(lock_mtime "$PROJECT_CONFIG")
fi
LAST_SNAPSHOT=$(snapshot_hash)
LAST_GH_PULL=0

# Sync skills on start
"$SCRIPT_DIR/skills_sync.sh" >> "$LOG_FILE" 2>&1 || true

_log "[serve] starting v${ORCH_VERSION} with interval=${INTERVAL}s" >> "$LOG_FILE"
run_hook on_service_start

echo "Orchestrator v${ORCH_VERSION} started, listening to tasks and delegating agents." \
  "(pid $(cat "$PID_FILE"), interval ${INTERVAL}s)"

if [ "${TAIL_LOG:-0}" = "1" ]; then
  if [ -f "$TAIL_PID_FILE" ]; then
    EXISTING_TPID=$(cat "$TAIL_PID_FILE")
    if [ -n "$EXISTING_TPID" ] && kill -0 "$EXISTING_TPID" >/dev/null 2>&1; then
      :
    else
      rm -f "$TAIL_PID_FILE"
    fi
  fi
  if [ ! -f "$TAIL_PID_FILE" ]; then
    tail -n 50 -F "$LOG_FILE" &
    TAIL_PID=$!
    echo "$TAIL_PID" > "$TAIL_PID_FILE"
  fi
fi

while true; do
  $_stopping && { _log "[serve] shutting down gracefully" >> "$LOG_FILE"; run_hook on_service_stop; break; }
  _log "[serve] tick" >> "$LOG_FILE"
  "$SCRIPT_DIR/poll.sh" >> "$LOG_FILE" 2>&1 || true
  $_stopping && break
  "$SCRIPT_DIR/jobs_tick.sh" >> "$LOG_FILE" 2>&1 || true
  $_stopping && break
  NOW_EPOCH=$(date +%s)
  if [ $((NOW_EPOCH - LAST_GH_PULL)) -ge "$GH_PULL_INTERVAL" ]; then
    # Skip if GitHub API backoff is active
    if _remaining=$(gh_backoff_active 2>/dev/null); then
      _log "[serve] gh backoff active (${_remaining}s remaining), skipping" >> "$LOG_FILE"
    else
      # Worktree cleanup for each project dir
      DIRS=$(db_task_projects 2>/dev/null || true)
      CLEANED_DEFAULT=false
      for dir in $DIRS; do
        $_stopping && break
        if [ -n "$dir" ] && [ "$dir" != "null" ] && [ -d "$dir" ]; then
          PROJECT_DIR="$dir" "$SCRIPT_DIR/cleanup_worktrees.sh" >> "$LOG_FILE" 2>&1 || true
          CLEANED_DEFAULT=true
        fi
      done
      if [ "$CLEANED_DEFAULT" = false ]; then
        "$SCRIPT_DIR/cleanup_worktrees.sh" >> "$LOG_FILE" 2>&1 || true
      fi
    fi
    LAST_GH_PULL=$NOW_EPOCH

    # Review open PRs (runs inside the same interval gate)
    if ! $_stopping; then
      DIRS=$(db_task_projects 2>/dev/null || true)
      REVIEWED_DEFAULT=false
      for dir in $DIRS; do
        $_stopping && break
        if [ -n "$dir" ] && [ "$dir" != "null" ] && [ -d "$dir" ]; then
          PROJECT_DIR="$dir" "$SCRIPT_DIR/review_prs.sh" >> "$LOG_FILE" 2>&1 || true
          REVIEWED_DEFAULT=true
        fi
      done
      if [ "$REVIEWED_DEFAULT" = false ]; then
        "$SCRIPT_DIR/review_prs.sh" >> "$LOG_FILE" 2>&1 || true
      fi
    fi

    # Check for @orchestrator mentions in issue/PR comments
    if ! $_stopping; then
      DIRS=$(db_task_projects 2>/dev/null || true)
      MENTIONED_DEFAULT=false
      for dir in $DIRS; do
        $_stopping && break
        if [ -n "$dir" ] && [ "$dir" != "null" ] && [ -d "$dir" ]; then
          PROJECT_DIR="$dir" "$SCRIPT_DIR/gh_mentions.sh" >> "$LOG_FILE" 2>&1 || true
          MENTIONED_DEFAULT=true
        fi
      done
      if [ "$MENTIONED_DEFAULT" = false ]; then
        "$SCRIPT_DIR/gh_mentions.sh" >> "$LOG_FILE" 2>&1 || true
      fi
    fi
  fi
  $_stopping && break

  CURRENT_MTIME=$(lock_mtime "$CONFIG_PATH")
  if [ "$CURRENT_MTIME" -ne "$LAST_CONFIG_MTIME" ]; then
    _log "[serve] config.yml changed; restarting" >> "$LOG_FILE"
    exec env RESTARTING=1 "$SCRIPT_DIR/serve.sh"
  fi

  if [ -n "$PROJECT_CONFIG" ] && [ -f "$PROJECT_CONFIG" ]; then
    CURRENT_PROJECT_MTIME=$(lock_mtime "$PROJECT_CONFIG")
    if [ "$CURRENT_PROJECT_MTIME" -ne "$LAST_PROJECT_CONFIG_MTIME" ]; then
      _log "[serve] orchestrator.yml changed; restarting" >> "$LOG_FILE"
      exec env RESTARTING=1 "$SCRIPT_DIR/serve.sh"
    fi
  fi

  CURRENT_SNAPSHOT=$(snapshot_hash)
  if [ "$CURRENT_SNAPSHOT" != "$LAST_SNAPSHOT" ]; then
    _log "[serve] code changed; restarting" >> "$LOG_FILE"
    exec env RESTARTING=1 "$SCRIPT_DIR/serve.sh"
  fi

  # Interruptible sleep: background sleep so SIGTERM wakes us immediately
  sleep "$INTERVAL" &
  _sleep_pid=$!
  wait "$_sleep_pid" 2>/dev/null || true
  _sleep_pid=""
done

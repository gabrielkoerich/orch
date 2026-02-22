#!/usr/bin/env bash
set -euo pipefail

STATE_DIR=${STATE_DIR:-".orchestrator"}
PID_FILE=${PID_FILE:-"${STATE_DIR}/orchestrator.pid"}

# Force mode: kill all orchestrator processes (including orphans from crashed/upgraded instances)
if [ "${1:-}" = "--force" ] || [ "${1:-}" = "-f" ]; then
  pkill -f 'orchestrator.*scripts/serve\.sh' 2>/dev/null || true
  pkill -f 'orchestrator.*scripts/poll\.sh' 2>/dev/null || true
  pkill -f 'orchestrator.*scripts/run_task\.sh' 2>/dev/null || true
  pkill -f 'orchestrator.*scripts/cleanup_worktrees\.sh' 2>/dev/null || true
  pkill -f 'orchestrator.*scripts/route_task\.sh' 2>/dev/null || true
  pkill -f 'orchestrator.*scripts/jobs_tick\.sh' 2>/dev/null || true
  pkill -f 'orchestrator.*scripts/review_prs\.sh' 2>/dev/null || true
  rm -f "$PID_FILE"
  rm -rf "${STATE_DIR}/serve.lock"
  if [ -f "${STATE_DIR}/tail.pid" ]; then
    TPID=$(cat "${STATE_DIR}/tail.pid")
    [ -n "$TPID" ] && kill "$TPID" >/dev/null 2>&1 || true
    rm -f "${STATE_DIR}/tail.pid"
  fi
  echo "Force-killed all orchestrator processes."
  exit 0
fi

if [ ! -f "$PID_FILE" ]; then
  echo "Orchestrator not running (no pid file)."
  exit 0
fi

PID=$(cat "$PID_FILE")
if kill -0 "$PID" >/dev/null 2>&1; then
  kill "$PID"
  # Wait up to 30s for graceful shutdown
  TIMEOUT=30
  while [ "$TIMEOUT" -gt 0 ] && kill -0 "$PID" >/dev/null 2>&1; do
    sleep 1
    TIMEOUT=$((TIMEOUT - 1))
  done
  if kill -0 "$PID" >/dev/null 2>&1; then
    echo "Process $PID did not exit after 30s, sending SIGKILL."
    kill -9 "$PID" >/dev/null 2>&1 || true
  fi
  echo "Stopped orchestrator (pid $PID)."
else
  echo "Orchestrator not running (stale pid $PID)."
fi

rm -f "$PID_FILE"
rm -rf "${STATE_DIR}/serve.lock"
if [ -f "${STATE_DIR}/tail.pid" ]; then
  TPID=$(cat "${STATE_DIR}/tail.pid")
  if [ -n "$TPID" ]; then
    kill "$TPID" >/dev/null 2>&1 || true
  fi
  rm -f "${STATE_DIR}/tail.pid"
fi

# Clean up agent tmux sessions
if command -v tmux >/dev/null 2>&1; then
  ORCH_SESSIONS=$(tmux list-sessions -F '#{session_name}' 2>/dev/null | grep '^orch-' || true)
  if [ -n "$ORCH_SESSIONS" ]; then
    echo "Cleaning up agent tmux sessions..."
    while IFS= read -r session; do
      tmux kill-session -t "$session" 2>/dev/null || true
      echo "  killed $session"
    done <<< "$ORCH_SESSIONS"
  fi
fi

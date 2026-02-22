#!/usr/bin/env bash
# task_attach.sh — Attach to a running agent's tmux session
# Usage: task_attach.sh <task_id>
set -euo pipefail
source "$(dirname "$0")/lib.sh"

TASK_ID="${1:-}"
if [ -z "$TASK_ID" ]; then
  log_err "Usage: task_attach.sh <task_id>"
  exit 1
fi

SESSION="orch-${TASK_ID}"

if ! command -v tmux >/dev/null 2>&1; then
  log_err "tmux is not installed"
  exit 1
fi

if ! tmux has-session -t "$SESSION" 2>/dev/null; then
  log_err "No active tmux session for task $TASK_ID (session: $SESSION)"
  log_err "The agent may have already finished. Check: orch task live"
  exit 1
fi

log_err "Attaching to $SESSION (detach: Ctrl-B D)"
exec tmux attach-session -t "$SESSION"

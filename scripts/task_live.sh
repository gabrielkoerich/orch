#!/usr/bin/env bash
# task_live.sh — List active agent tmux sessions
# Usage: task_live.sh
set -euo pipefail
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/output.sh"

if ! command -v tmux >/dev/null 2>&1; then
  log_err "tmux is not installed"
  exit 1
fi

SESSIONS=$(tmux list-sessions -F '#{session_name} #{session_created} #{session_activity}' 2>/dev/null \
  | grep '^orch-' || true)

if [ -z "$SESSIONS" ]; then
  echo "No active agent sessions."
  exit 0
fi

section "Active Agent Sessions:"
printf '  %-20s %-8s %-10s %s\n' "SESSION" "TASK" "AGENT" "STARTED"
printf '  %-20s %-8s %-10s %s\n' "-------" "----" "-----" "-------"

while IFS=' ' read -r name created activity; do
  TASK_ID="${name#orch-}"
  AGENT=$(db_task_field "$TASK_ID" "agent" 2>/dev/null || echo "?")
  TITLE=$(db_task_field "$TASK_ID" "title" 2>/dev/null || echo "")
  TITLE="${TITLE:0:40}"
  STARTED=$(date -r "$created" +"%H:%M:%S" 2>/dev/null || echo "?")
  printf '  %-20s %-8s %-10s %s  %s\n' "$name" "$TASK_ID" "$AGENT" "$STARTED" "$TITLE"
done <<< "$SESSIONS"

echo ""
echo "Attach: orch task attach <task_id>"

#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"

TASK_ID=${1:-}
AGENT=${2:-}
if [ -z "$TASK_ID" ] || [ -z "$AGENT" ]; then
  echo "usage: set_agent.sh TASK_ID codex|claude" >&2
  exit 1
fi

db_task_update "$TASK_ID" "agent=$AGENT"
append_history "$TASK_ID" "routed" "agent manually set to $AGENT"

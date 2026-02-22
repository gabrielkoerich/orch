#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"

TASK_ID="${1:-}"
if [ -z "$TASK_ID" ]; then
  log_err "Usage: retry_task.sh <task_id>"
  exit 1
fi

STATUS=$(db_task_field "$TASK_ID" "status")
if [ -z "$STATUS" ] || [ "$STATUS" = "null" ]; then
  log_err "[retry] task=$TASK_ID not found"
  exit 1
fi

if [ "$STATUS" = "new" ] || [ "$STATUS" = "routed" ] || [ "$STATUS" = "in_progress" ]; then
  log_err "[retry] task=$TASK_ID already $STATUS"
  exit 0
fi

db_task_update "$TASK_ID" "status=new" "agent=NULL" "limit_reroute_chain=NULL"
append_history "$TASK_ID" "new" "retried from $STATUS"
log "[retry] task=$TASK_ID reset from $STATUS to new"

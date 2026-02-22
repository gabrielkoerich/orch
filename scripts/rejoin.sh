#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"

JOBS=${POLL_JOBS:-4}
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

READY_IDS=()
BLOCKED_IDS=$(db_task_ids_by_status "blocked")
if [ -n "$BLOCKED_IDS" ]; then
  while IFS= read -r id; do
    [ -n "$id" ] || continue

    CHILD_IDS=$(db_task_children "$id")
    if [ -z "$CHILD_IDS" ]; then
      continue
    fi

    ALL_DONE=true
    while IFS= read -r cid; do
      [ -n "$cid" ] || continue
      STATUS=$(db_task_field "$cid" "status")
      if [ "$STATUS" != "done" ]; then
        ALL_DONE=false
        break
      fi
    done <<< "$CHILD_IDS"

    if [ "$ALL_DONE" = true ]; then
      READY_IDS+=("$id")
    fi
  done <<< "$BLOCKED_IDS"
fi

if [ ${#READY_IDS[@]} -gt 0 ]; then
  printf '%s\n' "${READY_IDS[@]}" | xargs -n1 -P "$JOBS" -I{} "$SCRIPT_DIR/run_task.sh" "{}"
fi

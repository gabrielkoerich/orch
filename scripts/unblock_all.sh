#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
BLOCKED_IDS=$(db_task_ids_by_status "blocked")

if [ -z "$BLOCKED_IDS" ]; then
  echo "No blocked tasks."
  exit 0
fi

while IFS= read -r id; do
  [ -n "$id" ] || continue
  "$SCRIPT_DIR/retry_task.sh" "$id"
done <<< "$BLOCKED_IDS"

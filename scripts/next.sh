#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"

PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"
export PROJECT_DIR

ID=$(db_task_ids_by_status "new" | head -1)
if [ -n "$ID" ]; then
  "$(dirname "$0")/route_task.sh" "$ID" >/dev/null
  "$(dirname "$0")/run_task.sh" "$ID"
  exit 0
fi

# Fallback to any runnable task
"$(dirname "$0")/run_task.sh"

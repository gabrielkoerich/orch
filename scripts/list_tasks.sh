#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/output.sh"

PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"
export PROJECT_DIR

COUNT=$(db_total_filtered_count)
if [ "$COUNT" -eq 0 ]; then
  echo "No tasks."
  exit 0
fi

db_task_display_tsv "true" "id" \
  | table_with_header "$TASK_HEADER"

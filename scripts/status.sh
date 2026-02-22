#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/output.sh"

PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"
export PROJECT_DIR

# Parse flags
IS_GLOBAL=false
IS_JSON=false
for arg in "$@"; do
  case "$arg" in
    --global|-g) IS_GLOBAL=true ;;
    --json)      IS_JSON=true ;;
  esac
done

if [ "$IS_JSON" = true ]; then
  db_status_json ""
  exit 0
fi

if [ "$IS_GLOBAL" = true ]; then
  : # counts below are global by default in the GitHub backend
else
  : # counts below are filtered by PROJECT_DIR in non-GitHub backends
fi

_count() {
  db_status_count "$1"
}

NEW=$(_count "new")
ROUTED=$(_count "routed")
INPROG=$(_count "in_progress")
IN_REVIEW=$(_count "in_review")
BLOCKED=$(_count "blocked")
DONE=$(_count "done")
NEEDS_REVIEW=$(_count "needs_review")
OPEN_TOTAL=$((NEW + ROUTED + INPROG + IN_REVIEW + BLOCKED + NEEDS_REVIEW))
TOTAL=$((OPEN_TOTAL + DONE))

if [ "$TOTAL" -eq 0 ]; then
  echo "No tasks. Add one with: orchestrator task add \"title\""
  exit 0
fi

{
  printf 'STATUS\tQTY\n'
  printf 'new\t%s\n' "$NEW"
  printf 'routed\t%s\n' "$ROUTED"
  printf 'in_progress\t%s\n' "$INPROG"
  printf 'in_review\t%s\n' "$IN_REVIEW"
  printf 'blocked\t%s\n' "$BLOCKED"
  printf 'needs_review\t%s\n' "$NEEDS_REVIEW"
  printf 'done\t%s\n' "$DONE"
  printf '────────\t───\n'
  printf 'open\t%s\n' "$OPEN_TOTAL"
  printf 'total\t%s\n' "$TOTAL"
} | column -t -s $'\t'

section "Recent tasks:"
if [ "$IS_GLOBAL" = true ]; then
  db_task_display_tsv_global "true" "updated_at" "10" \
    | table_with_header "$TASK_HEADER_GLOBAL"
else
  db_task_display_tsv "true" "id" "10" \
    | table_with_header "$TASK_HEADER"
fi

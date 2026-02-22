#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"

# Parse named args: --type bash --command "cmd" or positional for task type
JOB_TYPE="task"
COMMAND=""
SCHEDULE=""
TITLE=""
BODY=""
LABELS=""
AGENT=""

POS_INDEX=0
while [ $# -gt 0 ]; do
  case "$1" in
    --type)    JOB_TYPE="$2"; shift 2 ;;
    --command) COMMAND="$2"; shift 2 ;;
    *)
      # Positional args by index: schedule title [body] [labels] [agent]
      case $POS_INDEX in
        0) SCHEDULE="$1" ;;
        1) TITLE="$1" ;;
        2) BODY="$1" ;;
        3) LABELS="$1" ;;
        4) AGENT="$1" ;;
      esac
      POS_INDEX=$((POS_INDEX + 1))
      shift
      ;;
  esac
done

if [ -z "$SCHEDULE" ] || [ -z "$TITLE" ]; then
  echo "usage: jobs_add.sh \"SCHEDULE\" \"TITLE\" [\"BODY\"] [\"LABELS\"] [\"AGENT\"]" >&2
  echo "       jobs_add.sh --type bash --command \"CMD\" \"SCHEDULE\" \"TITLE\"" >&2
  echo "" >&2
  echo "  --type bash:  run a shell command directly (no LLM)" >&2
  echo "  --command:    the command to run (required for type=bash)" >&2
  echo "  SCHEDULE:     cron expression or alias (@hourly, @daily, @weekly, @monthly, @yearly)" >&2
  echo "  TITLE:        job title" >&2
  echo "  BODY:         task body (optional, ignored for type=bash)" >&2
  echo "  LABELS:       comma-separated labels (optional)" >&2
  echo "  AGENT:        force agent (optional, default: router decides)" >&2
  exit 1
fi

if [ "$JOB_TYPE" = "bash" ] && [ -z "$COMMAND" ]; then
  echo "error: --command is required for --type bash" >&2
  exit 1
fi

# Validate cron expression
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
if ! python3 "${SCRIPT_DIR}/cron_match.py" "$SCHEDULE" >/dev/null 2>&1; then
  RC=0
  python3 -c "
import sys; sys.path.insert(0, '${SCRIPT_DIR}')
from cron_match import ALIASES
expr = ALIASES.get('${SCHEDULE}'.strip(), '${SCHEDULE}'.strip())
fields = expr.split()
if len(fields) != 5:
    sys.exit(2)
" 2>/dev/null || RC=$?
  if [ "$RC" -eq 2 ]; then
    echo "Invalid cron expression: $SCHEDULE" >&2
    exit 1
  fi
fi

# Generate ID from title
JOB_ID=$(printf '%s' "$TITLE" | tr '[:upper:]' '[:lower:]' | sed 's/[^a-z0-9]/-/g; s/--*/-/g; s/^-//; s/-$//' | cut -c1-40)

# Check for duplicate ID
EXISTING=$(db_job_field "$JOB_ID" "id" 2>/dev/null || true)
if [ -n "$EXISTING" ]; then
  echo "Job '$JOB_ID' already exists. Choose a different title." >&2
  exit 1
fi

PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"

db_create_job "$JOB_ID" "$TITLE" "$SCHEDULE" "$JOB_TYPE" "$BODY" "$LABELS" "$AGENT" "$PROJECT_DIR" "$COMMAND"

if [ "$JOB_TYPE" = "bash" ]; then
  echo "Added bash job '$JOB_ID' (schedule: $SCHEDULE, command: $COMMAND)"
else
  echo "Added job '$JOB_ID' (schedule: $SCHEDULE)"
fi

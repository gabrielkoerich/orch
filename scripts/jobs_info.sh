#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/output.sh"

JOB_ID="$1"
if [ -z "$JOB_ID" ]; then
  echo "usage: jobs_info.sh JOB_ID" >&2
  exit 1
fi

# Check if job exists
TYPE=$(db_job_field "$JOB_ID" "type" 2>/dev/null || true)
if [ -z "$TYPE" ]; then
  echo "Job '$JOB_ID' not found." >&2
  exit 1
fi
[ "$TYPE" = "null" ] && TYPE="task"

SCHEDULE=$(db_job_field "$JOB_ID" "schedule")
ENABLED=$(db_job_field "$JOB_ID" "enabled")
LAST_RUN=$(db_job_field "$JOB_ID" "last_run")
LAST_STATUS=$(db_job_field "$JOB_ID" "last_task_status")
ACTIVE_TASK=$(db_job_field "$JOB_ID" "active_task_id")
DIR=$(db_job_field "$JOB_ID" "dir")

[ -z "$LAST_RUN" ] || [ "$LAST_RUN" = "null" ] && LAST_RUN="-"
[ -z "$LAST_STATUS" ] || [ "$LAST_STATUS" = "null" ] && LAST_STATUS="-"
[ -z "$ACTIVE_TASK" ] || [ "$ACTIVE_TASK" = "null" ] && ACTIVE_TASK="-"
[ -z "$DIR" ] || [ "$DIR" = "null" ] && DIR="-"
[ "$ENABLED" = "1" ] && ENABLED="true" || ENABLED="false"

kv "ID" "$JOB_ID"
kv "Type" "$TYPE"
kv "Schedule" "$SCHEDULE"
kv "Enabled" "$ENABLED"
kv "Last run" "$LAST_RUN"
kv "Last status" "$LAST_STATUS"
kv "Active task" "$ACTIVE_TASK"
kv "Directory" "$DIR"

if [ "$TYPE" = "bash" ]; then
  COMMAND=$(db_job_field "$JOB_ID" "command")
  section "Command"
  echo "${COMMAND:-}"
else
  TITLE=$(db_job_field "$JOB_ID" "title")
  BODY=$(db_job_field "$JOB_ID" "body")
  LABELS=$(db_job_field "$JOB_ID" "labels")
  AGENT=$(db_job_field "$JOB_ID" "agent")
  [ -z "$AGENT" ] || [ "$AGENT" = "null" ] && AGENT="(router decides)"

  section "Task"
  kv "Title" "$TITLE"
  kv "Agent" "$AGENT"
  kv "Labels" "${LABELS:-}"

  if [ -n "$BODY" ] && [ "$BODY" != "null" ]; then
    section "Body"
    echo "$BODY"
  fi
fi

#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/output.sh"

JOB_COUNT=$(db_enabled_job_count)
TOTAL=$(_read_jobs | jq 'length')
if [ "$TOTAL" -eq 0 ]; then
  echo "No jobs configured. Add one with: orchestrator job add \"0 9 * * *\" \"Title\""
  exit 0
fi

{
  printf 'ID\tTYPE\tSCHEDULE\tENABLED\tSTATUS\tLAST RUN\n'
  _read_jobs | jq -r '.[] | [.id, (.type // "task"), .schedule, (if .enabled then "true" else "false" end), (.last_task_status // "-"), (.last_run // "-")] | @tsv' \
    | while IFS=$'\t' read -r jid jtype jsched jenabled jstatus jrun; do
    [ "$jstatus" = "null" ] && jstatus="-"
    [ "$jrun" = "null" ] && jrun="-"
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$jid" "$jtype" "$jsched" "$jenabled" "$jstatus" "$jrun"
  done
} | column -t -s $'\t'

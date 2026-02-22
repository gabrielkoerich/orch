#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"

JOB_ID=${1:-}
if [ -z "$JOB_ID" ]; then
  echo "usage: jobs_remove.sh JOB_ID" >&2
  exit 1
fi

EXISTING=$(db_job_field "$JOB_ID" "id" 2>/dev/null || true)
if [ -z "$EXISTING" ]; then
  echo "Job '$JOB_ID' not found" >&2
  exit 1
fi

db_job_delete "$JOB_ID"
echo "Removed job '$JOB_ID'"

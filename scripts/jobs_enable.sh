#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"

ID="${1:?Usage: jobs_enable.sh <job-id>}"
db_job_set "$ID" "enabled" "1"
echo "Enabled job '$ID'"

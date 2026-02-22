#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"

ID="${1:?Usage: jobs_disable.sh <job-id>}"
db_job_set "$ID" "enabled" "0"
echo "Disabled job '$ID'"

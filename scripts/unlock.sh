#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"

rm -rf "${LOCK_PATH}" "${LOCK_PATH}".task.*
echo "Removed task locks."

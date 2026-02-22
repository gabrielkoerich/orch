#!/usr/bin/env bash
set -euo pipefail

INTERVAL=${1:-10}

while true; do
  "$(dirname "$0")/poll.sh"
  sleep "$INTERVAL"
done

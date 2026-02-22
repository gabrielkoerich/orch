#!/usr/bin/env bash
set -euo pipefail

if ! crontab -l 2>/dev/null | grep -qE "orchestrator (jobs-tick|job tick)"; then
  echo "No crontab entry found for orchestrator job tick"
  exit 0
fi

(crontab -l 2>/dev/null | grep -vE "orchestrator (jobs-tick|job tick)") | crontab -

echo "Removed crontab entry for orchestrator job tick"

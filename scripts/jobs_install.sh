#!/usr/bin/env bash
set -euo pipefail

BIN_DIR=${BIN_DIR:-"$HOME/.bin"}
ORCH_BIN="${BIN_DIR}/orchestrator"
STATE_DIR=${STATE_DIR:-"$HOME/.orchestrator/.orchestrator"}

if [ ! -f "$ORCH_BIN" ]; then
  echo "Orchestrator binary not found at $ORCH_BIN" >&2
  echo "Run 'just install' first." >&2
  exit 1
fi

mkdir -p "$STATE_DIR"
CRON_CMD="* * * * * ${ORCH_BIN} job tick >> ${STATE_DIR}/jobs.log 2>&1"

# Check if already installed (match both old and new command names)
if crontab -l 2>/dev/null | grep -qE "orchestrator (jobs-tick|job tick)"; then
  echo "Crontab entry already exists:"
  crontab -l 2>/dev/null | grep -E "orchestrator (jobs-tick|job tick)"
  exit 0
fi

# Add entry (preserve existing crontab)
(crontab -l 2>/dev/null; echo "$CRON_CMD") | crontab -

echo "Installed crontab entry:"
echo "  $CRON_CMD"
echo ""
echo "Jobs tick will run every minute. View log at:"
echo "  $STATE_DIR/jobs.log"

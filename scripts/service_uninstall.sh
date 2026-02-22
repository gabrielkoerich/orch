#!/usr/bin/env bash
set -euo pipefail

LABEL="com.orchestrator.serve"
PLIST_DIR="$HOME/Library/LaunchAgents"
PLIST_PATH="${PLIST_DIR}/${LABEL}.plist"

if [ "$(uname)" != "Darwin" ]; then
  echo "launchd services are macOS only." >&2
  exit 1
fi

if [ ! -f "$PLIST_PATH" ]; then
  echo "Service not installed (no plist at $PLIST_PATH)." >&2
  exit 0
fi

launchctl unload "$PLIST_PATH" 2>/dev/null || true
rm -f "$PLIST_PATH"

echo "Uninstalled launchd service: $LABEL"
echo "The orchestrator will no longer start automatically."

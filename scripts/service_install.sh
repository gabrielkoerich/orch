#!/usr/bin/env bash
set -euo pipefail

LABEL="com.orchestrator.serve"
PLIST_DIR="$HOME/Library/LaunchAgents"
PLIST_PATH="${PLIST_DIR}/${LABEL}.plist"
ORCH_BIN="${HOME}/.bin/orchestrator"
STATE_DIR="${HOME}/.orchestrator/.orchestrator"

if [ "$(uname)" != "Darwin" ]; then
  echo "launchd services are macOS only." >&2
  exit 1
fi

if [ ! -x "$ORCH_BIN" ]; then
  echo "orchestrator binary not found at $ORCH_BIN" >&2
  echo "Run 'just install' first." >&2
  exit 1
fi

mkdir -p "$PLIST_DIR" "$STATE_DIR"

# Unload existing service if present
if launchctl list "$LABEL" >/dev/null 2>&1; then
  launchctl unload "$PLIST_PATH" 2>/dev/null || true
fi

cat > "$PLIST_PATH" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${ORCH_BIN}</string>
    <string>serve</string>
  </array>
  <key>WorkingDirectory</key>
  <string>${HOME}/.orchestrator</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>${HOME}/.bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    <key>HOME</key>
    <string>${HOME}</string>
  </dict>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>${STATE_DIR}/launchd.out.log</string>
  <key>StandardErrorPath</key>
  <string>${STATE_DIR}/launchd.err.log</string>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
EOF

launchctl load "$PLIST_PATH"

echo "Installed launchd service: $LABEL"
echo "Plist: $PLIST_PATH"
echo "Logs:  $STATE_DIR/launchd.{out,err}.log"
echo ""
echo "The orchestrator will start automatically and restart on crashes."
echo "To stop:   just service-uninstall"
echo "To check:  launchctl list | grep orchestrator"

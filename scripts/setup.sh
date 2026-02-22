#!/usr/bin/env bash
set -euo pipefail

TARGET_DIR=${ORCH_HOME:-"$HOME/.orchestrator"}
BIN_DIR=${BIN_DIR:-"$HOME/.bin"}

mkdir -p "$TARGET_DIR" "$BIN_DIR"

# Copy repo to target, preserving local state/config
rsync -a --delete \
  --exclude '.git' \
  --exclude 'jobs.yml' \
  --exclude 'config.yml' \
  --exclude 'contexts/' \
  --exclude 'skills/' \
  "$(cd "$(dirname "$0")/.." && pwd)/" "$TARGET_DIR/"

# Initialize config if missing
if [ ! -f "$TARGET_DIR/config.yml" ] && [ -f "$TARGET_DIR/config.example.yml" ]; then
  cp "$TARGET_DIR/config.example.yml" "$TARGET_DIR/config.yml"
fi

# GitHub config is now handled per-project via 'orchestrator init'
# See scripts/init.sh for interactive GitHub project setup

# Sync skills catalog
"$TARGET_DIR/scripts/skills_sync.sh" || true

# Install justfile shortcut
cat > "$BIN_DIR/orchestrator" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
export PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"
cd "$HOME/.orchestrator"
just "$@"
EOF
chmod +x "$BIN_DIR/orchestrator"

if [ -f "$TARGET_DIR/config.yml" ]; then
  gh_project_id=$(yq -r '.gh.project_id // ""' "$TARGET_DIR/config.yml" 2>/dev/null || true)
  gh_status_field_id=$(yq -r '.gh.project_status_field_id // ""' "$TARGET_DIR/config.yml" 2>/dev/null || true)
  gh_backlog_id=$(yq -r '.gh.project_status_map.backlog // ""' "$TARGET_DIR/config.yml" 2>/dev/null || true)
  gh_inprog_id=$(yq -r '.gh.project_status_map.in_progress // ""' "$TARGET_DIR/config.yml" 2>/dev/null || true)
  gh_review_id=$(yq -r '.gh.project_status_map.review // ""' "$TARGET_DIR/config.yml" 2>/dev/null || true)
  gh_done_id=$(yq -r '.gh.project_status_map.done // ""' "$TARGET_DIR/config.yml" 2>/dev/null || true)

  echo
  echo "GitHub Project config set:"
  if [ -n "$gh_project_id" ] && [ "$gh_project_id" != "null" ]; then
    echo "  project_id: $gh_project_id"
  else
    echo "  project_id: (not set)"
  fi
  if [ -n "$gh_status_field_id" ] && [ "$gh_status_field_id" != "null" ]; then
    echo "  status_field_id: $gh_status_field_id"
  else
    echo "  status_field_id: (not set)"
  fi
  echo "  status_map (set):"
  status_map_set=false
  if [ -n "$gh_backlog_id" ] && [ "$gh_backlog_id" != "null" ]; then
    echo "    backlog: $gh_backlog_id"
    status_map_set=true
  fi
  if [ -n "$gh_inprog_id" ] && [ "$gh_inprog_id" != "null" ]; then
    echo "    in_progress: $gh_inprog_id"
    status_map_set=true
  fi
  if [ -n "$gh_review_id" ] && [ "$gh_review_id" != "null" ]; then
    echo "    review: $gh_review_id"
    status_map_set=true
  fi
  if [ -n "$gh_done_id" ] && [ "$gh_done_id" != "null" ]; then
    echo "    done: $gh_done_id"
    status_map_set=true
  fi
  if [ "$status_map_set" = false ]; then
    echo "    (none)"
  fi
fi

# Check available agents
echo
echo "Agent CLIs:"
has_agent=false
for agent in claude codex opencode; do
  if command -v "$agent" >/dev/null 2>&1; then
    echo "  $agent: $(command -v "$agent")"
    has_agent=true
  else
    echo "  $agent: not found"
  fi
done
if [ "$has_agent" = false ]; then
  echo
  echo "Error: No agent CLIs found. Install at least one (claude, codex, or opencode)." >&2
  exit 1
fi

echo
echo "Installed to $TARGET_DIR"
echo "Binary: $BIN_DIR/orchestrator"

# Offer launchd service on macOS
if [ "$(uname)" = "Darwin" ] && [ -t 0 ]; then
  echo
  read -r -p "Install macOS background service (auto-start + restart on crash)? (y/N): " INSTALL_SERVICE
  if [ "${INSTALL_SERVICE}" = "y" ] || [ "${INSTALL_SERVICE}" = "Y" ]; then
    "$TARGET_DIR/scripts/service_install.sh"
  fi
fi

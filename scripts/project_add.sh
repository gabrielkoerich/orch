#!/usr/bin/env bash
# Add an external GitHub repo to orchestrator via bare clone.
# Usage: project_add.sh <owner/repo | https://github.com/owner/repo | git@github.com:owner/repo.git>
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
source "$SCRIPT_DIR/lib.sh"
require_yq

INPUT="${1:-}"
if [ -z "$INPUT" ]; then
  echo "Usage: orchestrator project add <owner/repo>" >&2
  exit 1
fi

# Normalize input to owner/repo slug
SLUG=$(printf '%s' "$INPUT" \
  | sed -E 's#^https?://github\.com/##' \
  | sed -E 's#^git@github\.com:##' \
  | sed -E 's#\.git$##' \
  | sed -E 's#/$##')

# Validate slug format
if ! printf '%s' "$SLUG" | grep -qE '^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$'; then
  echo "Invalid repo: $SLUG (expected owner/repo)" >&2
  exit 1
fi

OWNER=$(printf '%s' "$SLUG" | cut -d/ -f1)
REPO_NAME=$(printf '%s' "$SLUG" | cut -d/ -f2)

PROJECTS_DIR="${ORCH_HOME}/projects"
BARE_DIR="${PROJECTS_DIR}/${OWNER}/${REPO_NAME}.git"

CLONE_URL="git@github.com:${SLUG}.git"

if [ -d "$BARE_DIR" ]; then
  echo "Already cloned — fetching latest..."
  git -C "$BARE_DIR" fetch --all --prune
else
  echo "Cloning ${SLUG} (bare)..."
  mkdir -p "$(dirname "$BARE_DIR")"
  git clone --bare "$CLONE_URL" "$BARE_DIR"
fi

# Write orchestrator.yml inside the bare repo
CONFIG_FILE="${BARE_DIR}/orchestrator.yml"
if [ -f "$CONFIG_FILE" ]; then
  # Update repo slug if it changed
  export SLUG
  yq -i '.gh.repo = strenv(SLUG)' "$CONFIG_FILE"
else
  cat > "$CONFIG_FILE" <<YAML
gh:
  repo: "${SLUG}"
  sync_label: ""
YAML
fi

echo ""
echo "Bare repo: $BARE_DIR"
echo "Config:    $CONFIG_FILE"

# Delegate to init.sh for GitHub Project setup (non-interactive via --repo flag)
echo ""
echo "Running init..."
PROJECT_DIR="$BARE_DIR" "$SCRIPT_DIR/init.sh" --repo "$SLUG"

echo ""
echo "=== Summary ==="
echo "  Repo:      $SLUG"
echo "  Bare clone: $BARE_DIR"
echo "  Config:     $CONFIG_FILE"
echo ""
echo "Next steps:"
echo "  orchestrator task add \"your task title\"   # with PROJECT_DIR=$BARE_DIR"
echo "  orchestrator start                         # serve loop picks it up"

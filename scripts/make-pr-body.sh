#!/usr/bin/env bash
set -euo pipefail

BASE_BRANCH="${1:-main}"

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT_DIR"

mkdir -p .orchestrator

TITLE=$(git log -1 --format=%s)
BRANCH=$(git branch --show-current 2>/dev/null || true)
TASK_ID=""
if [[ "$BRANCH" =~ ^gh-task-([0-9]+)- ]]; then
  TASK_ID="${BASH_REMATCH[1]}"
fi

{
  echo "$TITLE"
} > .orchestrator/pr-title.txt

{
  echo "## Summary"
  echo ""
  git log --format='- %s' "${BASE_BRANCH}..HEAD"
  echo ""
  echo "## Testing"
  echo ""
  echo "\`HOME=/tmp/orch-test-home bats tests/orchestrator.bats\`"
  echo ""
  if [ -n "$TASK_ID" ]; then
    echo "Closes #${TASK_ID}"
  fi
} > .orchestrator/pr-body.md

echo "Wrote .orchestrator/pr-title.txt and .orchestrator/pr-body.md"

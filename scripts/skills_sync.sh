#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"
require_yq

ORCH_HOME="${ORCH_HOME:-$HOME/.orchestrator}"
SKILLS_DIR=${SKILLS_DIR:-"${ORCH_HOME}/skills"}
mkdir -p "$SKILLS_DIR"

# Look for skills.yml in ORCH_HOME first, then CWD
SKILLS_YML=""
if [ -f "${ORCH_HOME}/skills.yml" ]; then
  SKILLS_YML="${ORCH_HOME}/skills.yml"
elif [ -f "skills.yml" ]; then
  SKILLS_YML="skills.yml"
fi

if [ -z "$SKILLS_YML" ]; then
  log "[skills_sync] no skills.yml found"
  exit 0
fi

REPO_COUNT=$(yq -r '.repositories | length' "$SKILLS_YML")
if [ "$REPO_COUNT" -eq 0 ]; then
  log "[skills_sync] no repositories in $SKILLS_YML"
  exit 0
fi

for i in $(seq 0 $((REPO_COUNT - 1))); do
  NAME=$(yq -r ".repositories[$i].name" "$SKILLS_YML")
  URL=$(yq -r ".repositories[$i].url" "$SKILLS_YML")
  PIN=$(yq -r ".repositories[$i].pin // \"\"" "$SKILLS_YML")
  if [ -z "$NAME" ] || [ -z "$URL" ] || [ "$NAME" = "null" ] || [ "$URL" = "null" ]; then
    continue
  fi

  DEST="$SKILLS_DIR/$NAME"
  if [ -d "$DEST/.git" ]; then
    log "[skills_sync] updating $NAME"
    git -C "$DEST" fetch --all --prune >/dev/null 2>&1 || true
    if [ -n "$PIN" ] && [ "$PIN" != "null" ]; then
      git -C "$DEST" checkout "$PIN" --quiet 2>/dev/null || true
    else
      git -C "$DEST" pull --ff-only >/dev/null 2>&1 || true
    fi
  else
    log "[skills_sync] cloning $NAME"
    git clone "$URL" "$DEST" >/dev/null 2>&1 || true
    if [ -n "$PIN" ] && [ "$PIN" != "null" ]; then
      git -C "$DEST" checkout "$PIN" --quiet 2>/dev/null || true
    fi
  fi
done

log "[skills_sync] done (dir=$SKILLS_DIR)"

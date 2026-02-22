#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"
require_yq

ORCH_HOME="${ORCH_HOME:-$HOME/.orchestrator}"

SKILLS_YML=""
if [ -f "${ORCH_HOME}/skills.yml" ]; then
  SKILLS_YML="${ORCH_HOME}/skills.yml"
elif [ -f "skills.yml" ]; then
  SKILLS_YML="skills.yml"
fi

if [ -z "$SKILLS_YML" ]; then
  log_err "No skills.yml found"
  exit 0
fi

SKILL_COUNT=$(yq -r '.skills | length // 0' "$SKILLS_YML")
if [ "$SKILL_COUNT" -eq 0 ]; then
  echo "No skills in catalog. Run: orchestrator skills sync"
  exit 0
fi

{
  printf 'ID\tNAME\tREPO\n'
  yq -r '.skills[] | [.id, .name // .id, .repo // "-"] | @tsv' "$SKILLS_YML"
} | column -t -s $'\t'

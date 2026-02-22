#!/usr/bin/env bash
# shellcheck source=scripts/lib.sh
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
source "$SCRIPT_DIR/lib.sh"
require_jq
require_rg
PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"
# Brew service starts with CWD / — detect and fix
if [ "$PROJECT_DIR" = "/" ] || { [ ! -d "$PROJECT_DIR/.git" ] && ! is_bare_repo "$PROJECT_DIR" 2>/dev/null; }; then
  _cfg_dir=$(config_get '.project_dir // ""' 2>/dev/null || true)
  if [ -n "$_cfg_dir" ] && [ "$_cfg_dir" != "null" ] && [ -d "$_cfg_dir" ]; then
    PROJECT_DIR="$_cfg_dir"
  elif [ -d "${ORCH_HOME:-$HOME/.orchestrator}" ]; then
    PROJECT_DIR="${ORCH_HOME:-$HOME/.orchestrator}"
  fi
fi
export PROJECT_DIR
init_config_file
load_project_config

TASK_ID=${1:-}
if [ -z "$TASK_ID" ]; then
  TASK_ID=$(db_task_ids_by_status "new" | head -1)
  if [ -z "$TASK_ID" ]; then
    log_err "No new tasks to route"
    exit 1
  fi
fi

log_err "[route] task=$TASK_ID starting"
mkdir -p .orchestrator
CMD_STATUS=0
ROUTED_AGENT=""
TMP_PROFILE=""
trap 'rm -f "$TMP_PROFILE"' EXIT

TASK_TITLE=$(db_task_field "$TASK_ID" "title")
TASK_BODY=$(db_task_field "$TASK_ID" "body")
TASK_LABELS=$(db_task_labels_csv "$TASK_ID")
ROUTING_MODE=${ROUTING_MODE:-$(config_get '.router.mode // "llm"')}
ROUTER_AGENT=${ROUTER_AGENT:-$(config_get '.router.agent // "claude"')}
ROUTER_MODEL=${ROUTER_MODEL:-$(config_get '.router.model // ""')}
ROUTER_TIMEOUT=${ROUTER_TIMEOUT:-$(config_get '.router.timeout_seconds // ""')}
ROUTER_FALLBACK=${ROUTER_FALLBACK:-$(config_get '.router.fallback_executor // "codex"')}
ALLOWED_TOOLS_CSV=$(config_get '.router.allowed_tools // [] | join(",")')
DEFAULT_SKILLS_CSV=$(config_get '.router.default_skills // [] | join(",")')

if [ -z "$TASK_TITLE" ] || [ "$TASK_TITLE" = "null" ]; then
  log_err "Task $TASK_ID not found"
  exit 1
fi

SKILLS_CATALOG=""
if [ -f "skills.yml" ]; then
  SKILLS_CATALOG=$(yq -o=json -I=0 '.skills // []' "skills.yml" 2>/dev/null || echo '[]')
fi
if [ -z "$SKILLS_CATALOG" ] || [ "$SKILLS_CATALOG" = "[]" ] || [ "$SKILLS_CATALOG" = "null" ]; then
  ORCH_SKILLS="${ORCH_HOME:-$HOME/.orchestrator}/skills"
  if [ -d "$ORCH_SKILLS" ]; then
    SKILLS_CATALOG=$(build_skills_catalog "$ORCH_SKILLS")
  elif [ -d "skills" ]; then
    SKILLS_CATALOG=$(build_skills_catalog "skills")
  fi
fi

AVAILABLE_AGENTS=$(available_agents)
if [ -z "$AVAILABLE_AGENTS" ]; then
  log_err "No agent CLIs found in PATH."
  exit 1
fi

export TASK_ID TASK_TITLE TASK_LABELS TASK_BODY SKILLS_CATALOG AVAILABLE_AGENTS

# ── Round-robin mode: skip LLM router, cycle through agents ──
if [ "$ROUTING_MODE" = "round_robin" ]; then
  IFS=',' read -ra _agents <<< "$AVAILABLE_AGENTS"
  _agent_count=${#_agents[@]}
  ROUTED_AGENT="${_agents[$((TASK_ID % _agent_count))]}"
  REASON="round_robin (task $TASK_ID % $_agent_count agents)"
  COMPLEXITY="medium"
  PROFILE_JSON='{}'
  SELECTED_SKILLS_CSV=""
  ROUTE_WARNING=""

  if [ -n "$ALLOWED_TOOLS_CSV" ]; then
    PROFILE_JSON=$(printf '%s' "$PROFILE_JSON" | jq -c --arg tools "$ALLOWED_TOOLS_CSV" '.tools = ($tools | split(",") | map(select(length > 0)))')
  fi
  if [ -n "$DEFAULT_SKILLS_CSV" ]; then
    SELECTED_SKILLS_CSV="$DEFAULT_SKILLS_CSV"
  fi

  log_err "[route] round_robin: task=$TASK_ID → $ROUTED_AGENT (${TASK_ID} % ${_agent_count})"

  db_task_update "$TASK_ID" \
    "agent=$ROUTED_AGENT" \
    "complexity=$COMPLEXITY" \
    "status=routed" \
    "route_reason=$REASON" \
    "agent_profile=$PROFILE_JSON" \
    "dir=$PROJECT_DIR"

  if [ -n "$SELECTED_SKILLS_CSV" ]; then
    db_set_selected_skills "$TASK_ID" "$SELECTED_SKILLS_CSV"
  fi

  db_add_label "$TASK_ID" "agent:${ROUTED_AGENT}"
  append_history "$TASK_ID" "routed" "routed to $ROUTED_AGENT ($REASON)"

  log_err "[route] task=$TASK_ID routed to $ROUTED_AGENT"
  export TASK_AGENT="$ROUTED_AGENT"
  run_hook on_task_routed
  echo "$ROUTED_AGENT"
  exit 0
fi

# ── LLM router mode (default) ──
PROMPT=$(render_template "$SCRIPT_DIR/../prompts/route.md")
PROMPT_FILE=".orchestrator/route-prompt-${TASK_ID}.txt"
printf '%s' "$PROMPT" > "$PROMPT_FILE"

require_agent "$ROUTER_AGENT"

run_router_cmd() {
  if [ -n "${ROUTER_TIMEOUT:-}" ] && [ "${ROUTER_TIMEOUT}" != "0" ]; then
    AGENT_TIMEOUT_SECONDS="${ROUTER_TIMEOUT}" run_with_timeout "$@"
    return $?
  fi
  "$@"
}

start_spinner "Routing task $TASK_ID"

case "$ROUTER_AGENT" in
  codex)
    log_err "[route] using codex model=${ROUTER_MODEL:-default}"
    log_err "[route] timeout=${ROUTER_TIMEOUT:-${AGENT_TIMEOUT_SECONDS:-900}}s"
    if [ -n "$ROUTER_MODEL" ]; then
      RESPONSE=$(run_router_cmd codex exec --model "$ROUTER_MODEL" --json "$PROMPT") || CMD_STATUS=$?
    else
      RESPONSE=$(run_router_cmd codex exec --json "$PROMPT") || CMD_STATUS=$?
    fi
    ;;
  claude)
    log_err "[route] using claude model=${ROUTER_MODEL:-default}"
    log_err "[route] timeout=${ROUTER_TIMEOUT:-${AGENT_TIMEOUT_SECONDS:-900}}s"
    if [ -n "$ROUTER_MODEL" ]; then
      RESPONSE=$(run_router_cmd claude --model "$ROUTER_MODEL" --output-format json --print "$PROMPT") || CMD_STATUS=$?
    else
      RESPONSE=$(run_router_cmd claude --output-format json --print "$PROMPT") || CMD_STATUS=$?
    fi
    ;;
  opencode)
    log_err "[route] using opencode"
    log_err "[route] timeout=${ROUTER_TIMEOUT:-${AGENT_TIMEOUT_SECONDS:-900}}s"
    RESPONSE=$(run_router_cmd opencode run --format json "$PROMPT") || CMD_STATUS=$?
    ;;
  *)
    log_err "[route] unknown router agent: $ROUTER_AGENT"
    exit 1
    ;;
esac

stop_spinner
log_err "[route] raw response:"
printf '%s\n' "$RESPONSE" | sed 's/^/[route] > /' >&2

if [ "${CMD_STATUS:-0}" -ne 0 ]; then
  log_err "[route] router failed exit=${CMD_STATUS}"
  if [ -n "${ROUTER_FALLBACK:-}" ]; then
    if command -v "$ROUTER_FALLBACK" >/dev/null 2>&1; then
      ROUTED_AGENT="$ROUTER_FALLBACK"
    else
      ROUTED_AGENT=$(printf '%s' "$AVAILABLE_AGENTS" | cut -d',' -f1)
    fi
    REASON="router failed (exit $CMD_STATUS); fallback to $ROUTED_AGENT"
    ROUTE_WARNING=""
    PROFILE_JSON='{}'
    SELECTED_SKILLS_CSV=""
    COMPLEXITY="medium"
    if [ -n "$ALLOWED_TOOLS_CSV" ]; then
      PROFILE_JSON=$(printf '%s' "$PROFILE_JSON" | jq -c --arg tools "$ALLOWED_TOOLS_CSV" '.tools = ($tools | split(",") | map(select(length > 0)))')
    fi

    db_task_update "$TASK_ID" \
      "agent=$ROUTED_AGENT" \
      "complexity=$COMPLEXITY" \
      "status=routed" \
      "route_reason=$REASON" \
      "agent_profile=$PROFILE_JSON" \
      "dir=$PROJECT_DIR"
    db_add_label "$TASK_ID" "agent:${ROUTED_AGENT}"

    append_history "$TASK_ID" "routed" "$REASON"
    log_err "[route] task=$TASK_ID fallback to ${ROUTED_AGENT}"
    echo "$ROUTED_AGENT"
    exit 0
  fi

  db_task_update "$TASK_ID" \
    "status=needs_review" \
    "last_error=router failed (exit ${CMD_STATUS})"
  append_history "$TASK_ID" "needs_review" "router failed (exit ${CMD_STATUS})"
  exit 0
fi

RESPONSE_JSON=$(normalize_json_response "$RESPONSE" 2>/dev/null || true)
if [ -z "$RESPONSE_JSON" ] || ! printf '%s' "$RESPONSE_JSON" | jq -e 'type=="object"' >/dev/null 2>&1; then
  log_err "[route] invalid JSON response"
  db_task_update "$TASK_ID" \
    "status=needs_review" \
    "last_error=router response invalid JSON" \
    "summary=Router error: invalid JSON response"
  db_set_blockers "$TASK_ID" "Router failed to return valid JSON"
  mkdir -p "$CONTEXTS_DIR"
  printf '%s' "$RESPONSE" > "${CONTEXTS_DIR}/route-response-${TASK_ID}.md"
  append_history "$TASK_ID" "needs_review" "router response invalid JSON"
  exit 0
fi

ROUTED_AGENT=$(printf '%s' "$RESPONSE_JSON" | jq -r '.executor // ""')
REASON=$(printf '%s' "$RESPONSE_JSON" | jq -r '.reason // ""')
PROFILE_JSON=$(printf '%s' "$RESPONSE_JSON" | jq -c '.profile // {}')
SELECTED_SKILLS_CSV=$(printf '%s' "$RESPONSE_JSON" | jq -r '.selected_skills // [] | join(",")')
COMPLEXITY=$(printf '%s' "$RESPONSE_JSON" | jq -r '.complexity // "medium"')

# Validate routed agent is installed
if [ -n "$ROUTED_AGENT" ] && ! command -v "$ROUTED_AGENT" >/dev/null 2>&1; then
  FIRST_AVAILABLE=$(printf '%s' "$AVAILABLE_AGENTS" | cut -d',' -f1)
  log_err "[route] $ROUTED_AGENT not installed, falling back to $FIRST_AVAILABLE"
  REASON="$REASON (router picked $ROUTED_AGENT but not installed; using $FIRST_AVAILABLE)"
  ROUTED_AGENT="$FIRST_AVAILABLE"
fi

# Apply static defaults
if [ -n "$ALLOWED_TOOLS_CSV" ]; then
  PROFILE_JSON=$(printf '%s' "$PROFILE_JSON" | jq -c --arg tools "$ALLOWED_TOOLS_CSV" '.tools = ($tools | split(",") | map(select(length > 0)))')
fi
if [ -n "$DEFAULT_SKILLS_CSV" ]; then
  if [ -n "$SELECTED_SKILLS_CSV" ]; then
    SELECTED_SKILLS_CSV="$SELECTED_SKILLS_CSV,$DEFAULT_SKILLS_CSV"
  else
    SELECTED_SKILLS_CSV="$DEFAULT_SKILLS_CSV"
  fi
fi

# Simple router sanity check
ROUTE_WARNING=""
LABELS_LOWER=$(printf '%s' "$TASK_LABELS" | tr '[:upper:]' '[:lower:]')
SKILLS=$(printf '%s' "$PROFILE_JSON" | jq -r '.skills // [] | join(",")')
SKILLS_LOWER=$(printf '%s' "$SKILLS" | tr '[:upper:]' '[:lower:]')

if printf '%s' "$LABELS_LOWER" | rg -q '(backend|api|database|db)'; then
  if [ "$ROUTED_AGENT" = "claude" ]; then
    ROUTE_WARNING="backend-labeled task routed to claude"
  fi
fi

if printf '%s' "$LABELS_LOWER" | rg -q '(docs|documentation|writing)'; then
  if [ "$ROUTED_AGENT" = "codex" ]; then
    ROUTE_WARNING="docs-labeled task routed to codex"
  fi
fi

if [ -z "$SKILLS_LOWER" ]; then
  ROUTE_WARNING="profile missing skills"
fi

ROLE=$(printf '%s' "$PROFILE_JSON" | jq -r '.role // "general"')

# Decompose: manual "plan" label
DECOMPOSE_LABEL=""
if printf '%s' "$LABELS_LOWER" | rg -q '(^|,)plan(,|$)'; then
  DECOMPOSE_LABEL="plan"
fi

# Update task in SQLite
ROUTE_WARNING_VAL="${ROUTE_WARNING:-NULL}"
db_task_update "$TASK_ID" \
  "agent=$ROUTED_AGENT" \
  "complexity=$COMPLEXITY" \
  "status=routed" \
  "route_reason=$REASON" \
  "route_warning=$ROUTE_WARNING_VAL" \
  "agent_profile=$PROFILE_JSON" \
  "dir=$PROJECT_DIR"

# Store selected skills
if [ -n "$SELECTED_SKILLS_CSV" ]; then
  db_set_selected_skills "$TASK_ID" "$SELECTED_SKILLS_CSV"
fi

# Add routing labels
db_add_label "$TASK_ID" "agent:${ROUTED_AGENT}"
if [ -n "$DECOMPOSE_LABEL" ]; then
  db_add_label "$TASK_ID" "$DECOMPOSE_LABEL"
fi

NOTE="routed to $ROUTED_AGENT (complexity: $COMPLEXITY)"
if [ -n "$ROUTE_WARNING" ]; then
  NOTE="$NOTE (warning: $ROUTE_WARNING)"
fi
append_history "$TASK_ID" "routed" "$NOTE"

log_err "[route] task=$TASK_ID routed to ${ROUTED_AGENT:-unknown}"
export TASK_AGENT="$ROUTED_AGENT"
run_hook on_task_routed

echo "$ROUTED_AGENT"

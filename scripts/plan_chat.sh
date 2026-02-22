#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
source "${SCRIPT_DIR}/lib.sh"
require_jq
PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"
export PROJECT_DIR
init_tasks_file
init_config_file
load_project_config

PLAN_TITLE="${1:-}"
PLAN_BODY="${2:-}"
PLAN_LABELS="${3:-}"

if [ -z "$PLAN_TITLE" ]; then
  echo "usage: plan_chat.sh \"title\" [\"body\"] [\"labels\"]" >&2
  exit 1
fi

# Shared REPL functions (call_llm, parse_response, history, sigint)
HISTORY_PREFIX="plan"
source "${SCRIPT_DIR}/chat_lib.sh"

cleanup() {
  cleanup_chat_lib
}
trap cleanup EXIT
trap 'handle_sigint' INT

# Gather current status context
gather_status() {
  "${SCRIPT_DIR}/status.sh" --json 2>/dev/null || echo '{}'
}

# Build and send a turn to the LLM
send_turn() {
  local user_msg="$1"

  STATUS_JSON=$(gather_status)
  CHAT_HISTORY=$(read_history)
  REPO_TREE=$(build_repo_tree "$PROJECT_DIR")
  PROJECT_INSTRUCTIONS=$(build_project_instructions "$PROJECT_DIR")
  export STATUS_JSON CHAT_HISTORY REPO_TREE PROJECT_INSTRUCTIONS
  export PLAN_TITLE PLAN_BODY PLAN_LABELS PROJECT_DIR

  local prompt
  prompt=$(render_template "${SCRIPT_DIR}/../prompts/plan_chat.md")

  local full_prompt
  full_prompt=$(printf '%s\n\nUser message: %s' "$prompt" "$user_msg")

  start_spinner "Thinking"
  local raw_response
  raw_response=$(call_llm "$full_prompt" 2>/dev/null || true)
  stop_spinner

  if [ -z "$raw_response" ]; then
    echo "Sorry, I couldn't get a response. Please try again."
    append_exchange "$user_msg" "(error: no response)"
    return 1
  fi

  local response_json
  response_json=$(parse_response "$raw_response" || true)

  if [ -z "$response_json" ]; then
    echo "$raw_response"
    append_exchange "$user_msg" "$raw_response"
    return 1
  fi

  local action llm_response params
  action=$(printf '%s' "$response_json" | jq -r '.action // "ask"')
  llm_response=$(printf '%s' "$response_json" | jq -r '.response // ""')
  params=$(printf '%s' "$response_json" | jq -c '.params // {}')

  if [ -n "$llm_response" ]; then
    echo "$llm_response"
  fi

  append_exchange "$user_msg" "[${action}] ${llm_response}"

  if [ "$action" = "create_tasks" ]; then
    create_planned_tasks "$params"
    return 99  # Signal to exit the loop
  fi

  return 0
}

# Create all planned tasks atomically
create_planned_tasks() {
  local params="$1"

  local tasks_json
  tasks_json=$(printf '%s' "$params" | jq -c '.tasks // []')

  local count
  count=$(printf '%s' "$tasks_json" | jq 'length')

  if [ "$count" -eq 0 ]; then
    echo "No tasks to create."
    return 0
  fi

  NOW=$(now_iso)
  export NOW PROJECT_DIR

  local i=0
  while [ "$i" -lt "$count" ]; do
    local title body labels suggested_agent
    title=$(printf '%s' "$tasks_json" | jq -r ".[$i].title // \"\"")
    body=$(printf '%s' "$tasks_json" | jq -r ".[$i].body // \"\"")
    labels=$(printf '%s' "$tasks_json" | jq -r ".[$i].labels // \"\"")
    suggested_agent=$(printf '%s' "$tasks_json" | jq -r ".[$i].suggested_agent // \"\"")

    # Merge plan labels with per-task labels
    local all_labels="$labels"
    if [ -n "$PLAN_LABELS" ]; then
      if [ -n "$all_labels" ]; then
        all_labels="${PLAN_LABELS},${all_labels}"
      else
        all_labels="$PLAN_LABELS"
      fi
    fi

    db_create_task "$title" "$body" "$PROJECT_DIR" "$all_labels" "" "$suggested_agent"

    i=$((i + 1))
  done

  echo ""
  echo "Created $count task(s)."
}

# --- Main ---
echo "orchestrator plan: ${PLAN_TITLE}"
echo "(type 'exit' to quit, approve the plan to create tasks)"
echo ""

# Auto-send first turn — LLM analyzes the request immediately
FIRST_MSG="Please analyze this plan request and propose subtasks or ask clarifying questions."
rc=0
send_turn "$FIRST_MSG" || rc=$?
if [ "$rc" -eq 99 ]; then
  exit 0
fi
echo ""

# Interactive REPL loop
while true; do
  printf '> '

  if ! IFS= read -r USER_INPUT; then
    echo ""
    break
  fi

  USER_INPUT=$(printf '%s' "$USER_INPUT" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')

  if [ -z "$USER_INPUT" ]; then
    continue
  fi

  case "$USER_INPUT" in
    exit|quit|q|bye)
      echo "Goodbye!"
      break
      ;;
  esac

  rc=0
  send_turn "$USER_INPUT" || rc=$?
  if [ "$rc" -eq 99 ]; then
    exit 0
  fi

  echo ""
done

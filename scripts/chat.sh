#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
source "${SCRIPT_DIR}/lib.sh"
require_jq
PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"
export PROJECT_DIR
init_tasks_file
init_jobs_file
init_config_file
load_project_config

# Shared REPL functions (call_llm, parse_response, history, sigint)
HISTORY_PREFIX="chat"
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

# Dispatch an action to the appropriate script
dispatch() {
  local action="$1"
  local params="$2"

  case "$action" in
    add_task)
      local title body labels
      title=$(printf '%s' "$params" | jq -r '.title // ""')
      body=$(printf '%s' "$params" | jq -r '.body // ""')
      labels=$(printf '%s' "$params" | jq -r '.labels // ""')
      "${SCRIPT_DIR}/add_task.sh" "$title" "$body" "$labels"
      ;;
    plan_task)
      local title body labels
      title=$(printf '%s' "$params" | jq -r '.title // ""')
      body=$(printf '%s' "$params" | jq -r '.body // ""')
      labels=$(printf '%s' "$params" | jq -r '.labels // ""')
      "${SCRIPT_DIR}/add_task.sh" "$title" "$body" "plan,${labels}"
      ;;
    retry)
      local id
      id=$(printf '%s' "$params" | jq -r '.id // ""')
      "${SCRIPT_DIR}/retry_task.sh" "$id"
      ;;
    unblock)
      local id
      id=$(printf '%s' "$params" | jq -r '.id // ""')
      if [ "$id" = "all" ]; then
        db_task_ids_by_status "blocked" | xargs -n1 "${SCRIPT_DIR}/retry_task.sh"
      else
        "${SCRIPT_DIR}/retry_task.sh" "$id"
      fi
      ;;
    add_job)
      local schedule title body labels agent
      schedule=$(printf '%s' "$params" | jq -r '.schedule // ""')
      title=$(printf '%s' "$params" | jq -r '.title // ""')
      body=$(printf '%s' "$params" | jq -r '.body // ""')
      labels=$(printf '%s' "$params" | jq -r '.labels // ""')
      agent=$(printf '%s' "$params" | jq -r '.agent // ""')
      "${SCRIPT_DIR}/jobs_add.sh" "$schedule" "$title" "$body" "$labels" "$agent"
      ;;
    list)
      "${SCRIPT_DIR}/list_tasks.sh"
      ;;
    status)
      "${SCRIPT_DIR}/status.sh"
      ;;
    dashboard)
      "${SCRIPT_DIR}/dashboard.sh"
      ;;
    tree)
      "${SCRIPT_DIR}/tree.sh"
      ;;
    jobs_list)
      "${SCRIPT_DIR}/jobs_list.sh"
      ;;
    run)
      local id
      id=$(printf '%s' "$params" | jq -r '.id // ""')
      if [ -n "$id" ] && [ "$id" != "null" ]; then
        "${SCRIPT_DIR}/run_task.sh" "$id"
      else
        "${SCRIPT_DIR}/next.sh"
      fi
      ;;
    set_agent)
      local id agent
      id=$(printf '%s' "$params" | jq -r '.id // ""')
      agent=$(printf '%s' "$params" | jq -r '.agent // ""')
      "${SCRIPT_DIR}/set_agent.sh" "$id" "$agent"
      ;;
    remove_job)
      local id
      id=$(printf '%s' "$params" | jq -r '.id // ""')
      "${SCRIPT_DIR}/jobs_remove.sh" "$id"
      ;;
    enable_job)
      local id
      id=$(printf '%s' "$params" | jq -r '.id // ""')
      db_job_set "$id" "enabled" "1"
      echo "Enabled job '$id'"
      ;;
    disable_job)
      local id
      id=$(printf '%s' "$params" | jq -r '.id // ""')
      db_job_set "$id" "enabled" "0"
      echo "Disabled job '$id'"
      ;;
    gh_sync|gh_pull|gh_push)
      echo "GitHub is the native backend — no sync needed."
      ;;
    gh_project_create)
      local title
      title=$(printf '%s' "$params" | jq -r '.title // ""')
      "${SCRIPT_DIR}/gh_project_create.sh" "$title"
      ;;
    quick_task)
      local prompt
      prompt=$(printf '%s' "$params" | jq -r '.prompt // ""')
      echo "Running quick task..."
      (cd "$PROJECT_DIR"
      case "$CHAT_AGENT" in
        claude)
          if [ -n "$CHAT_MODEL" ]; then
            claude --model "$CHAT_MODEL" --print "$prompt"
          else
            claude --print "$prompt"
          fi
          ;;
        codex)
          if [ -n "$CHAT_MODEL" ]; then
            codex exec --model "$CHAT_MODEL" "$prompt"
          else
            codex exec "$prompt"
          fi
          ;;
        opencode)
          opencode run "$prompt"
          ;;
      esac
      )
      ;;
    answer)
      # No dispatch needed — just show the response
      ;;
    *)
      echo "Unknown action: $action"
      ;;
  esac
}

# Persistent readline history
READLINE_HIST="${STATE_DIR}/chat_readline_history"
touch "$READLINE_HIST"
history -r "$READLINE_HIST"

# Main REPL loop
echo "orchestrator chat (type 'exit' to quit)"
echo ""

while true; do
  # Read input with readline (arrow keys, history)
  if ! IFS= read -e -p "> " USER_INPUT; then
    echo ""
    break
  fi

  # Trim whitespace
  USER_INPUT=$(printf '%s' "$USER_INPUT" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')

  # Skip empty input
  if [ -z "$USER_INPUT" ]; then
    continue
  fi

  # Save to readline history
  history -s "$USER_INPUT"
  history -w "$READLINE_HIST"

  # Exit commands
  case "$USER_INPUT" in
    exit|quit|q|bye)
      echo "Goodbye!"
      break
      ;;
  esac

  # Gather context
  STATUS_JSON=$(gather_status)
  CHAT_HISTORY=$(read_history)
  export STATUS_JSON CHAT_HISTORY PROJECT_DIR

  # Render prompt template
  PROMPT=$(render_template "${SCRIPT_DIR}/../prompts/chat.md")

  # Append user message to prompt
  FULL_PROMPT=$(printf '%s\n\nUser message: %s' "$PROMPT" "$USER_INPUT")

  # Call LLM
  start_spinner "Thinking"
  RAW_RESPONSE=$(call_llm "$FULL_PROMPT" 2>/dev/null || true)
  stop_spinner

  if [ -z "$RAW_RESPONSE" ]; then
    echo "Sorry, I couldn't get a response. Please try again."
    append_exchange "$USER_INPUT" "(error: no response)"
    continue
  fi

  # Parse response
  RESPONSE_JSON=$(parse_response "$RAW_RESPONSE" || true)

  if [ -z "$RESPONSE_JSON" ]; then
    # Graceful fallback: print raw response as text
    echo "$RAW_RESPONSE"
    append_exchange "$USER_INPUT" "$RAW_RESPONSE"
    continue
  fi

  ACTION=$(printf '%s' "$RESPONSE_JSON" | jq -r '.action // "answer"')
  PARAMS=$(printf '%s' "$RESPONSE_JSON" | jq -c '.params // {}')
  LLM_RESPONSE=$(printf '%s' "$RESPONSE_JSON" | jq -r '.response // ""')

  # Dispatch action (if not just an answer)
  CMD_OUTPUT=""
  if [ "$ACTION" != "answer" ]; then
    CMD_OUTPUT=$(dispatch "$ACTION" "$PARAMS" 2>&1 || true)
  fi

  # Show LLM response
  if [ -n "$LLM_RESPONSE" ]; then
    echo "$LLM_RESPONSE"
  fi

  # Show command output (if any)
  if [ -n "$CMD_OUTPUT" ]; then
    echo ""
    echo "$CMD_OUTPUT"
  fi

  # Record in history
  HISTORY_ENTRY="[${ACTION}] ${LLM_RESPONSE}"
  append_exchange "$USER_INPUT" "$HISTORY_ENTRY"

  echo ""
done

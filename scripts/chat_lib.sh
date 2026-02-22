#!/usr/bin/env bash
# Shared REPL functions for chat.sh and plan_chat.sh.
# Caller sets HISTORY_PREFIX (e.g. "chat" or "plan") before sourcing.
# Defines functions only — no REPL loop, no traps (caller sets those).

# --- Agent config ---
CHAT_AGENT=${CHAT_AGENT:-$(config_get '.router.agent // "claude"')}
CHAT_MODEL=${CHAT_MODEL:-$(config_get '.router.model // ""')}
require_agent "$CHAT_AGENT"

# --- Session history ---
HISTORY_PREFIX=${HISTORY_PREFIX:-"chat"}
ensure_state_dir
HISTORY_FILE="${STATE_DIR}/${HISTORY_PREFIX}-history-$$.txt"
touch "$HISTORY_FILE"

MAX_HISTORY=10

cleanup_chat_lib() {
  rm -f "$HISTORY_FILE"
  stop_spinner 2>/dev/null || true
}

# --- Double Ctrl-C exit ---
SIGINT_COUNT=0
SIGINT_LAST=0

handle_sigint() {
  local now
  now=$(date +%s)
  if [ $((now - SIGINT_LAST)) -le 2 ]; then
    echo ""
    echo "Goodbye!"
    exit 0
  fi
  SIGINT_COUNT=1
  SIGINT_LAST=$now
  echo ""
  echo "(Press Ctrl-C again to exit)"
  printf '> '
}

# --- History helpers ---
read_history() {
  if [ ! -s "$HISTORY_FILE" ]; then
    echo "(no prior messages)"
    return
  fi
  tail -n $((MAX_HISTORY * 2)) "$HISTORY_FILE"
}

append_exchange() {
  local user_msg="$1"
  local assistant_msg="$2"
  printf 'User: %s\nAssistant: %s\n' "$user_msg" "$assistant_msg" >> "$HISTORY_FILE"
}

# --- LLM call ---
call_llm() {
  local prompt="$1"
  local response=""
  local rc=0

  case "$CHAT_AGENT" in
    codex)
      if [ -n "$CHAT_MODEL" ]; then
        response=$(codex exec --model "$CHAT_MODEL" --json "$prompt") || rc=$?
      else
        response=$(codex exec --json "$prompt") || rc=$?
      fi
      ;;
    claude)
      if [ -n "$CHAT_MODEL" ]; then
        response=$(claude --model "$CHAT_MODEL" --output-format json --print "$prompt") || rc=$?
      else
        response=$(claude --output-format json --print "$prompt") || rc=$?
      fi
      ;;
    opencode)
      response=$(opencode run --format json "$prompt") || rc=$?
      ;;
    *)
      echo "Unknown chat agent: $CHAT_AGENT" >&2
      return 1
      ;;
  esac

  if [ "$rc" -ne 0 ]; then
    echo ""
    return "$rc"
  fi
  printf '%s' "$response"
}

# --- JSON response parsing ---
parse_response() {
  local raw="$1"
  local json=""

  # Try normalize_json_response first (handles Claude wrapper + fenced JSON)
  json=$(normalize_json_response "$raw" 2>/dev/null || true)

  if [ -z "$json" ] || ! printf '%s' "$json" | jq -e 'type=="object"' >/dev/null 2>&1; then
    # Fallback: maybe it's already valid JSON
    if printf '%s' "$raw" | jq -e 'type=="object"' >/dev/null 2>&1; then
      json="$raw"
    else
      echo ""
      return 1
    fi
  fi

  printf '%s' "$json"
}

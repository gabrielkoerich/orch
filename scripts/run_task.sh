#!/usr/bin/env bash
# shellcheck source=scripts/lib.sh
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
source "$SCRIPT_DIR/lib.sh"
require_jq
require_rg
init_config_file

PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"

# Detect missing tooling in agent output to avoid retry loops.
# Prints: "<tool>\t<kind>" (kind is "command not found" or "no such file")
detect_missing_tooling() {
  python3 -c '
import re
import sys

text = sys.stdin.read()
known = {
    # JS/TS
    "bun","node","npm","pnpm","yarn","deno","tsc","eslint","prettier","jest","vitest",
    # Rust/Go/Python
    "cargo","rustc","go","python","python3","pip","pip3","uv","poetry","pytest","ruff","black","mypy",
    # Build tools
    "make","cmake","ninja","just","bats",
    # Containers/infra
    "docker","docker-compose","podman","kubectl","helm","terraform",
    # Solana/Anchor
    "anchor","avm","solana","solana-test-validator",
}

patterns = [
    ("command not found", re.compile(r"(?im)^\s*(?:zsh|bash|sh|/bin/sh|env)(?:\[[0-9]+\])?:\s*(?:line\s+\d+:\s*)?command not found:\s*([^\s]+)\s*$")),
    ("command not found", re.compile(r"(?im)^\s*(?:bash|sh|/bin/sh)(?:\[[0-9]+\])?:\s*([^\s:]+):\s*command not found\s*$")),
    ("command not found", re.compile(r"(?im)^\s*(?:zsh|bash|sh|/bin/sh|env)(?:\[[0-9]+\])?:\s*(?:line\s+\d+:\s*)?([^\s:]+):\s*not found\s*$")),
    ("no such file", re.compile(r"(?im)^\s*env:\s*([^\s:]+):\s*No such file or directory\s*$")),
    ("no such file", re.compile(r"(?im)^\s*([^\s:]+):\s*No such file or directory\s*$")),
    ("no such file", re.compile(r"(?im)\bspawn\s+([^\s]+)\s+ENOENT\b")),
]

for kind, pat in patterns:
    for m in pat.finditer(text):
        cmd = m.group(1).strip().strip("\"").strip("\x27")
        base = cmd.rsplit("/", 1)[-1].lower()
        if base in known:
            sys.stdout.write(f"{base}\t{kind}")
            sys.exit(0)

sys.exit(1)
'
}

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

# Augment PATH (brew services / launchd start with minimal PATH)
# If $HOME/.path exists, source it to pick up user-configured paths;
# otherwise fall back to common dev tool locations.
if [[ -f "$HOME/.path" ]]; then
  _OLD_PATH="$PATH"
  source "$HOME/.path" >/dev/null 2>&1
  # Preserve any paths that were already at the front (e.g. test mocks)
  export PATH="${_OLD_PATH}:${PATH}"
else
  for _p in "$HOME/.bun/bin" "$HOME/.cargo/bin" "$HOME/.local/share/solana/install/active_release/bin" "$HOME/.local/bin" "/opt/homebrew/bin" "/usr/local/bin"; do
    [[ -d "$_p" ]] && [[ ":$PATH:" != *":$_p:"* ]] && export PATH="$_p:$PATH"
  done
fi

load_project_config

TASK_ID=${1:-}
if [ -z "$TASK_ID" ]; then
  TASK_ID=$(db_task_ids_by_status "new" | head -1)
  if [ -z "$TASK_ID" ]; then
    TASK_ID=$(db_task_ids_by_status "routed" | head -1)
  fi
  if [ -z "$TASK_ID" ]; then
    log_err "No runnable tasks found"
    exit 1
  fi
fi

log_err "[run] task=$TASK_ID starting"
export TASK_ID

# Per-task lock to avoid double-run across multiple watchers.
# Must be checked BEFORE the cleanup trap so failed lock attempts exit cleanly.
TASK_LOCK="${LOCK_PATH}.task.${TASK_ID}"
TASK_LOCK_OWNED=false
if ! mkdir "$TASK_LOCK" 2>/dev/null; then
  lock_pid=""
  if [ -f "$TASK_LOCK/pid" ]; then
    lock_pid=$(cat "$TASK_LOCK/pid" 2>/dev/null || true)
  fi
  if [ -n "$lock_pid" ] && kill -0 "$lock_pid" >/dev/null 2>&1; then
    exit 0
  fi
  if lock_is_stale "$TASK_LOCK"; then
    rm -f "$TASK_LOCK/pid"
    rmdir "$TASK_LOCK" 2>/dev/null || true
  fi
  if ! mkdir "$TASK_LOCK" 2>/dev/null; then
    exit 0
  fi
fi
TASK_LOCK_OWNED=true
echo "$$" > "$TASK_LOCK/pid"

# Combined cleanup: recover crashed tasks AND release per-task lock.
_run_task_cleanup() {
  local exit_code=$?

  if [ "$TASK_LOCK_OWNED" = true ]; then
    rm -f "$TASK_LOCK/pid"
    rmdir "$TASK_LOCK" 2>/dev/null || true
  fi

  if [ $exit_code -ne 0 ] && [ "$TASK_LOCK_OWNED" = true ]; then
    log_err "[run] task=$TASK_ID crashed (exit=$exit_code) at line ${BASH_LINENO[0]:-?}"
    local current_status
    current_status=$(db_task_field "$TASK_ID" "status" 2>/dev/null || true)
    if [ "$current_status" = "routed" ] || [ "$current_status" = "in_progress" ] || [ "$current_status" = "new" ]; then
      db_task_update "$TASK_ID" \
        "status=needs_review" \
        "last_error=run_task crashed (exit $exit_code)" 2>/dev/null || true
    fi
  fi
}
trap '_run_task_cleanup' EXIT

# Read task's dir field and override PROJECT_DIR if set
TASK_DIR_VAL=$(db_task_field "$TASK_ID" "dir")
if [ -n "$TASK_DIR_VAL" ] && [ "$TASK_DIR_VAL" != "null" ]; then
  if [ -d "$TASK_DIR_VAL" ]; then
    PROJECT_DIR="$TASK_DIR_VAL"
    export PROJECT_DIR
    load_project_config
  else
    log_err "[run] task=$TASK_ID dir=$TASK_DIR_VAL does not exist"
    append_history "$TASK_ID" "blocked" "task dir does not exist: $TASK_DIR_VAL"
    db_task_update "$TASK_ID" "status=blocked" "last_error=task dir does not exist: $TASK_DIR_VAL"
    exit 0
  fi
fi

# Load all task fields in one pass
load_task "$TASK_ID"

if [ -z "$TASK_TITLE" ] || [ "$TASK_TITLE" = "null" ]; then
  log_err "Task $TASK_ID not found"
  exit 1
fi

# Ensure TASK_STATUS is set before checking it (some backends may not export it)
TASK_STATUS="${TASK_STATUS:-$(db_task_field "$TASK_ID" "status" 2>/dev/null || true)}"

# Guard: never re-run tasks that need human review
if [ "$TASK_STATUS" = "needs_review" ]; then
  log_err "[run] task=$TASK_ID status=needs_review, skipping (requires human review before retry)"
  exit 0
fi

if [ -z "$TASK_AGENT" ] || [ "$TASK_AGENT" = "null" ]; then
  TASK_AGENT=$("$(dirname "$0")/route_task.sh" "$TASK_ID")
  load_task "$TASK_ID"
fi

if [ -z "$TASK_AGENT" ] || [ "$TASK_AGENT" = "null" ]; then
  log_err "[run] task=$TASK_ID missing agent after routing"
  mark_needs_review "$TASK_ID" "$ATTEMPTS" "router did not set agent"
  exit 0
fi

# Build GitHub issue reference for agent prompt
export GH_ISSUE_REF=""
if [ -n "${GH_ISSUE_NUMBER:-}" ] && [ "$GH_ISSUE_NUMBER" != "null" ] && [ "$GH_ISSUE_NUMBER" != "0" ]; then
  GH_REPO=$(config_get '.gh.repo // ""')
  if [ -n "$GH_REPO" ] && [ "$GH_REPO" != "null" ]; then
    GH_ISSUE_REF="#${GH_ISSUE_NUMBER} (${GH_REPO})"
  else
    GH_ISSUE_REF="#${GH_ISSUE_NUMBER}"
  fi
fi

# Fetch GitHub issue comments for agent context
export ISSUE_COMMENTS=""
if [ -n "${GH_ISSUE_NUMBER:-}" ] && [ "$GH_ISSUE_NUMBER" != "null" ] && [ "$GH_ISSUE_NUMBER" != "0" ]; then
  GH_REPO=$(config_get '.gh.repo // ""')
  if [ -n "$GH_REPO" ] && [ "$GH_REPO" != "null" ]; then
    ISSUE_COMMENTS=$(fetch_issue_comments "$GH_REPO" "$GH_ISSUE_NUMBER" 10)
  fi
fi

# Resolve PROJECT_DIR to the main repo if it's inside a worktree
# This prevents nested worktrees when subtasks inherit parent's worktree dir
_MAIN_WT=$(git -C "$PROJECT_DIR" worktree list --porcelain 2>/dev/null | head -1 | sed 's/^worktree //' || true)
if [ -n "$_MAIN_WT" ] && [ "$_MAIN_WT" != "$PROJECT_DIR" ]; then
  log_err "[run] task=$TASK_ID resolving worktree dir to main repo: $PROJECT_DIR → $_MAIN_WT"
  PROJECT_DIR="$_MAIN_WT"
  export PROJECT_DIR
fi

# Save the main project dir before worktree override
MAIN_PROJECT_DIR="$PROJECT_DIR"
export MAIN_PROJECT_DIR

# Set up worktree for coding tasks
export WORKTREE_DIR=""
DECOMPOSE_VAL=$(db_task_field "$TASK_ID" "decompose")
if [ "$DECOMPOSE_VAL" = "1" ] || [ "$DECOMPOSE_VAL" = "true" ]; then
  log_err "[run] task=$TASK_ID decompose=true (planning task)"
fi

# Always create a worktree — never work in the main project dir
SAVED_BRANCH="$TASK_BRANCH"
SAVED_WORKTREE="$TASK_WORKTREE"
PROJECT_NAME=$(basename "$PROJECT_DIR" .git)

# Project-local worktrees: stored inside the project at .orchestrator/worktrees/
# Falls back to the old global location for backward compatibility
WORKTREES_BASE="${MAIN_PROJECT_DIR}/.orchestrator/worktrees"
mkdir -p "$WORKTREES_BASE"

if [ -n "$SAVED_BRANCH" ] && [ "$SAVED_BRANCH" != "null" ]; then
  BRANCH_NAME="$SAVED_BRANCH"
  if [ -n "$SAVED_WORKTREE" ] && [ "$SAVED_WORKTREE" != "null" ] && [ -d "$SAVED_WORKTREE" ]; then
    WORKTREE_DIR="$SAVED_WORKTREE"
  else
    WORKTREE_DIR="${WORKTREES_BASE}/${BRANCH_NAME}"
  fi
else
  # Try to find an existing worktree by issue/task prefix
  EXISTING_WT=""
  # Check project-local location first, then fall back to old global location
  for _wt_search in "$WORKTREES_BASE" "${ORCH_WORKTREES}/${PROJECT_NAME}"; do
    [ -d "$_wt_search" ] || continue
    if [ -n "${GH_ISSUE_NUMBER:-}" ] && [ "$GH_ISSUE_NUMBER" != "null" ] && [ "$GH_ISSUE_NUMBER" != "0" ]; then
      EXISTING_WT=$(fd -g "gh-task-${GH_ISSUE_NUMBER}-*" --max-depth 1 --type d "$_wt_search" 2>/dev/null | head -1 || true)
    fi
    if [ -z "$EXISTING_WT" ]; then
      EXISTING_WT=$(fd -g "task-${TASK_ID}-*" --max-depth 1 --type d "$_wt_search" 2>/dev/null | head -1 || true)
    fi
    [ -n "$EXISTING_WT" ] && break
  done

  if [ -n "$EXISTING_WT" ]; then
    BRANCH_NAME=$(basename "$EXISTING_WT")
    WORKTREE_DIR="$EXISTING_WT"
    log_err "[run] task=$TASK_ID found existing worktree: $WORKTREE_DIR"
  else
    BRANCH_SLUG=$(printf '%s' "$TASK_TITLE" | tr '[:upper:]' '[:lower:]' | tr -cs '[:alnum:]' '-' | sed 's/^-//;s/-$//' | head -c 40)
    if [ -n "${GH_ISSUE_NUMBER:-}" ] && [ "$GH_ISSUE_NUMBER" != "null" ] && [ "$GH_ISSUE_NUMBER" != "0" ]; then
      BRANCH_NAME="gh-task-${GH_ISSUE_NUMBER}-${BRANCH_SLUG}"
    else
      BRANCH_NAME="task-${TASK_ID}-${BRANCH_SLUG}"
    fi
    WORKTREE_DIR="${WORKTREES_BASE}/${BRANCH_NAME}"
  fi
fi
export BRANCH_NAME

# Guard: never create a worktree with an empty branch name
if [ -z "$BRANCH_NAME" ]; then
  log_err "[run] task=$TASK_ID ERROR: empty branch name, cannot create worktree"
  mark_needs_review "$TASK_ID" "$ATTEMPTS" "empty branch name — cannot create worktree"
  exit 0
fi

# Detect default branch (bare repos may use a different name)
DEFAULT_BRANCH=$(git -C "$PROJECT_DIR" symbolic-ref --short HEAD 2>/dev/null || echo "main")

if [ ! -d "$WORKTREE_DIR" ]; then
  log_err "[run] task=$TASK_ID creating worktree at $WORKTREE_DIR"
  if is_bare_repo "$PROJECT_DIR"; then
    git -C "$PROJECT_DIR" fetch --all --prune 2>/dev/null || true
  elif [ -n "${GH_ISSUE_NUMBER:-}" ] && [ "$GH_ISSUE_NUMBER" != "null" ] && [ "$GH_ISSUE_NUMBER" != "0" ]; then
    cd "$PROJECT_DIR" && gh issue develop "$GH_ISSUE_NUMBER" --base "$DEFAULT_BRANCH" --name "$BRANCH_NAME" 2>/dev/null || true
  fi
  cd "$PROJECT_DIR" && git branch "$BRANCH_NAME" "$DEFAULT_BRANCH" 2>/dev/null || true
  mkdir -p "$(dirname "$WORKTREE_DIR")"
  cd "$PROJECT_DIR" && git worktree add "$WORKTREE_DIR" "$BRANCH_NAME" 2>/dev/null || true
  run_hook on_worktree_created
fi

if [ -d "$WORKTREE_DIR" ]; then
  PROJECT_DIR="$WORKTREE_DIR"
  export PROJECT_DIR
  export WORKTREE_DIR BRANCH_NAME
  db_task_update "$TASK_ID" "worktree=$WORKTREE_DIR" "branch=$BRANCH_NAME"
  log_err "[run] task=$TASK_ID agent will run in worktree $WORKTREE_DIR"
else
  # Retry: prune and recreate
  log_err "[run] task=$TASK_ID worktree creation failed, retrying"
  cd "$PROJECT_DIR" && git worktree prune 2>/dev/null || true
  cd "$PROJECT_DIR" && git branch -D "$BRANCH_NAME" 2>/dev/null || true
  cd "$PROJECT_DIR" && git branch "$BRANCH_NAME" "$DEFAULT_BRANCH" 2>/dev/null || true
  cd "$PROJECT_DIR" && git worktree add "$WORKTREE_DIR" "$BRANCH_NAME" 2>/dev/null || true
  if [ -d "$WORKTREE_DIR" ]; then
    PROJECT_DIR="$WORKTREE_DIR"
    export PROJECT_DIR
    export WORKTREE_DIR BRANCH_NAME
    db_task_update "$TASK_ID" "worktree=$WORKTREE_DIR" "branch=$BRANCH_NAME"
    log_err "[run] task=$TASK_ID worktree created on retry: $WORKTREE_DIR"
  else
    log_err "[run] task=$TASK_ID worktree creation failed, blocking task"
    append_history "$TASK_ID" "blocked" "worktree creation failed for $WORKTREE_DIR"
    db_task_update "$TASK_ID" "status=blocked" "last_error=worktree creation failed: $WORKTREE_DIR"
    exit 0
  fi
fi

# Extract error history and last_error for agent context
export TASK_HISTORY=""
TASK_HISTORY=$(db_task_history_formatted "$TASK_ID" 5 2>/dev/null || true)
export TASK_LAST_ERROR
TASK_LAST_ERROR=$(db_task_field "$TASK_ID" "last_error")

# Merge required skills into selected skills
REQUIRED_SKILLS_CSV=$(config_get '.workflow.required_skills // [] | join(",")')
if [ -n "$REQUIRED_SKILLS_CSV" ]; then
  if [ -n "${SELECTED_SKILLS:-}" ]; then
    SELECTED_SKILLS="${SELECTED_SKILLS},${REQUIRED_SKILLS_CSV}"
  else
    SELECTED_SKILLS="$REQUIRED_SKILLS_CSV"
  fi
fi

# Build context enrichment
export TASK_CONTEXT
TASK_CONTEXT=$(load_task_context "$TASK_ID" "$ROLE")
export PARENT_CONTEXT
PARENT_CONTEXT=$(build_parent_context "$TASK_ID")
export PROJECT_INSTRUCTIONS
PROJECT_INSTRUCTIONS=$(build_project_instructions "$PROJECT_DIR")
export SKILLS_DOCS
SKILLS_DOCS=$(build_skills_docs "${SELECTED_SKILLS:-}")
export REPO_TREE
REPO_TREE=$(build_repo_tree "$PROJECT_DIR")
export GIT_DIFF
if [ "$ATTEMPTS" -gt 0 ]; then
  GIT_DIFF=$(build_git_diff "$PROJECT_DIR" "$DEFAULT_BRANCH")
else
  GIT_DIFF=""
fi

# Output file for agentic mode
mkdir -p "${PROJECT_DIR}/.orchestrator"
OUTPUT_FILE="${PROJECT_DIR}/.orchestrator/output-${TASK_ID}.json"
rm -f "$OUTPUT_FILE"
export OUTPUT_FILE

ATTEMPTS=$((ATTEMPTS + 1))
NOW=$(now_iso)
export NOW ATTEMPTS

# Check max attempts before starting
MAX=$(max_attempts)

# Detect retry loops: if 4+ attempts and last 3 blocked comments have identical text, stop
if [ "$ATTEMPTS" -ge 4 ]; then
  # Strip timestamp prefix (e.g. "[2026-01-01T00:00:00Z] ") before comparing for uniqueness
  _COMMENTS_JSON=$(gh_api -X GET "repos/$(_gh_ensure_repo 2>/dev/null; echo "$_GH_REPO")/issues/$TASK_ID/comments" -f per_page=100 2>/dev/null || echo '[]')
  BLOCKED_NOTES=$(printf '%s' "$_COMMENTS_JSON" \
    | jq -r '[.[] | select(.body | test("blocked:"; "i"))] | .[-3:] | [.[] | (.body | sub("^\\[\\d{4}-[^]]+\\] "; ""))] | unique | length' 2>/dev/null || echo "0")
  BLOCKED_COUNT=$(printf '%s' "$_COMMENTS_JSON" \
    | jq '[.[] | select(.body | test("blocked:"; "i"))] | .[-3:] | length' 2>/dev/null || echo "0")
  if [ "$BLOCKED_COUNT" -ge 3 ] && [ "$BLOCKED_NOTES" -eq 1 ]; then
    log_err "[run] task=$TASK_ID retry loop detected (same error 3x)"
    mark_needs_review "$TASK_ID" "$ATTEMPTS" "retry loop: same error repeated 3 times"
    exit 0
  fi
fi

if [ "$ATTEMPTS" -gt "$MAX" ]; then
  log_err "[run] task=$TASK_ID exceeded max attempts ($MAX)"
  db_task_update "$TASK_ID" \
    "status=needs_review" \
    "reason=exceeded max attempts ($MAX)" \
    "last_error=max attempts exceeded"
  append_history "$TASK_ID" "needs_review" "exceeded max attempts ($MAX)"
  exit 0
fi

# Preflight: validate required tools exist on PATH before launching the agent.
# Configure per project in .orchestrator.yml (orchestrator.yml) as:
#   required_tools: [bun, anchor, solana-test-validator]
REQUIRED_TOOLS_CSV=$(config_get '.required_tools // [] | map(select(. != null and . != "")) | join(",")' 2>/dev/null || true)
if [ -n "$REQUIRED_TOOLS_CSV" ] && [ "$REQUIRED_TOOLS_CSV" != "null" ]; then
  IFS=',' read -ra _req_tools <<< "$REQUIRED_TOOLS_CSV"
  _missing_tools=()
  for _tool in "${_req_tools[@]}"; do
    [ -z "$_tool" ] && continue
    if ! command -v "$_tool" >/dev/null 2>&1; then
      _missing_tools+=("$_tool")
    fi
  done
  if [ "${#_missing_tools[@]}" -gt 0 ]; then
    _missing_csv=$(IFS=', '; echo "${_missing_tools[*]}")
    _reason="missing required tools on PATH: ${_missing_csv} (configure required_tools in .orchestrator.yml or install them)"
    log_err "[run] task=$TASK_ID blocked: $_reason"
    db_task_update "$TASK_ID" \
      "status=blocked" \
      "reason=$_reason" \
      "last_error=$_reason" \
      "attempts=$ATTEMPTS"
    append_history "$TASK_ID" "blocked" "$_reason"
    exit 0
  fi
fi

db_task_update "$TASK_ID" "status=in_progress" "attempts=$ATTEMPTS"
append_history "$TASK_ID" "in_progress" "started attempt $ATTEMPTS"

# Detect decompose/plan mode
DECOMPOSE=false
LABELS_LOWER=$(printf '%s' "$TASK_LABELS" | tr '[:upper:]' '[:lower:]')
if printf '%s' "$LABELS_LOWER" | rg -q '(^|,)plan(,|$)'; then
  DECOMPOSE=true
fi

# Build system prompt and agent message
if [ "$DECOMPOSE" = true ] && [ "$ATTEMPTS" -le 1 ]; then
  log_err "[run] task=$TASK_ID using plan/decompose mode"
  SYSTEM_PROMPT=$(render_template "$SCRIPT_DIR/../prompts/plan.md")
else
  SYSTEM_PROMPT=$(render_template "$SCRIPT_DIR/../prompts/system.md")
fi
AGENT_MESSAGE=$(render_template "$SCRIPT_DIR/../prompts/agent.md")

require_agent "$TASK_AGENT"

# Resolve agent/task timeout (seconds) unless explicitly overridden via env.
# Priority: env AGENT_TIMEOUT_SECONDS > workflow.timeout_by_complexity > workflow.timeout_seconds > default.
if [ -z "${AGENT_TIMEOUT_SECONDS:-}" ] || [ "${AGENT_TIMEOUT_SECONDS:-}" = "null" ]; then
  AGENT_TIMEOUT_SECONDS=$(task_timeout_seconds "${TASK_COMPLEXITY:-medium}")
  export AGENT_TIMEOUT_SECONDS
fi
if [ "${AGENT_TIMEOUT_SECONDS}" = "0" ]; then
  log_err "[run] task=$TASK_ID timeout=disabled (complexity=${TASK_COMPLEXITY:-medium})"
else
  log_err "[run] task=$TASK_ID timeout=${AGENT_TIMEOUT_SECONDS}s (complexity=${TASK_COMPLEXITY:-medium})"
fi

# Build disallowed tools list
DISALLOWED_TOOLS=$(config_get '.workflow.disallowed_tools // ["Bash(rm *)","Bash(rm -*)"] | join(",")')

# Sandbox: block agent access to the main project directory
SANDBOX_ENABLED=$(config_get '.workflow.sandbox // true')
if [ "$SANDBOX_ENABLED" = "true" ] && [ -n "$MAIN_PROJECT_DIR" ] && [ "$PROJECT_DIR" != "$MAIN_PROJECT_DIR" ]; then
  SANDBOX_PATTERNS="Bash(cd ${MAIN_PROJECT_DIR}*),Read(${MAIN_PROJECT_DIR}/*),Write(${MAIN_PROJECT_DIR}/*),Edit(${MAIN_PROJECT_DIR}/*)"
  if [ -n "$DISALLOWED_TOOLS" ]; then
    DISALLOWED_TOOLS="${DISALLOWED_TOOLS},${SANDBOX_PATTERNS}"
  else
    DISALLOWED_TOOLS="$SANDBOX_PATTERNS"
  fi
  log_err "[run] task=$TASK_ID sandbox enabled: blocking access to $MAIN_PROJECT_DIR"
fi

# Save prompt for debugging
ensure_state_dir
RUN_DATE=$(date -u +"%Y%m%d-%H%M%S")
FILE_PREFIX="${RUN_DATE}-task-${TASK_ID}-${TASK_AGENT}"
PROMPT_FILE="${STATE_DIR}/${FILE_PREFIX}-prompt-${ATTEMPTS}.txt"
printf '=== SYSTEM PROMPT ===\n%s\n\n=== AGENT MESSAGE ===\n%s\n' "$SYSTEM_PROMPT" "$AGENT_MESSAGE" > "$PROMPT_FILE"
PROMPT_HASH=$(shasum -a 256 "$PROMPT_FILE" | cut -c1-8)
export PROMPT_HASH
db_task_set "$TASK_ID" "prompt_hash" "$PROMPT_HASH"
log_err "[run] task=$TASK_ID prompt saved to $PROMPT_FILE (hash=$PROMPT_HASH)"
log_err "[run] task=$TASK_ID agent=$TASK_AGENT model=${AGENT_MODEL:-default} attempt=$ATTEMPTS project=$PROJECT_DIR"
log_err "[run] task=$TASK_ID skills=${SELECTED_SKILLS:-none} issue=${GH_ISSUE_REF:-none}"

run_hook on_task_started
start_spinner "Running task $TASK_ID ($TASK_AGENT)"
AGENT_START=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

RESPONSE=""
STDERR_FILE="${STATE_DIR}/${FILE_PREFIX}-stderr-${ATTEMPTS}.txt"
: > "$STDERR_FILE"

# Monitor stderr in background for stuck agent indicators
MONITOR_PID=""
MONITOR_INTERVAL="${MONITOR_INTERVAL:-10}"
(
  while true; do
    sleep "$MONITOR_INTERVAL"
    [ -f "$STDERR_FILE" ] || continue
    STDERR_SIZE=$(wc -c < "$STDERR_FILE" 2>/dev/null || echo 0)
    if [ "$STDERR_SIZE" -gt 0 ]; then
      if rg -qi 'waiting.*approv|passphrase|unlock|1password|biometric|touch.id|press.*button|enter.*password|interactive.*auth|permission.*denied.*publickey|sign_and_send_pubkey' "$STDERR_FILE" 2>/dev/null; then
        error_log "[run] task=$TASK_ID WARNING: agent may be stuck waiting for interactive approval"
        error_log "[run] task=$TASK_ID stderr: $(tail -c 300 "$STDERR_FILE")"
      fi
    fi
  done
) &
MONITOR_PID=$!
cleanup_monitor() { kill "$MONITOR_PID" 2>/dev/null || true; wait "$MONITOR_PID" 2>/dev/null || true; }

# Set git identity for agent commits
export GIT_AUTHOR_NAME="${TASK_AGENT}[bot]"
export GIT_COMMITTER_NAME="${TASK_AGENT}[bot]"
export GIT_AUTHOR_EMAIL="${TASK_AGENT}[bot]@users.noreply.github.com"
export GIT_COMMITTER_EMAIL="${TASK_AGENT}[bot]@users.noreply.github.com"

# Resolve model from complexity + config model_map
if [ -z "$AGENT_MODEL" ] || [ "$AGENT_MODEL" = "null" ]; then
  AGENT_MODEL=$(model_for_complexity "$TASK_AGENT" "${TASK_COMPLEXITY:-medium}")
fi

# Set model: label on the issue for visibility (diagnostic, not blocking)
if [ -n "$AGENT_MODEL" ] && [ "$AGENT_MODEL" != "null" ]; then
  _gh_set_prefixed_label "$TASK_ID" "$_GH_MODEL_PREFIX" "$AGENT_MODEL" 2>/dev/null || true
fi

CMD_STATUS=0
TMUX_RESPONSE_FILE="${STATE_DIR}/${FILE_PREFIX}-tmux-response-${ATTEMPTS}.txt"
TMUX_STATUS_FILE="${STATE_DIR}/${FILE_PREFIX}-tmux-status-${ATTEMPTS}.txt"
TMUX_SESSION="orch-${TASK_ID}"
USE_TMUX=${USE_TMUX:-$(config_get '.workflow.use_tmux // "true"')}

# Wait for tmux session to finish with a timeout
# Usage: tmux_wait <session_name> <timeout_seconds>
tmux_wait() {
  local session="$1"
  local timeout="${2:-${AGENT_TIMEOUT_SECONDS:-1800}}"
  local elapsed=0
  while tmux has-session -t "$session" 2>/dev/null; do
    sleep 5
    if [ "$timeout" != "0" ]; then
      elapsed=$((elapsed + 5))
      if [ "$elapsed" -ge "$timeout" ]; then
        log_err "[run] task=$TASK_ID tmux session timed out after ${timeout}s"
        tmux kill-session -t "$session" 2>/dev/null || true
        CMD_STATUS=124
        return 1
      fi
    fi
  done
  return 0
}

# Build agent command into a runner script for tmux
RUNNER_SCRIPT="${STATE_DIR}/${FILE_PREFIX}-runner-${ATTEMPTS}.sh"

case "$TASK_AGENT" in
  claude)
    log_err "[run] cmd: claude -p ${AGENT_MODEL:+--model $AGENT_MODEL} --permission-mode bypassPermissions --output-format json --append-system-prompt <prompt> <message>"
    DISALLOW_ARGS=()
    if [ -n "$DISALLOWED_TOOLS" ]; then
      IFS=',' read -ra _tools <<< "$DISALLOWED_TOOLS"
      for _t in "${_tools[@]}"; do
        DISALLOW_ARGS+=(--disallowedTools "$_t")
      done
    fi
    ALLOW_ARGS=()
    ALLOWED_TOOLS=$(config_get '.agents.claude.allowed_tools // [] | join(",")' 2>/dev/null || true)
    if [ -n "$ALLOWED_TOOLS" ]; then
      IFS=',' read -ra _atools <<< "$ALLOWED_TOOLS"
      for _t in "${_atools[@]}"; do
        ALLOW_ARGS+=(--allowedTools "$_t")
      done
    fi

    if [ "$USE_TMUX" = "true" ] && command -v tmux >/dev/null 2>&1; then
      # Write prompt content to temp files to avoid shell injection from task data
      PROMPT_SYS_FILE="${STATE_DIR}/${FILE_PREFIX}-sys-prompt-${ATTEMPTS}.txt"
      PROMPT_MSG_FILE="${STATE_DIR}/${FILE_PREFIX}-message-${ATTEMPTS}.txt"
      TOOL_ARGS_FILE="${STATE_DIR}/${FILE_PREFIX}-tool-args-${ATTEMPTS}.txt"
      printf '%s' "$SYSTEM_PROMPT" > "$PROMPT_SYS_FILE"
      printf '%s' "$AGENT_MESSAGE" > "$PROMPT_MSG_FILE"
      # Write tool args one per line for safe reading
      {
        for _a in ${ALLOW_ARGS[@]+"${ALLOW_ARGS[@]}"}; do printf '%s\n' "$_a"; done
        for _d in ${DISALLOW_ARGS[@]+"${DISALLOW_ARGS[@]}"}; do printf '%s\n' "$_d"; done
      } > "$TOOL_ARGS_FILE"

      # Use quoted heredoc to prevent variable expansion in the script body
      cat > "$RUNNER_SCRIPT" <<'RUNNER_EOF'
#!/usr/bin/env bash
set -euo pipefail
RUNNER_EOF
      # Append environment setup (safe values only — no user-controlled data)
      cat >> "$RUNNER_SCRIPT" <<RUNNER_ENV
export PATH="$PATH"
export GIT_AUTHOR_NAME="$GIT_AUTHOR_NAME"
export GIT_COMMITTER_NAME="$GIT_COMMITTER_NAME"
export GIT_AUTHOR_EMAIL="$GIT_AUTHOR_EMAIL"
export GIT_COMMITTER_EMAIL="$GIT_COMMITTER_EMAIL"
export ORCH_HOME="$ORCH_HOME"
cd "$PROJECT_DIR"
TOOL_ARGS=()
while IFS= read -r arg; do
  [ -n "\$arg" ] && TOOL_ARGS+=("\$arg")
done < "$TOOL_ARGS_FILE"
claude -p \
  ${AGENT_MODEL:+--model "$AGENT_MODEL"} \
  --permission-mode bypassPermissions \
  \${TOOL_ARGS[@]+"\${TOOL_ARGS[@]}"} \
  --output-format json \
  --append-system-prompt "\$(cat "$PROMPT_SYS_FILE")" \
  "\$(cat "$PROMPT_MSG_FILE")" \
  > "$TMUX_RESPONSE_FILE" 2>"$STDERR_FILE"
echo \$? > "$TMUX_STATUS_FILE"
RUNNER_ENV
      chmod +x "$RUNNER_SCRIPT"

      # Kill any existing session for this task
      tmux kill-session -t "$TMUX_SESSION" 2>/dev/null || true

      # Start tmux session with the runner
      tmux new-session -d -s "$TMUX_SESSION" -x 200 -y 50 "$RUNNER_SCRIPT"
      log_err "[run] task=$TASK_ID tmux session started: $TMUX_SESSION (attach: orch task attach $TASK_ID)"
      run_hook on_agent_session_start

      # Wait for the session to complete (with timeout)
      tmux_wait "$TMUX_SESSION" "${AGENT_TIMEOUT_SECONDS:-1800}" || true
      run_hook on_agent_session_end

      # Read response from file
      if [ -f "$TMUX_RESPONSE_FILE" ]; then
        RESPONSE=$(cat "$TMUX_RESPONSE_FILE")
      fi
      if [ -f "$TMUX_STATUS_FILE" ]; then
        CMD_STATUS=$(cat "$TMUX_STATUS_FILE")
      fi
    else
      # No tmux — run directly (original behavior)
      RESPONSE=$(cd "$PROJECT_DIR" && run_with_timeout claude -p \
        ${AGENT_MODEL:+--model "$AGENT_MODEL"} \
        --permission-mode bypassPermissions \
        ${ALLOW_ARGS[@]+"${ALLOW_ARGS[@]}"} \
        ${DISALLOW_ARGS[@]+"${DISALLOW_ARGS[@]}"} \
        --output-format json \
        --append-system-prompt "$SYSTEM_PROMPT" \
        "$AGENT_MESSAGE" 2>"$STDERR_FILE") || CMD_STATUS=$?
    fi
    ;;
  codex)
    log_err "[run] cmd: codex ${AGENT_MODEL:+-m $AGENT_MODEL} --ask-for-approval never --sandbox <mode> exec --json <stdin>"
    FULL_MESSAGE="${SYSTEM_PROMPT}

${AGENT_MESSAGE}"
    CODEX_SANDBOX=${CODEX_SANDBOX:-$(config_get '.agents.codex.sandbox // "full-auto"')}
    CODEX_ARGS=()
    # Orchestrator runs Codex non-interactively; approvals will never be granted, so disable them.
    CODEX_ARGS+=(--ask-for-approval never)
    case "$CODEX_SANDBOX" in
      full-auto)
        # Avoid `--full-auto` (implies `--ask-for-approval on-request`); keep sandboxed execution.
        CODEX_ARGS+=(--sandbox workspace-write)
        CODEX_ARGS+=(-c 'sandbox_workspace_write.network_access=true')
        CODEX_ARGS+=(-c 'sandbox_permissions=["disk-full-read-access"]')
        CODEX_ARGS+=(-c 'shell_environment_policy.inherit=all')
        ;;
      workspace-write)
        CODEX_ARGS+=(--sandbox workspace-write)
        CODEX_ARGS+=(-c 'sandbox_workspace_write.network_access=true')
        CODEX_ARGS+=(-c 'sandbox_permissions=["disk-full-read-access"]')
        CODEX_ARGS+=(-c 'shell_environment_policy.inherit=all')
        ;;
      danger-full-access)
        CODEX_ARGS+=(--sandbox danger-full-access)
        ;;
      none)
        CODEX_ARGS+=(--dangerously-bypass-approvals-and-sandbox)
        ;;
    esac

    if [ "$USE_TMUX" = "true" ] && command -v tmux >/dev/null 2>&1; then
      PROMPT_INPUT_FILE="${STATE_DIR}/${FILE_PREFIX}-codex-input-${ATTEMPTS}.txt"
      printf '%s' "$FULL_MESSAGE" > "$PROMPT_INPUT_FILE"
      cat > "$RUNNER_SCRIPT" <<RUNNER_EOF
#!/usr/bin/env bash
set -euo pipefail
export PATH="$PATH"
export GIT_AUTHOR_NAME="$GIT_AUTHOR_NAME"
export GIT_COMMITTER_NAME="$GIT_COMMITTER_NAME"
export GIT_AUTHOR_EMAIL="$GIT_AUTHOR_EMAIL"
export GIT_COMMITTER_EMAIL="$GIT_COMMITTER_EMAIL"
cd "$PROJECT_DIR"
cat "$PROMPT_INPUT_FILE" | codex \
  ${AGENT_MODEL:+-m "$AGENT_MODEL"} \
  $(printf '%s ' ${CODEX_ARGS[@]+"${CODEX_ARGS[@]}"}) \
  exec --json - \
  > "$TMUX_RESPONSE_FILE" 2>"$STDERR_FILE"
echo \$? > "$TMUX_STATUS_FILE"
RUNNER_EOF
      chmod +x "$RUNNER_SCRIPT"
      tmux kill-session -t "$TMUX_SESSION" 2>/dev/null || true
      tmux new-session -d -s "$TMUX_SESSION" -x 200 -y 50 "$RUNNER_SCRIPT"
      log_err "[run] task=$TASK_ID tmux session started: $TMUX_SESSION"

      tmux_wait "$TMUX_SESSION" "${AGENT_TIMEOUT_SECONDS:-1800}" || true

      [ -f "$TMUX_RESPONSE_FILE" ] && RESPONSE=$(cat "$TMUX_RESPONSE_FILE")
      [ -f "$TMUX_STATUS_FILE" ] && CMD_STATUS=$(cat "$TMUX_STATUS_FILE")
    else
      RESPONSE=$(cd "$PROJECT_DIR" && printf '%s' "$FULL_MESSAGE" | run_with_timeout codex \
        ${AGENT_MODEL:+-m "$AGENT_MODEL"} \
        ${CODEX_ARGS[@]+"${CODEX_ARGS[@]}"} \
        exec --json - \
        2>"$STDERR_FILE") || CMD_STATUS=$?
    fi
    ;;
  opencode)
    log_err "[run] cmd: opencode run ${AGENT_MODEL:+-m $AGENT_MODEL} --format json <message>"
    FULL_MESSAGE="${SYSTEM_PROMPT}

${AGENT_MESSAGE}"

    if [ "$USE_TMUX" = "true" ] && command -v tmux >/dev/null 2>&1; then
      PROMPT_INPUT_FILE="${STATE_DIR}/${FILE_PREFIX}-opencode-input-${ATTEMPTS}.txt"
      printf '%s' "$FULL_MESSAGE" > "$PROMPT_INPUT_FILE"
      cat > "$RUNNER_SCRIPT" <<RUNNER_EOF
#!/usr/bin/env bash
set -euo pipefail
export PATH="$PATH"
export GIT_AUTHOR_NAME="$GIT_AUTHOR_NAME"
export GIT_COMMITTER_NAME="$GIT_COMMITTER_NAME"
export GIT_AUTHOR_EMAIL="$GIT_AUTHOR_EMAIL"
export GIT_COMMITTER_EMAIL="$GIT_COMMITTER_EMAIL"
cd "$PROJECT_DIR"
opencode run \
  ${AGENT_MODEL:+-m "$AGENT_MODEL"} \
  --format json \
  "\$(cat "$PROMPT_INPUT_FILE")" > "$TMUX_RESPONSE_FILE" 2>"$STDERR_FILE"
echo \$? > "$TMUX_STATUS_FILE"
RUNNER_EOF
      chmod +x "$RUNNER_SCRIPT"
      tmux kill-session -t "$TMUX_SESSION" 2>/dev/null || true
      tmux new-session -d -s "$TMUX_SESSION" -x 200 -y 50 "$RUNNER_SCRIPT"
      log_err "[run] task=$TASK_ID tmux session started: $TMUX_SESSION"

      tmux_wait "$TMUX_SESSION" "${AGENT_TIMEOUT_SECONDS:-1800}" || true

      [ -f "$TMUX_RESPONSE_FILE" ] && RESPONSE=$(cat "$TMUX_RESPONSE_FILE")
      [ -f "$TMUX_STATUS_FILE" ] && CMD_STATUS=$(cat "$TMUX_STATUS_FILE")
    else
      RESPONSE=$(cd "$PROJECT_DIR" && run_with_timeout opencode run \
        ${AGENT_MODEL:+-m "$AGENT_MODEL"} \
        --format json \
        "$FULL_MESSAGE" 2>"$STDERR_FILE") || CMD_STATUS=$?
    fi
    ;;
  *)
    log_err "[run] task=$TASK_ID unknown agent: $TASK_AGENT"
    exit 1
    ;;
esac

stop_spinner
cleanup_monitor
AGENT_END=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
AGENT_START_EPOCH=$(date -u -jf "%Y-%m-%dT%H:%M:%SZ" "$AGENT_START" +%s 2>/dev/null || date -d "$AGENT_START" +%s 2>/dev/null || echo 0)
AGENT_END_EPOCH=$(date +%s)
AGENT_DURATION=$((AGENT_END_EPOCH - AGENT_START_EPOCH))
export AGENT_DURATION
log_err "[run] task=$TASK_ID agent finished (exit=$CMD_STATUS) duration=$(duration_fmt $AGENT_DURATION)"

# Save raw response for debugging
RESPONSE_FILE="${STATE_DIR}/${FILE_PREFIX}-response-${ATTEMPTS}.txt"
printf '%s' "$RESPONSE" > "$RESPONSE_FILE"
RESPONSE_LEN=${#RESPONSE}
log_err "[run] task=$TASK_ID response saved to $RESPONSE_FILE (${RESPONSE_LEN} bytes)"

# Extract permission denials / errors from agent response
DENIAL_LOG="${STATE_DIR}/permission-denials.log"
case "$TASK_AGENT" in
  claude)
    DENIALS=$(printf '%s' "$RESPONSE" | jq -r '.permission_denials[]? | "\(.tool_name)(\(.tool_input.command // .tool_input | tostring | .[0:80]))"' 2>/dev/null || true)
    ;;
  codex)
    # Codex JSONL: extract sandbox/permission errors from failed commands and turn.failed events
    DENIALS=$(printf '%s' "$RESPONSE" | jq -r 'select(.type == "turn.failed") | .error.message // empty' 2>/dev/null || true)
    SANDBOX_ERRORS=$(printf '%s' "$RESPONSE" | jq -r 'select(.type == "item.completed" and .item.type == "command_execution" and .item.exit_code != 0) | select(.item.aggregated_output | test("permission denied|sandbox|not allowed|EPERM"; "i")) | "Bash(\(.item.command | .[0:80]))"' 2>/dev/null || true)
    if [ -n "$SANDBOX_ERRORS" ]; then
      DENIALS=$(printf '%s\n%s' "$DENIALS" "$SANDBOX_ERRORS" | sed '/^$/d')
    fi
    ;;
  opencode)
    # OpenCode JSON: extract errors from failed events
    DENIALS=$(printf '%s' "$RESPONSE" | jq -r 'select(.type == "error") | .message // empty' 2>/dev/null || true)
    ;;
esac
if [ -n "$DENIALS" ]; then
  DENIAL_COUNT=$(printf '%s\n' "$DENIALS" | wc -l | tr -d ' ')
  log_err "[run] task=$TASK_ID permission_denials ($DENIAL_COUNT):"
  printf '%s\n' "$DENIALS" | while IFS= read -r _d; do
    log_err "[run]   denied: $_d"
  done
  printf '%s\n' "$DENIALS" | while IFS= read -r _d; do
    printf '%s task=%s agent=%s %s\n' "$(now_iso)" "$TASK_ID" "$TASK_AGENT" "$_d" >> "$DENIAL_LOG"
  done
fi

# Extract tool history from agent response
TOOL_SUMMARY=$(RAW_RESPONSE="$RESPONSE" python3 "$SCRIPT_DIR/normalize_json.py" --tool-summary 2>/dev/null || true)
TOOL_COUNT=0
if [ -n "$TOOL_SUMMARY" ]; then
  RAW_RESPONSE="$RESPONSE" python3 "$SCRIPT_DIR/normalize_json.py" --tool-history > "${STATE_DIR}/${FILE_PREFIX}-tools-${ATTEMPTS}.json" 2>/dev/null || true
  append_task_context "$TASK_ID" "Commands run by agent (attempt $ATTEMPTS):\n$TOOL_SUMMARY"
  TOOL_COUNT=$(printf '%s' "$TOOL_SUMMARY" | wc -l | tr -d ' ')
  log_err "[run] task=$TASK_ID tool history saved ($TOOL_COUNT calls)"
fi

# Extract token usage
INPUT_TOKENS=0
OUTPUT_TOKENS=0
USAGE_JSON=$(RAW_RESPONSE="$RESPONSE" python3 "$SCRIPT_DIR/normalize_json.py" --usage 2>/dev/null || true)
if [ -n "$USAGE_JSON" ]; then
  INPUT_TOKENS=$(printf '%s' "$USAGE_JSON" | jq -r '.input_tokens // 0')
  OUTPUT_TOKENS=$(printf '%s' "$USAGE_JSON" | jq -r '.output_tokens // 0')
fi

# Log stderr even on success
AGENT_STDERR=""
if [ -f "$STDERR_FILE" ] && [ -s "$STDERR_FILE" ]; then
  AGENT_STDERR=$(cat "$STDERR_FILE")
  log_err "[run] task=$TASK_ID stderr: $(printf '%s' "$AGENT_STDERR" | head -c 200)"
fi

reroute_on_usage_limit() {
  local combined_output="$1"
  local hint="${2:-usage/rate limit}"

  local chain
  chain=$(db_task_field "$TASK_ID" "limit_reroute_chain" 2>/dev/null || true)
  chain=$(printf '%s' "$chain" | tr -d '[:space:]')

  if [ -n "$TASK_AGENT" ]; then
    if [ -z "$chain" ]; then
      chain="$TASK_AGENT"
    elif ! printf ',%s,' "$chain" | grep -qF ",${TASK_AGENT},"; then
      chain="${chain},${TASK_AGENT}"
    fi
  fi

  local next_agent
  next_agent=$(pick_fallback_agent "$TASK_AGENT" "$chain" 2>/dev/null || true)

  local snippet
  snippet=$(redact_snippet "$(printf '%s' "$combined_output" | tail -c 300)")

  if [ -z "$next_agent" ]; then
    error_log "[run] task=$TASK_ID $hint for agent=$TASK_AGENT; no fallback agents available (chain=$chain)"
    db_task_update "$TASK_ID" "limit_reroute_chain=$chain" \
      "last_error=$TASK_AGENT hit usage/rate limit; no fallback agents available"
    mark_needs_review "$TASK_ID" "$ATTEMPTS" "$TASK_AGENT hit usage/rate limit; no fallback agents available"
    comment_on_issue "$TASK_ID" "🚨 **Usage/rate limit**: \`$TASK_AGENT\` hit a limit and no fallback agent is available.

Tried: \`${chain:-$TASK_AGENT}\`

\`\`\`
${snippet}
\`\`\`"
    return 1
  fi

  local next_model=""
  local FREE_MODELS=""
  local -a _fm=()
  if [ "$next_agent" = "opencode" ]; then
    FREE_MODELS=$(config_get '.model_map.free // [] | join(",")' 2>/dev/null || true)
    if [ -n "${FREE_MODELS:-}" ]; then
      IFS=',' read -ra _fm <<< "$FREE_MODELS"
      if [ ${#_fm[@]} -gt 0 ]; then
        # Count how many times opencode has already appeared in the reroute chain
        # to cycle through free models correctly across multi-hop reroutes.
        local opencode_idx=0
        if [ -n "$chain" ]; then
          local -a _c=()
          IFS=',' read -ra _c <<< "$chain"
          for _item in "${_c[@]}"; do
            [ "$_item" = "opencode" ] && opencode_idx=$(( opencode_idx + 1 )) || true
          done
        fi
        next_model="${_fm[$((opencode_idx % ${#_fm[@]}))]}"
      fi
    fi
  fi

  error_log "[run] task=$TASK_ID $hint; rerouting $TASK_AGENT → $next_agent${next_model:+ (model=$next_model)} (chain=$chain)"
  db_task_update "$TASK_ID" \
    "agent=$next_agent" \
    "agent_model=${next_model:-}" \
    "status=new" \
    "limit_reroute_chain=$chain" \
    "last_error=$TASK_AGENT hit usage/rate limit; rerouted to $next_agent"
  append_history "$TASK_ID" "new" "$TASK_AGENT usage/rate limit — rerouted to $next_agent${next_model:+ (model=$next_model)}"
  comment_on_issue "$TASK_ID" "⚠️ **Auto-reroute**: \`$TASK_AGENT\` hit a usage/rate limit. Rerouting to \`$next_agent\`${next_model:+ with model \`$next_model\`}.

\`\`\`
${snippet}
\`\`\`"
  return 0
}

# Classify error from exit code, stderr, and stdout
if [ "$CMD_STATUS" -ne 0 ]; then
  COMBINED_OUTPUT="${RESPONSE}${AGENT_STDERR}"

  ENV_FAIL_INFO=$(printf '%s' "$COMBINED_OUTPUT" | detect_missing_tooling 2>/dev/null || true)
  if [ -n "$ENV_FAIL_INFO" ]; then
    ENV_TOOL=""
    ENV_KIND=""
    IFS=$'\t' read -r ENV_TOOL ENV_KIND <<< "$ENV_FAIL_INFO"
    ENV_FAIL_MSG="env/tooling failure: missing ${ENV_TOOL} (${ENV_KIND})"
    error_log "[run] task=$TASK_ID $ENV_FAIL_MSG"
    mark_needs_review "$TASK_ID" "$ATTEMPTS" "$ENV_FAIL_MSG"
    exit 0
  fi

  if [ "$CMD_STATUS" -eq 124 ]; then
    TIMEOUT_REASON="agent timed out (exit 124)"
    if [ -f "$STDERR_FILE" ] && rg -qi 'waiting.*approv|passphrase|unlock|1password|biometric|touch.id|press.*button|enter.*password|interactive.*auth|permission.*denied.*publickey|sign_and_send_pubkey' "$STDERR_FILE" 2>/dev/null; then
      TIMEOUT_REASON="agent stuck waiting for interactive approval (1Password/SSH/passphrase) — configure headless auth"
      error_log "[run] task=$TASK_ID TIMEOUT: stuck on interactive approval"
    else
      error_log "[run] task=$TASK_ID TIMEOUT after $(duration_fmt $AGENT_DURATION)"
    fi
    mark_needs_review "$TASK_ID" "$ATTEMPTS" "$TIMEOUT_REASON"
    exit 0
  fi

  if is_usage_limit_error "$COMBINED_OUTPUT"; then
    reroute_on_usage_limit "$COMBINED_OUTPUT" "agent command usage/rate limit" || true
    exit 0
  fi

  if printf '%s' "$COMBINED_OUTPUT" | rg -qi 'unauthorized|invalid.*(api|key|token)|auth.*fail|401|403|no.*(api|key|token)|expired.*(key|token|plan)|billing|insufficient.*credit|payment.*required|credit.balance.*too.low'; then
    error_log "[run] task=$TASK_ID AUTH/BILLING ERROR for agent=$TASK_AGENT"
    AVAILABLE=$(available_agents)
    NEXT_AGENT=$(pick_fallback_agent "$TASK_AGENT" "" 2>/dev/null || true)
    if [ -n "$NEXT_AGENT" ]; then
      # If switching to opencode, pick a free model via round-robin indexed by
      # opencode-specific switch count (not total attempts, to avoid skipping models).
      NEXT_MODEL=""
      if [ "$NEXT_AGENT" = "opencode" ]; then
        FREE_MODELS=$(config_get '.model_map.free // [] | join(",")' 2>/dev/null || true)
        if [ -n "$FREE_MODELS" ]; then
          FREE_IDX=$(db_task_history "$TASK_ID" 2>/dev/null | grep -c "switched to opencode" || echo "0")
          IFS=',' read -ra _fm <<< "$FREE_MODELS"
          if [ ${#_fm[@]} -gt 0 ]; then
            NEXT_MODEL="${_fm[$((FREE_IDX % ${#_fm[@]}))]}"
          fi
        fi
      fi
      log_err "[run] task=$TASK_ID switching from $TASK_AGENT to $NEXT_AGENT${NEXT_MODEL:+ (model=$NEXT_MODEL)} (auth/billing error)"
      # Always reset agent_model — let model_for_complexity resolve the correct one for the new agent
      # Keep attempt count so max_attempts guard still works (prevents infinite reroute loops)
      db_task_update "$TASK_ID" \
        "agent=$NEXT_AGENT" \
        "agent_model=${NEXT_MODEL:-}" \
        "status=new" \
        "last_error=$TASK_AGENT auth/billing error, switched to $NEXT_AGENT"
      append_history "$TASK_ID" "new" "$TASK_AGENT auth/billing error — switched to $NEXT_AGENT${NEXT_MODEL:+ (model=$NEXT_MODEL)}"
      STDERR_SNIPPET=$(redact_snippet "$(printf '%s' "$COMBINED_OUTPUT" | tail -c 300)")
      comment_on_issue "$TASK_ID" "⚠️ **Agent switch**: \`$TASK_AGENT\` failed with auth/billing error. Switching to \`$NEXT_AGENT\`${NEXT_MODEL:+ with model \`$NEXT_MODEL\`}.

\`\`\`
${STDERR_SNIPPET}
\`\`\`"
    else
      # All agents exhausted — try opencode with free models as last resort
      if command -v opencode >/dev/null 2>&1; then
        FREE_MODELS=$(config_get '.model_map.free // [] | join(",")' 2>/dev/null || true)
        if [ -n "$FREE_MODELS" ]; then
          FREE_IDX=$(db_task_history "$TASK_ID" 2>/dev/null | grep -c "free model fallback" || echo "0")
          IFS=',' read -ra _fm <<< "$FREE_MODELS"
          if [ ${#_fm[@]} -gt 0 ]; then
            FREE_MODEL="${_fm[$((FREE_IDX % ${#_fm[@]}))]}"
            log_err "[run] task=$TASK_ID all agents exhausted, trying opencode with free model: $FREE_MODEL"
            db_task_update "$TASK_ID" \
              "agent=opencode" \
              "agent_model=$FREE_MODEL" \
              "status=new" \
              "last_error=all agents hit limits, free model fallback: $FREE_MODEL"
            append_history "$TASK_ID" "new" "free model fallback — opencode with $FREE_MODEL"
            comment_on_issue "$TASK_ID" "⚠️ **All agents at limit**: falling back to \`opencode\` with free model \`$FREE_MODEL\`."
          else
            mark_needs_review "$TASK_ID" "$ATTEMPTS" "auth/billing error for $TASK_AGENT — no agents or free models available"
            comment_on_issue "$TASK_ID" "🚨 **Auth/billing error**: \`$TASK_AGENT\` failed and no other agents or free models are available."
          fi
        else
          mark_needs_review "$TASK_ID" "$ATTEMPTS" "auth/billing error for $TASK_AGENT — no other agents available"
          comment_on_issue "$TASK_ID" "🚨 **Auth/billing error**: \`$TASK_AGENT\` failed and no other agents are available. Manual intervention needed."
        fi
      else
        mark_needs_review "$TASK_ID" "$ATTEMPTS" "auth/billing error for $TASK_AGENT — no other agents available"
        comment_on_issue "$TASK_ID" "🚨 **Auth/billing error**: \`$TASK_AGENT\` failed and no other agents are available. Manual intervention needed."
      fi
    fi
    exit 0
  fi

  error_log "[run] task=$TASK_ID agent command failed exit=$CMD_STATUS"
  mark_needs_review "$TASK_ID" "$ATTEMPTS" "agent command failed (exit $CMD_STATUS)"
  exit 0
fi

# Read structured output from file (primary), fall back to stdout parsing
RESPONSE_JSON=""
if [ -f "$OUTPUT_FILE" ]; then
  RESPONSE_JSON=$(cat "$OUTPUT_FILE")
  rm -f "$OUTPUT_FILE"
  log_err "[run] read output from $OUTPUT_FILE"
else
  for _alt in "${STATE_DIR}/output-${TASK_ID}.json" \
              "${PROJECT_DIR}/.orchestrator-output-${TASK_ID}.json" \
              "/tmp/output-${TASK_ID}.json"; do
    if [ -f "$_alt" ]; then
      RESPONSE_JSON=$(cat "$_alt")
      rm -f "$_alt"
      log_err "[run] read output from $_alt (fallback)"
      break
    fi
  done
  if [ -z "$RESPONSE_JSON" ]; then
    log_err "[run] output file not found, trying stdout fallback"
    RESPONSE_JSON=$(normalize_json_response "$RESPONSE" 2>/dev/null || true)
  fi
fi

if [ -z "$RESPONSE_JSON" ]; then
  if is_usage_limit_error "${RESPONSE}${AGENT_STDERR}"; then
    reroute_on_usage_limit "${RESPONSE}${AGENT_STDERR}" "invalid/empty response (likely due to usage limit)" || true
    exit 0
  fi
  log_err "[run] task=$TASK_ID invalid JSON response"
  mkdir -p "$CONTEXTS_DIR"
  printf '%s' "$RESPONSE" > "${CONTEXTS_DIR}/${FILE_PREFIX}-response-${ATTEMPTS}.md"
  mark_needs_review "$TASK_ID" "$ATTEMPTS" "agent response invalid YAML/JSON"
  exit 0
fi

# Inject agent/model metadata if not already present
RESPONSE_JSON=$(printf '%s' "$RESPONSE_JSON" | jq \
  --arg agent "$TASK_AGENT" \
  --arg model "${AGENT_MODEL:-default}" \
  '. + {agent: (.agent // $agent), model: (.model // $model)}')

AGENT_STATUS=$(printf '%s' "$RESPONSE_JSON" | jq -r '.status // ""')
SUMMARY=$(printf '%s' "$RESPONSE_JSON" | jq -r '.summary // ""')
ACCOMPLISHED_STR=$(printf '%s' "$RESPONSE_JSON" | jq -r '.accomplished[]?' | tr '\n' '\n')
REMAINING_STR=$(printf '%s' "$RESPONSE_JSON" | jq -r '.remaining[]?' | tr '\n' '\n')
BLOCKERS_STR=$(printf '%s' "$RESPONSE_JSON" | jq -r '.blockers[]?' | tr '\n' '\n')
FILES_CHANGED_STR=$(printf '%s' "$RESPONSE_JSON" | jq -r '.files_changed[]?' | tr '\n' '\n')
REMAINING_STR=${REMAINING_STR:-""}
ACCOMPLISHED_STR=${ACCOMPLISHED_STR:-""}
BLOCKERS_STR=${BLOCKERS_STR:-""}
FILES_CHANGED_STR=${FILES_CHANGED_STR:-""}
NEEDS_HELP=$(printf '%s' "$RESPONSE_JSON" | jq -r '.needs_help // false')
REASON=$(printf '%s' "$RESPONSE_JSON" | jq -r '.reason // ""')
DELEGATIONS_JSON=$(printf '%s' "$RESPONSE_JSON" | jq -c '.delegations // []')

# If the agent output indicates missing tooling, mark as needs_review and
# surface the missing tool in the error message to prevent retry loops.
ENV_FAIL_INFO=$(printf '%s\n%s\n%s\n%s\n' "$RESPONSE" "$AGENT_STDERR" "$REASON" "$BLOCKERS_STR" | detect_missing_tooling 2>/dev/null || true)
ENV_FAIL_MSG=""
if [ -n "$ENV_FAIL_INFO" ]; then
  ENV_TOOL=""
  ENV_KIND=""
  IFS=$'\t' read -r ENV_TOOL ENV_KIND <<< "$ENV_FAIL_INFO"
  ENV_FAIL_MSG="env/tooling failure: missing ${ENV_TOOL} (${ENV_KIND})"
  if [ -z "$REASON" ] || [ "$REASON" = "null" ]; then
    REASON="$ENV_FAIL_MSG"
  elif ! printf '%s' "$REASON" | rg -qi '^env/tooling failure:'; then
    REASON="${ENV_FAIL_MSG} — ${REASON}"
  fi
  if [ "$AGENT_STATUS" != "needs_review" ] && [ "$AGENT_STATUS" != "blocked" ]; then
    AGENT_STATUS="needs_review"
  fi
fi

if [ -z "$AGENT_STATUS" ] || [ "$AGENT_STATUS" = "null" ]; then
  if is_usage_limit_error "${RESPONSE}${AGENT_STDERR}"; then
    reroute_on_usage_limit "${RESPONSE}${AGENT_STDERR}" "agent response missing status (likely due to usage limit)" || true
    exit 0
  fi
  mark_needs_review "$TASK_ID" "$ATTEMPTS" "agent response missing status"
  exit 0
fi

NOW=$(now_iso)
export AGENT_STATUS SUMMARY NEEDS_HELP NOW FILES_CHANGED_STR ACCOMPLISHED_STR REMAINING_STR BLOCKERS_STR REASON

# If the agent reports needs_review/blocked due to usage limits, auto-reroute.
if { [ "$AGENT_STATUS" = "needs_review" ] || [ "$AGENT_STATUS" = "blocked" ]; } && \
   is_usage_limit_error "${REASON}${AGENT_STDERR}"; then
  reroute_on_usage_limit "${REASON}${AGENT_STDERR}" "agent reported usage/rate limit" || true
  exit 0
fi

# Store agent metadata
RESP_AGENT=$(printf '%s' "$RESPONSE_JSON" | jq -r '.agent // ""')
RESP_MODEL=$(printf '%s' "$RESPONSE_JSON" | jq -r '.model // ""')
STDERR_SNIPPET=""
if [ -n "$AGENT_STDERR" ]; then
  STDERR_SNIPPET=$(printf '%s' "$AGENT_STDERR" | tail -c 500)
fi

# Store all response data in SQLite
db_store_agent_response "$TASK_ID" "$AGENT_STATUS" "$SUMMARY" "$REASON" \
  "$NEEDS_HELP" "$RESP_MODEL" "$AGENT_DURATION" "$INPUT_TOKENS" "$OUTPUT_TOKENS" \
  "$STDERR_SNIPPET" "$PROMPT_HASH"

# Preserve a clear last_error for env/tooling failures (used by operators and retry-loop tooling).
if [ -n "$ENV_FAIL_MSG" ]; then
  db_task_update "$TASK_ID" "last_error=$ENV_FAIL_MSG"
fi

db_store_agent_arrays "$TASK_ID" "$ACCOMPLISHED_STR" "$REMAINING_STR" "$BLOCKERS_STR" "$FILES_CHANGED_STR"

# Clear usage-limit reroute chain only when task reaches a terminal success state.
# Do NOT clear on in_progress — the next iteration could still hit a limit and we
# need the chain intact to prevent ping-pong back to an already-exhausted agent.
if [ "$AGENT_STATUS" = "done" ]; then
  db_task_update "$TASK_ID" "limit_reroute_chain=NULL" 2>/dev/null || true
fi

# Fallback: auto-commit any uncommitted changes the agent left behind
if [ "$AGENT_STATUS" = "done" ] || [ "$AGENT_STATUS" = "in_progress" ]; then
  if [ -d "$PROJECT_DIR" ] && (cd "$PROJECT_DIR" && git rev-parse --is-inside-work-tree >/dev/null 2>&1); then
    if (cd "$PROJECT_DIR" && ! git diff --quiet 2>/dev/null) || \
       (cd "$PROJECT_DIR" && ! git diff --cached --quiet 2>/dev/null) || \
       (cd "$PROJECT_DIR" && [ -n "$(git ls-files --others --exclude-standard 2>/dev/null)" ]); then
      COMMIT_MSG="feat: ${TASK_TITLE}

Task #${TASK_ID}${GH_ISSUE_NUMBER:+ (Closes #${GH_ISSUE_NUMBER})}
Agent: ${TASK_AGENT}
Attempt: ${ATTEMPTS}"
      log_err "[run] task=$TASK_ID auto-committing uncommitted changes"
      (cd "$PROJECT_DIR" && git add -A && git commit -m "$COMMIT_MSG" 2>>"$STDERR_FILE") || \
        log_err "[run] task=$TASK_ID auto-commit failed"
    fi
  fi
fi

# Fallback: push agent's branch if there are local commits not on remote
if [ "$AGENT_STATUS" = "done" ] || [ "$AGENT_STATUS" = "in_progress" ]; then
  if [ -d "$PROJECT_DIR" ] && (cd "$PROJECT_DIR" && git rev-parse --is-inside-work-tree >/dev/null 2>&1); then
    CURRENT_BRANCH=$(cd "$PROJECT_DIR" && git branch --show-current 2>/dev/null || true)
    if [ -n "$CURRENT_BRANCH" ] && [ "$CURRENT_BRANCH" != "main" ] && [ "$CURRENT_BRANCH" != "master" ]; then
      HAS_UNPUSHED=false
      if (cd "$PROJECT_DIR" && git rev-parse "origin/${CURRENT_BRANCH}" >/dev/null 2>&1); then
        if (cd "$PROJECT_DIR" && git log "origin/${CURRENT_BRANCH}..HEAD" --oneline 2>/dev/null | rg -q .); then
          HAS_UNPUSHED=true
        fi
      else
        if (cd "$PROJECT_DIR" && git log "${DEFAULT_BRANCH}..HEAD" --oneline 2>/dev/null | rg -q .); then
          HAS_UNPUSHED=true
        fi
      fi

      if [ "$HAS_UNPUSHED" = true ]; then
        log_err "[run] task=$TASK_ID pushing branch $CURRENT_BRANCH"
        if (cd "$PROJECT_DIR" && git \
          -c "url.https://github.com/.insteadOf=git@github.com:" \
          push -u origin "$CURRENT_BRANCH" 2>>"$STDERR_FILE"); then
          run_hook on_branch_pushed
        else
          error_log "[run] task=$TASK_ID failed to push branch $CURRENT_BRANCH"
        fi

        if command -v gh >/dev/null 2>&1; then
          EXISTING_PR=$(cd "$PROJECT_DIR" && gh pr list --head "$CURRENT_BRANCH" --json number -q '.[0].number' 2>/dev/null || true)
          if [ -z "$EXISTING_PR" ]; then
            PR_TITLE="${SUMMARY:-$TASK_TITLE}"
            PR_BODY="## Summary

${SUMMARY:-$TASK_TITLE}"
            if [ -n "$ACCOMPLISHED_STR" ]; then
              PR_BODY="${PR_BODY}

### What was done

$(printf '%s\n' "$ACCOMPLISHED_STR" | sed 's/^/- /')"
            fi
            if [ -n "$REMAINING_STR" ]; then
              PR_BODY="${PR_BODY}

### Remaining

$(printf '%s\n' "$REMAINING_STR" | sed 's/^/- /')"
            fi
            if [ -n "$FILES_CHANGED_STR" ]; then
              PR_BODY="${PR_BODY}

### Files changed

$(printf '%s\n' "$FILES_CHANGED_STR" | sed 's/^/- `/' | sed 's/$/`/')"
            fi
            PR_BODY="${PR_BODY}

${GH_ISSUE_NUMBER:+Closes #${GH_ISSUE_NUMBER}}

---
*Created by ${TASK_AGENT}[bot] via [Orchestrator](https://github.com/gabrielkoerich/orchestrator)*"
            PR_URL=$(cd "$PROJECT_DIR" && gh pr create \
              --title "$PR_TITLE" \
              --body "$PR_BODY" \
              --head "$CURRENT_BRANCH" 2>>"$STDERR_FILE" || true)
            if [ -n "$PR_URL" ]; then
              log_err "[run] task=$TASK_ID created PR: $PR_URL"
              run_hook on_pr_created
            else
              log_err "[run] task=$TASK_ID failed to create PR for $CURRENT_BRANCH"
            fi
          fi
        fi
      fi
    fi
  fi
fi

# Override "done" → "in_review" when there's an open PR
if [ "$AGENT_STATUS" = "done" ]; then
  PR_NUMBER=""
  if [ -n "${BRANCH_NAME:-}" ] && [ "$BRANCH_NAME" != "main" ] && [ "$BRANCH_NAME" != "master" ]; then
    if command -v gh >/dev/null 2>&1 && [ -d "$PROJECT_DIR" ]; then
      PR_NUMBER=$(cd "$PROJECT_DIR" && gh pr list --head "$BRANCH_NAME" --json number,state -q '.[] | select(.state == "OPEN") | .number' 2>/dev/null || true)
    fi
  fi
  if [ -n "$PR_NUMBER" ]; then
    log_err "[run] task=$TASK_ID overriding done → in_review (PR #$PR_NUMBER open)"
    AGENT_STATUS="in_review"
    db_task_set "$TASK_ID" "status" "in_review"
  fi
fi

# Build history note with reason if present
HISTORY_NOTE="agent completed"
if [ -n "$REASON" ] && [ "$REASON" != "null" ]; then
  HISTORY_NOTE="agent completed: $REASON"
fi
append_history "$TASK_ID" "$AGENT_STATUS" "$HISTORY_NOTE"

FILES_CHANGED=$(printf '%s' "$RESPONSE_JSON" | jq -r '.files_changed | join(", ")')
append_task_context "$TASK_ID" "[$NOW] status: $AGENT_STATUS\nsummary: $SUMMARY\nreason: $REASON\nfiles: $FILES_CHANGED\n"

# Optional review step
ENABLE_REVIEW_AGENT=${ENABLE_REVIEW_AGENT:-$(config_get '.workflow.enable_review_agent // false')}
REVIEW_AGENT=${REVIEW_AGENT:-$(opposite_agent "$TASK_AGENT")}

if [ "$AGENT_STATUS" = "in_review" ] && [ "$ENABLE_REVIEW_AGENT" = "true" ] && [ -n "$PR_NUMBER" ]; then
  FILES_CHANGED=$(printf '%s' "$RESPONSE_JSON" | jq -r '.files_changed | join(", ")')

  export TASK_SUMMARY="$SUMMARY"
  export TASK_FILES_CHANGED="$FILES_CHANGED"
  export GIT_DIFF PR_NUMBER
  GIT_DIFF=$(cd "$PROJECT_DIR" && gh pr diff "$PR_NUMBER" 2>/dev/null | head -500 || true)

  REVIEW_PROMPT=$(render_template "$SCRIPT_DIR/../prompts/review.md")

  log_err "[run] task=$TASK_ID starting review by $REVIEW_AGENT for PR #$PR_NUMBER"

  run_review_agent_once() {
    local agent="$1" model="$2" prompt="$3"
    local response _rra_status
    response=""
    _rra_status=0
    case "$agent" in
      codex)
        response=$(run_with_timeout codex ${model:+--model "$model"} --print "$prompt") || _rra_status=$?
        ;;
      claude)
        response=$(run_with_timeout claude ${model:+--model "$model"} --print "$prompt") || _rra_status=$?
        ;;
      opencode)
        response=$(run_with_timeout opencode ${model:+--model "$model"} --print "$prompt") || _rra_status=$?
        ;;
      *)
        log_err "[run] task=$TASK_ID unknown review agent: $agent"
        _rra_status=1
        ;;
    esac

    if [ "$_rra_status" -eq 0 ]; then
      if ! printf '%s' "$response" | jq -e . >/dev/null 2>&1; then
        log_err "[run] task=$TASK_ID review agent $agent returned invalid JSON"
        _rra_status=2
      fi
    fi

    REVIEW_RESPONSE="$response"
    return "$_rra_status"
  }

  REVIEW_MODEL=$(model_for_complexity "$REVIEW_AGENT" "review")
  REVIEW_RESPONSE=""
  REVIEW_STATUS=0
  run_review_agent_once "$REVIEW_AGENT" "$REVIEW_MODEL" "$REVIEW_PROMPT" || REVIEW_STATUS=$?

  if [ "$REVIEW_STATUS" -ne 0 ]; then
    PRIMARY_REVIEW_AGENT="$REVIEW_AGENT"
    PRIMARY_REVIEW_STATUS="$REVIEW_STATUS"

    FALLBACK_REVIEWER=$(opposite_agent "$PRIMARY_REVIEW_AGENT")
    if [ -n "$FALLBACK_REVIEWER" ] && [ "$FALLBACK_REVIEWER" != "$PRIMARY_REVIEW_AGENT" ]; then
      log_err "[run] task=$TASK_ID review by $PRIMARY_REVIEW_AGENT failed (exit=$PRIMARY_REVIEW_STATUS) — retrying once with $FALLBACK_REVIEWER"
      REVIEW_AGENT="$FALLBACK_REVIEWER"
      REVIEW_MODEL=$(model_for_complexity "$REVIEW_AGENT" "review")
      REVIEW_RESPONSE=""
      REVIEW_STATUS=0
      run_review_agent_once "$REVIEW_AGENT" "$REVIEW_MODEL" "$REVIEW_PROMPT" || REVIEW_STATUS=$?
    fi

    if [ "$REVIEW_STATUS" -ne 0 ]; then
      if [ -n "${FALLBACK_REVIEWER:-}" ] && [ "$FALLBACK_REVIEWER" != "$PRIMARY_REVIEW_AGENT" ]; then
        mark_needs_review "$TASK_ID" "$ATTEMPTS" "review agent failed (primary=$PRIMARY_REVIEW_AGENT exit=$PRIMARY_REVIEW_STATUS, fallback=$FALLBACK_REVIEWER exit=$REVIEW_STATUS)"
      else
        mark_needs_review "$TASK_ID" "$ATTEMPTS" "review agent failed (agent=$PRIMARY_REVIEW_AGENT exit=$PRIMARY_REVIEW_STATUS)"
      fi
      exit 0
    fi
  fi

  REVIEW_DECISION=$(printf '%s' "$REVIEW_RESPONSE" | jq -r '.decision // ""')
  REVIEW_NOTES_VAL=$(printf '%s' "$REVIEW_RESPONSE" | jq -r '.notes // ""')

  if [ "$REVIEW_DECISION" = "approve" ]; then
    (cd "$PROJECT_DIR" && gh pr review "$PR_NUMBER" --approve --body "${REVIEW_NOTES_VAL:-Approved by $REVIEW_AGENT review agent}" 2>/dev/null) || true
    log_err "[run] task=$TASK_ID review approved PR #$PR_NUMBER"
  elif [ "$REVIEW_DECISION" = "request_changes" ]; then
    (cd "$PROJECT_DIR" && gh pr review "$PR_NUMBER" --request-changes --body "${REVIEW_NOTES_VAL:-Changes requested by $REVIEW_AGENT review agent}" 2>/dev/null) || true
    log_err "[run] task=$TASK_ID review requested changes on PR #$PR_NUMBER"
    mark_needs_review "$TASK_ID" "$ATTEMPTS" "review requested changes: ${REVIEW_NOTES_VAL:-}"
    exit 0
  elif [ "$REVIEW_DECISION" = "reject" ]; then
    log_err "[run] task=$TASK_ID review rejected — closing PR #$PR_NUMBER"
    (cd "$PROJECT_DIR" && gh pr review "$PR_NUMBER" --request-changes --body "Rejected by $REVIEW_AGENT: ${REVIEW_NOTES_VAL:-no notes}" 2>/dev/null) || true
    (cd "$PROJECT_DIR" && gh pr close "$PR_NUMBER" --comment "Rejected by review agent: ${REVIEW_NOTES_VAL:-no notes}" 2>/dev/null) || true
    log_err "[run] task=$TASK_ID closed PR #$PR_NUMBER"
    mark_needs_review "$TASK_ID" "$ATTEMPTS" "review rejected: ${REVIEW_NOTES_VAL:-hallucination or broken changes}"
    exit 0
  fi

  db_task_update "$TASK_ID" "review_decision=approve" "review_notes=$REVIEW_NOTES_VAL"
  append_history "$TASK_ID" "done" "review approved by $REVIEW_AGENT"
fi

# Only allow delegations from plan/decompose tasks
DELEG_COUNT=0
if [ "$DECOMPOSE" = true ]; then
  DELEG_COUNT=$(printf '%s' "$DELEGATIONS_JSON" | jq -r 'length')
else
  _raw_deleg=$(printf '%s' "$DELEGATIONS_JSON" | jq -r 'length')
  if [ "$_raw_deleg" -gt 0 ] 2>/dev/null; then
    log_err "[run] task=$TASK_ID ignoring $_raw_deleg delegations (not a plan task)"
  fi
fi

if [ "$DELEG_COUNT" -gt 0 ]; then
  CHILD_IDS=()
  for i in $(seq 0 $((DELEG_COUNT - 1))); do
    D_TITLE=$(printf '%s' "$DELEGATIONS_JSON" | jq -r ".[$i].title // \"\"")
    D_BODY=$(printf '%s' "$DELEGATIONS_JSON" | jq -r ".[$i].body // \"\"")
    D_LABELS=$(printf '%s' "$DELEGATIONS_JSON" | jq -r ".[$i].labels // [] | join(\",\")")
    D_AGENT=$(printf '%s' "$DELEGATIONS_JSON" | jq -r ".[$i].suggested_agent // \"\"")

    NEW_ID=$(db_create_task "$D_TITLE" "$D_BODY" "$PROJECT_DIR" "$D_LABELS" "$TASK_ID" "$D_AGENT")
    CHILD_IDS+=("$NEW_ID")
  done

  db_task_set "$TASK_ID" "status" "blocked"
  printf 'Spawned children: %s\n' "${CHILD_IDS[*]}"
  append_history "$TASK_ID" "blocked" "spawned children: ${CHILD_IDS[*]}"
fi

log_err "[run] task=$TASK_ID DONE status=$AGENT_STATUS agent=$TASK_AGENT model=${RESP_MODEL:-${AGENT_MODEL:-default}} attempt=$ATTEMPTS duration=$(duration_fmt $AGENT_DURATION) tokens=${INPUT_TOKENS}in/${OUTPUT_TOKENS}out tools=$TOOL_COUNT"

# Fire completion hooks
case "$AGENT_STATUS" in
  done|in_review) run_hook on_task_completed ;;
  blocked)        run_hook on_task_blocked ;;
  needs_review)   run_hook on_task_failed ;;
  *)              run_hook on_task_completed ;;
esac

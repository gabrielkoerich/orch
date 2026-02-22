#!/usr/bin/env bash
# GitHub Issues backend for orchestrator.
# Task ID = GitHub Issue Number. No local database.
# Sourced by backend.sh — provides all backend_* and db_* functions.
# shellcheck disable=SC2155

# ============================================================
# Config & Constants
# ============================================================

_GH_REPO="${ORCH_GH_REPO:-}"
_GH_STATUS_PREFIX="status:"
_GH_AGENT_PREFIX="agent:"
_GH_COMPLEXITY_PREFIX="complexity:"
_GH_SKILL_PREFIX="skill:"
_GH_MODEL_PREFIX="model:"

# Validation constants
_GH_VALID_STATUSES="new routed in_progress done blocked in_review needs_review"
_GH_VALID_AGENTS="claude codex opencode"
_GH_KNOWN_STANDALONE="plan scheduled blocked no-agent no-review has-error"
_GH_RESERVED_PREFIXES="status: agent: model: skill: job:"

# Model patterns per agent (prefix matching, for post-run diagnostics)
_GH_CLAUDE_MODELS="haiku sonnet opus claude-"
_GH_CODEX_MODELS="o1 o3 o4 gpt-4 codex-"

# Allowed issue authors — repo owner + configured collaborators
_GH_ALLOWED_AUTHORS=""
_gh_allowed_authors() {
  if [ -n "$_GH_ALLOWED_AUTHORS" ]; then echo "$_GH_ALLOWED_AUTHORS"; return 0; fi
  _gh_ensure_repo || return 1
  # Repo owner is always allowed
  local owner="${_GH_REPO%%/*}"
  _GH_ALLOWED_AUTHORS="$owner"
  # Add configured collaborators
  local extra
  extra=$(config_get '.workflow.allowed_authors // [] | .[]' 2>/dev/null || true)
  if [ -n "$extra" ]; then
    while IFS= read -r _auth; do
      [ -n "$_auth" ] && [ "$_auth" != "null" ] && _GH_ALLOWED_AUTHORS="${_GH_ALLOWED_AUTHORS},$_auth"
    done <<< "$extra"
  fi
  echo "$_GH_ALLOWED_AUTHORS"
}

# Check if an author is allowed to create tasks
# Usage: _gh_is_allowed_author <login>
_gh_is_allowed_author() {
  local login="$1"
  [ -n "$login" ] || return 1
  local allowed
  allowed=$(_gh_allowed_authors) || return 1
  # Check each allowed author (comma-separated)
  local IFS=','
  local _auth
  for _auth in $allowed; do
    [ "$_auth" = "$login" ] && return 0
  done
  return 1
}

# Sidecar dir is computed dynamically to pick up project-local STATE_DIR
_gh_sidecar_dir() {
  echo "${STATE_DIR:-${ORCH_HOME:-.}/.orchestrator}/tasks"
}

_gh_ensure_repo() {
  if [ -n "$_GH_REPO" ]; then return 0; fi
  _GH_REPO=$(config_get '.gh.repo // ""' 2>/dev/null || true)
  if [ -z "$_GH_REPO" ] || [ "$_GH_REPO" = "null" ]; then
    local dir="${PROJECT_DIR:-.}"
    if [ -d "$dir/.git" ] && command -v gh >/dev/null 2>&1; then
      _GH_REPO=$(cd "$dir" && gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || true)
    elif is_bare_repo "$dir" 2>/dev/null; then
      _GH_REPO=$(git -C "$dir" config remote.origin.url 2>/dev/null \
        | sed -E 's#^https?://github\.com/##; s#^git@github\.com:##; s#\.git$##' || true)
    fi
  fi
  if [ -z "$_GH_REPO" ] || [ "$_GH_REPO" = "null" ]; then
    echo "No GitHub repo configured. Run 'orchestrator init' first." >&2
    return 1
  fi
}

# ============================================================
# Sidecar helpers — local JSON for ephemeral/machine-local fields
# ============================================================

_sidecar_path() {
  local id="$1"
  echo "$(_gh_sidecar_dir)/${id}.json"
}

_sidecar_ensure() {
  mkdir -p "$(_gh_sidecar_dir)"
}

_sidecar_read() {
  local id="$1" field="$2"
  local path
  path=$(_sidecar_path "$id")
  if [ -f "$path" ]; then
    jq -r ".${field} // empty" "$path" 2>/dev/null || true
  fi
}

_sidecar_write() {
  local id="$1" field="$2" value="$3"
  _sidecar_ensure
  local path
  path=$(_sidecar_path "$id")
  local existing='{}'
  [ -f "$path" ] && existing=$(cat "$path" 2>/dev/null || echo '{}')
  printf '%s' "$existing" | jq -c --arg k "$field" --arg v "$value" '.[$k] = $v' > "$path"
}

_sidecar_write_json() {
  local id="$1" field="$2" value_json="$3"
  _sidecar_ensure
  local path
  path=$(_sidecar_path "$id")
  local existing='{}'
  [ -f "$path" ] && existing=$(cat "$path" 2>/dev/null || echo '{}')
  printf '%s' "$existing" | jq -c --arg k "$field" --argjson v "$value_json" '.[$k] = $v' > "$path"
}

_sidecar_merge() {
  local id="$1" json="$2"
  _sidecar_ensure
  local path
  path=$(_sidecar_path "$id")
  local existing='{}'
  [ -f "$path" ] && existing=$(cat "$path" 2>/dev/null || echo '{}')
  printf '%s' "$existing" | jq -c --argjson n "$json" '. * $n' > "$path"
}

_sidecar_full() {
  local id="$1"
  local path
  path=$(_sidecar_path "$id")
  if [ -f "$path" ]; then
    cat "$path"
  else
    echo '{}'
  fi
}

# ============================================================
# Label helpers
# ============================================================

_gh_status_label() {
  local status="$1"
  echo "${_GH_STATUS_PREFIX}${status}"
}

# Extract status from labels array (JSON)
_gh_status_from_labels() {
  local labels_json="$1"
  printf '%s' "$labels_json" | jq -r --arg p "$_GH_STATUS_PREFIX" \
    '[.[] | (if type == "object" then .name else . end) | select(startswith($p))] | .[0] // "" | ltrimstr($p)' 2>/dev/null || true
}

# Extract agent from labels
_gh_agent_from_labels() {
  local labels_json="$1"
  printf '%s' "$labels_json" | jq -r --arg p "$_GH_AGENT_PREFIX" \
    '[.[] | (if type == "object" then .name else . end) | select(startswith($p))] | .[0] // "" | ltrimstr($p)' 2>/dev/null || true
}

# Extract complexity from labels
_gh_complexity_from_labels() {
  local labels_json="$1"
  printf '%s' "$labels_json" | jq -r --arg p "$_GH_COMPLEXITY_PREFIX" \
    '[.[] | (if type == "object" then .name else . end) | select(startswith($p))] | .[0] // "" | ltrimstr($p)' 2>/dev/null || echo "medium"
}

# Extract model from labels
_gh_model_from_labels() {
  local labels_json="$1"
  printf '%s' "$labels_json" | jq -r --arg p "$_GH_MODEL_PREFIX" \
    '[.[] | (if type == "object" then .name else . end) | select(startswith($p))] | .[0] // "" | ltrimstr($p)' 2>/dev/null || true
}

# Ensure a label exists on the repo
_gh_ensure_label() {
  local name="$1" color="${2:-ededed}" description="${3:-}"
  _gh_validate_label "$name" || return 1
  _gh_ensure_repo || return 0
  local encoded
  encoded=$(printf '%s' "$name" | jq -sRr @uri)
  local existing
  existing=$(gh_api "repos/$_GH_REPO/labels/$encoded" 2>/dev/null || true)
  if [ -z "$existing" ]; then
    gh_api "repos/$_GH_REPO/labels" \
      -f name="$name" -f color="$color" -f description="$description" >/dev/null 2>&1 || true
  fi
}

_gh_status_color() {
  local status="$1"
  case "$status" in
    new)           echo "0e8a16" ;;
    routed)        echo "1d76db" ;;
    in_progress)   echo "fbca04" ;;
    done)          echo "0e8a16" ;;
    blocked)       echo "d73a4a" ;;
    in_review)     echo "0075ca" ;;
    needs_review)  echo "e4e669" ;;
    *)             echo "c5def5" ;;
  esac
}

# ============================================================
# Label validation
# ============================================================

# Validate a label before creation/addition.
# Returns 0 if valid, 1 if invalid.
# Usage: _gh_validate_label "status:new" → ok
#        _gh_validate_label "status:invalid" → fail
#        _gh_validate_label "my-custom-label" → ok (user label)
_gh_validate_label() {
  local label="$1"
  [ -z "$label" ] && return 0

  # Check each reserved prefix
  for prefix in $_GH_RESERVED_PREFIXES; do
    if [[ "$label" == "${prefix}"* ]]; then
      local value="${label#"$prefix"}"
      case "$prefix" in
        "status:")
          for v in $_GH_VALID_STATUSES; do
            [ "$v" = "$value" ] && return 0
          done
          log_err "[validate] invalid status label: $label (allowed: $_GH_VALID_STATUSES)"
          return 1
          ;;
        "agent:")
          for v in $_GH_VALID_AGENTS; do
            [ "$v" = "$value" ] && return 0
          done
          log_err "[validate] invalid agent label: $label (allowed: $_GH_VALID_AGENTS)"
          return 1
          ;;
        "model:")
          # model: labels are validated via _gh_validate_agent_model at runtime
          return 0
          ;;
        "skill:"|"job:")
          # Open-ended — any value allowed
          return 0
          ;;
      esac
    fi
  done

  # Not a reserved prefix — user label, always allowed
  return 0
}

# Cross-validate agent + model combination (for post-run diagnostics).
# Returns 0 if consistent, 1 if mismatch.
# NOT used as a pre-validation gate — models change too often.
# Usage: _gh_validate_agent_model "claude" "opus-4"
_gh_validate_agent_model() {
  local agent="$1" model="$2"
  [ -z "$model" ] || [ "$model" = "null" ] && return 0
  [ -z "$agent" ] || [ "$agent" = "null" ] && return 0

  case "$agent" in
    claude)
      for pattern in $_GH_CLAUDE_MODELS; do
        [[ "$model" == "${pattern}"* ]] && return 0
      done
      log_err "[validate] invalid model '$model' for agent '$agent' (allowed prefixes: $_GH_CLAUDE_MODELS)"
      return 1
      ;;
    codex)
      for pattern in $_GH_CODEX_MODELS; do
        [[ "$model" == "${pattern}"* ]] && return 0
      done
      log_err "[validate] invalid model '$model' for agent '$agent' (allowed prefixes: $_GH_CODEX_MODELS)"
      return 1
      ;;
    opencode)
      # Any model allowed for opencode
      return 0
      ;;
    *)
      # Unknown agent — allow
      return 0
      ;;
  esac
}

# ============================================================
# Agent badge
# ============================================================

agent_badge() {
  local agent="${1:-orchestrator}"
  case "$agent" in
    claude)   echo "🤖 🟣 Claude" ;;
    codex)    echo "🤖 🟢 Codex" ;;
    opencode) echo "🤖 🔵 OpenCode" ;;
    *)        echo "🤖 $agent" ;;
  esac
}

# ============================================================
# Structured comment helpers
# ============================================================

_GH_COMMENT_MARKER="<!-- orch:agent-response -->"

# Parse the last agent response comment from issue comments JSON
_gh_parse_agent_comment() {
  local comments_json="$1"
  # Find last comment with our marker
  printf '%s' "$comments_json" | jq -r \
    '[.[] | select(.body | contains("<!-- orch:agent-response -->"))] | last // empty | .body // ""' 2>/dev/null || true
}

# Extract a markdown section value from structured comment
_gh_extract_section() {
  local body="$1" section="$2"
  printf '%s' "$body" | python3 -c "
import sys, re
body = sys.stdin.read()
pattern = r'## ${section}\s*\n(.*?)(?=\n## |\Z)'
m = re.search(pattern, body, re.DOTALL)
if m:
    print(m.group(1).strip())
" 2>/dev/null || true
}

# Extract table field from structured comment
_gh_extract_table_field() {
  local body="$1" field="$2"
  printf '%s' "$body" | grep -i "| *\*\*${field}\*\* *|" | sed 's/.*| *\*\*[^*]*\*\* *| *//' | sed 's/ *|.*//' | sed 's/^`//;s/`$//' || true
}

# ============================================================
# Initialization
# ============================================================

db_init() {
  _sidecar_ensure
  backend_init_jobs
}

db_exists() {
  _gh_ensure_repo 2>/dev/null
}

# ============================================================
# Task CRUD
# ============================================================

# Read a single field from a task (issue).
# Usage: db_task_field <id> <column>
db_task_field() {
  local id="$1" field="$2"

  # GitHub issues are always numeric
  if ! [[ "$id" =~ ^[0-9]+$ ]]; then
    return 0
  fi

  # Check sidecar first for ephemeral fields
  case "$field" in
    branch|worktree|prompt_hash|last_comment_hash|retry_at|worktree_cleaned| \
    attempts|duration|input_tokens|output_tokens|stderr_snippet|agent_model| \
    agent_profile|route_reason|route_warning|decompose|gh_synced_at| \
    gh_synced_status|gh_project_item_id|gh_archived|gh_last_feedback_at| \
    review_decision|review_notes|needs_help|last_error|summary|reason| \
    selected_skills)
      local val
      val=$(_sidecar_read "$id" "$field")
      if [ -n "$val" ]; then
        echo "$val"
        return
      fi
      ;;
  esac

  # For core fields, query GitHub
  _gh_ensure_repo || return 0
  local json
  json=$(gh_api "repos/$_GH_REPO/issues/$id" --cache 60s 2>/dev/null) || return 0

  case "$field" in
    title)       printf '%s' "$json" | jq -r '.title // empty' ;;
    body|description) printf '%s' "$json" | jq -r '.body // empty' ;;
    status)
      local labels_json
      labels_json=$(printf '%s' "$json" | jq -c '.labels // []')
      local st
      st=$(_gh_status_from_labels "$labels_json")
      echo "${st:-new}"
      ;;
    agent)
      local labels_json
      labels_json=$(printf '%s' "$json" | jq -c '.labels // []')
      _gh_agent_from_labels "$labels_json"
      ;;
    complexity)
      # Sidecar first, then fall back to labels (backward compat)
      local sc_complexity
      sc_complexity=$(_sidecar_read "$id" "complexity")
      if [ -n "$sc_complexity" ] && [ "$sc_complexity" != "null" ]; then
        echo "$sc_complexity"
      else
        local labels_json
        labels_json=$(printf '%s' "$json" | jq -c '.labels // []')
        _gh_complexity_from_labels "$labels_json"
      fi
      ;;
    gh_issue_number) echo "$id" ;;
    gh_url)      printf '%s' "$json" | jq -r '.html_url // empty' ;;
    gh_state)    printf '%s' "$json" | jq -r '.state // empty' ;;
    created_at)  printf '%s' "$json" | jq -r '.created_at // empty' ;;
    updated_at)  printf '%s' "$json" | jq -r '.updated_at // empty' ;;
    dir)         _sidecar_read "$id" "dir" ;;
    parent_id)   _sidecar_read "$id" "parent_id" ;;
    *)           _sidecar_read "$id" "$field" ;;
  esac
}

# Update a single field on a task.
# Usage: db_task_set <id> <column> <value>
db_task_set() {
  local id="$1" field="$2" value="$3"
  _gh_ensure_repo || return 0

  case "$field" in
    title)
      gh_api "repos/$_GH_REPO/issues/$id" -X PATCH -f title="$value" >/dev/null 2>&1
      ;;
    body|description)
      gh_api "repos/$_GH_REPO/issues/$id" -X PATCH -f body="$value" >/dev/null 2>&1
      ;;
    status)
      _gh_set_status_label "$id" "$value"
      _sidecar_write "$id" "status" "$value"
      ;;
    agent)
      _gh_set_prefixed_label "$id" "$_GH_AGENT_PREFIX" "$value"
      _sidecar_write "$id" "agent" "$value"
      ;;
    complexity)
      # Sidecar only — no label
      _sidecar_write "$id" "complexity" "$value"
      ;;
    *)
      _sidecar_write "$id" "$field" "$value"
      ;;
  esac
}

# Set a status label (remove old, add new)
_gh_set_status_label() {
  local id="$1" status="$2"
  # Validate status value
  local _valid=false
  for v in $_GH_VALID_STATUSES; do
    [ "$v" = "$status" ] && _valid=true && break
  done
  if [ "$_valid" = false ]; then
    log_err "[validate] invalid status: $status (allowed: $_GH_VALID_STATUSES)"
    return 1
  fi
  _gh_ensure_repo || return 0

  # Remove existing status labels
  local existing_labels
  existing_labels=$(gh_api "repos/$_GH_REPO/issues/$id" --cache 0s -q '[.labels[].name]' 2>/dev/null || echo '[]')
  local old_status_labels
  old_status_labels=$(printf '%s' "$existing_labels" | jq -r --arg p "$_GH_STATUS_PREFIX" '.[] | select(startswith($p))' 2>/dev/null || true)
  for lbl in $old_status_labels; do
    local encoded
    encoded=$(printf '%s' "$lbl" | jq -sRr @uri)
    gh_api "repos/$_GH_REPO/issues/$id/labels/$encoded" -X DELETE >/dev/null 2>&1 || true
  done

  # Add new status label
  local label
  label=$(_gh_status_label "$status")
  local color
  color=$(_gh_status_color "$status")
  _gh_ensure_label "$label" "$color" "Task status: $status"
  gh_api "repos/$_GH_REPO/issues/$id/labels" \
    --input - <<< "{\"labels\":[\"$label\"]}" >/dev/null 2>&1 || true
}

# Remove all labels with a given prefix from an issue
_gh_remove_prefixed_labels() {
  local id="$1" prefix="$2"
  _gh_ensure_repo || return 0

  local existing_labels
  existing_labels=$(gh_api "repos/$_GH_REPO/issues/$id" --cache 0s -q '[.labels[].name]' 2>/dev/null || echo '[]')
  local old_labels
  old_labels=$(printf '%s' "$existing_labels" | jq -r --arg p "$prefix" '.[] | select(startswith($p))' 2>/dev/null || true)
  for lbl in $old_labels; do
    local encoded
    encoded=$(printf '%s' "$lbl" | jq -sRr @uri)
    gh_api "repos/$_GH_REPO/issues/$id/labels/$encoded" -X DELETE >/dev/null 2>&1 || true
  done
}

# Set a prefixed label (remove old prefix:* labels, add new one)
_gh_set_prefixed_label() {
  local id="$1" prefix="$2" value="$3"
  _gh_ensure_repo || return 0
  if [ -z "$value" ] || [ "$value" = "null" ] || [ "$value" = "NULL" ]; then return 0; fi

  local label="${prefix}${value}"
  _gh_validate_label "$label" || return 1

  _gh_remove_prefixed_labels "$id" "$prefix"

  _gh_ensure_label "$label" "c5def5" ""
  gh_api "repos/$_GH_REPO/issues/$id/labels" \
    --input - <<< "{\"labels\":[\"$label\"]}" >/dev/null 2>&1 || true
}

# Atomically claim a task.
# Usage: db_task_claim <id> <from_status> <to_status>
db_task_claim() {
  local id="$1" from_status="$2" to_status="$3"
  local current
  current=$(db_task_field "$id" "status")
  if [ "$current" = "$from_status" ]; then
    _gh_set_status_label "$id" "$to_status"
    _sidecar_write "$id" "status" "$to_status"
    return 0
  fi
  return 1
}

# Count tasks, optionally filtered by status and/or dir.
db_task_count() {
  local _tc_status="${1:-}" _tc_dir="${2:-}"
  _gh_ensure_repo || { echo 0; return; }

  local query="repo:$_GH_REPO is:issue"
  if [ -n "$_tc_status" ]; then
    query="$query label:${_GH_STATUS_PREFIX}${_tc_status}"
  fi
  # For state=open/closed, we query open by default. Done = closed.
  if [ "$_tc_status" = "done" ]; then
    query="$query is:closed"
  else
    query="$query is:open"
  fi

  local result
  result=$(gh_api -X GET "search/issues" -f q="$query" -f per_page=1 -q '.total_count' 2>/dev/null || echo 0)
  echo "$result"
}

# Get task IDs matching a status.
# Usage: db_task_ids_by_status <status> [exclude_label]
db_task_ids_by_status() {
  local _ids_status="$1" exclude_label="${2:-}"
  _gh_ensure_repo || return 0

  local state="open"
  [ "$_ids_status" = "done" ] && state="closed"

  local args=(-f state="$state" -f per_page=100)
  args+=(-f labels="${_GH_STATUS_PREFIX}${_ids_status}")
  local expected_label="${_GH_STATUS_PREFIX}${_ids_status}"
  local needs_review_label="${_GH_STATUS_PREFIX}needs_review"

  local json
  json=$(gh_api -X GET "repos/$_GH_REPO/issues" "${args[@]}" 2>/dev/null) || return 0
  # Handle pagination: flatten arrays
  json=$(printf '%s' "$json" | jq -s 'if type == "array" and length > 0 and (.[0] | type) == "array" then [.[][]] else . end' 2>/dev/null || echo "$json")

  # Build allowed-authors filter for jq (fail closed: empty = reject all)
  local allowed_csv
  allowed_csv=$(_gh_allowed_authors 2>/dev/null) || allowed_csv="__NONE__"

  if [ -n "$exclude_label" ]; then
    printf '%s' "$json" | jq -r --arg lbl "$exclude_label" --arg expected "$expected_label" --arg nr "$needs_review_label" --arg allowed "$allowed_csv" '
      def label_names: (.labels // []) | map(.name);
      def is_allowed: .user.login as $u | $allowed | split(",") | any(. == $u);
      [.[]
        | select(.pull_request == null)
        | select(is_allowed)
        | select(label_names | any(. == $expected))
        | select(($expected == $nr) or ((label_names | any(. == $nr)) | not))
        | select(label_names | all(. != $lbl))
      ]
      | .[].number' 2>/dev/null || true
  else
    printf '%s' "$json" | jq -r --arg expected "$expected_label" --arg nr "$needs_review_label" --arg allowed "$allowed_csv" '
      def label_names: (.labels // []) | map(.name);
      def is_allowed: .user.login as $u | $allowed | split(",") | any(. == $u);
      .[]
      | select(.pull_request == null)
      | select(is_allowed)
      | select(label_names | any(. == $expected))
      | select(($expected == $nr) or ((label_names | any(. == $nr)) | not))
      | .number' 2>/dev/null || true
  fi
}

# Normalize open issues: add status:new to any open issue missing a status: label.
# This ensures externally-created issues (by agents, humans, retrospective jobs) get picked up.
db_normalize_new_issues() {
  _gh_ensure_repo || return 0

  local json
  json=$(gh_api -X GET "repos/$_GH_REPO/issues" \
    -f state=open -f per_page=50 -f sort=created -f direction=desc 2>/dev/null) || return 0

  # Fail closed: if we can't determine allowed authors, skip all issues
  if ! _gh_allowed_authors >/dev/null 2>&1; then
    log_err "[normalize] could not determine allowed authors — skipping all issues"
    return 0
  fi

  # Find issues (not PRs) without any status: label, excluding no-agent, blocked, and needs_review
  local unlabeled
  unlabeled=$(printf '%s' "$json" | jq -r --arg p "$_GH_STATUS_PREFIX" '
    [.[] | select(.pull_request == null)
         | select((.labels // []) | map(.name) | all(startswith($p) | not))
         | select((.labels // []) | map(.name) | all(. != "no-agent" and . != "blocked" and . != "needs_review"))]
    | .[] | "\(.number)\t\(.user.login)"' 2>/dev/null || true)

  [ -n "$unlabeled" ] || return 0

  local _norm_id _norm_author
  while IFS=$'\t' read -r _norm_id _norm_author; do
    [ -n "$_norm_id" ] || continue
    if ! _gh_is_allowed_author "$_norm_author"; then
      log_err "[normalize] issue #$_norm_id by '$_norm_author' — not an allowed author, skipping"
      continue
    fi
    log_err "[normalize] issue #$_norm_id missing status label, adding status:new"
    gh_api "repos/$_GH_REPO/issues/$_norm_id/labels" \
      -f "labels[]=${_GH_STATUS_PREFIX}new" >/dev/null 2>&1 || true
  done <<< "$unlabeled"
}

# Create a new task. Returns the new task ID (issue number).
# Usage: db_create_task <title> [body] [dir] [labels_csv] [parent_id] [agent]
db_create_task() {
  local title="$1" body="${2:-}" dir="${3:-${PROJECT_DIR:-}}"
  local labels_csv="${4:-}" parent_id="${5:-}" agent="${6:-}"
  _gh_ensure_repo || return 1

  local args=(-f title="$title")
  [ -n "$body" ] && args+=(-f body="$body")

  # Build labels array
  local label_args=()
  label_args+=(-f "labels[]=${_GH_STATUS_PREFIX}new")
  _gh_ensure_label "${_GH_STATUS_PREFIX}new" "0e8a16" "Task is new"

  if [ -n "$agent" ]; then
    local agent_label="${_GH_AGENT_PREFIX}${agent}"
    if _gh_validate_label "$agent_label"; then
      _gh_ensure_label "$agent_label" "c5def5" "Assigned to $agent"
      label_args+=(-f "labels[]=$agent_label")
    fi
  fi

  if [ -n "$labels_csv" ]; then
    IFS=',' read -ra _labels <<< "$labels_csv"
    for _l in "${_labels[@]}"; do
      _l=$(printf '%s' "$_l" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
      [ -z "$_l" ] && continue
      if _gh_validate_label "$_l"; then
        label_args+=(-f "labels[]=$_l")
      else
        log_err "[create_task] skipping invalid label: $_l"
      fi
    done
  fi

  local resp
  resp=$(gh_api "repos/$_GH_REPO/issues" "${args[@]}" "${label_args[@]}") || return 1
  local num
  num=$(printf '%s' "$resp" | jq -r '.number')
  local url
  url=$(printf '%s' "$resp" | jq -r '.html_url')

  # Initialize sidecar
  _sidecar_ensure
  local sidecar
  sidecar=$(jq -nc --arg dir "$dir" --arg agent "$agent" --arg parent "$parent_id" --arg url "$url" \
    '{dir: $dir, agent: $agent, parent_id: $parent, gh_url: $url,
      attempts: "0", needs_help: "false", worktree_cleaned: "false",
      gh_archived: "false", status: "new"}')
  printf '%s' "$sidecar" > "$(_sidecar_path "$num")"

  # Link as sub-issue if parent specified
  if [ -n "$parent_id" ] && [ "$parent_id" != "null" ]; then
    _sidecar_write "$num" "parent_id" "$parent_id"
    local parent_node child_node
    parent_node=$(gh_api "repos/$_GH_REPO/issues/$parent_id" -q '.node_id' 2>/dev/null || true)
    child_node=$(printf '%s' "$resp" | jq -r '.node_id')
    if [ -n "$parent_node" ] && [ -n "$child_node" ]; then
      gh api graphql \
        -H GraphQL-Features:sub_issues \
        -f parentIssueId="$parent_node" \
        -f childIssueId="$child_node" \
        -f query='mutation($parentIssueId: ID!, $childIssueId: ID!) {
          addSubIssue(input: {issueId: $parentIssueId, subIssueId: $childIssueId}) {
            issue { number }
            subIssue { number }
          }
        }' >/dev/null 2>&1 || true
    fi
  fi

  echo "$num"
}

# ============================================================
# Task multi-field update
# ============================================================

# Update multiple fields on a task at once.
# Usage: db_task_update <id> <field1>=<value1> [field2=value2] ...
db_task_update() {
  local id="$1"; shift

  local gh_patches=()
  local sidecar_json='{}'
  local new_status=""

  for pair in "$@"; do
    local field="${pair%%=*}"
    local value="${pair#*=}"

    case "$field" in
      title)       gh_patches+=(-f title="$value") ;;
      body|description) gh_patches+=(-f body="$value") ;;
      status)
        if [ "$value" = "NULL" ] || [ "$value" = "null" ]; then
          value="new"
        fi
        new_status="$value"
        sidecar_json=$(printf '%s' "$sidecar_json" | jq -c --arg v "$value" '.status = $v')
        ;;
      agent)
        if [ "$value" = "NULL" ] || [ "$value" = "null" ] || [ -z "$value" ]; then
          _gh_remove_prefixed_labels "$id" "$_GH_AGENT_PREFIX"
          sidecar_json=$(printf '%s' "$sidecar_json" | jq -c '.agent = null')
        else
          _gh_set_prefixed_label "$id" "$_GH_AGENT_PREFIX" "$value"
          sidecar_json=$(printf '%s' "$sidecar_json" | jq -c --arg v "$value" '.agent = $v')
        fi
        ;;
      complexity)
        # Sidecar only — no label
        if [ "$value" = "NULL" ] || [ "$value" = "null" ] || [ -z "$value" ]; then
          sidecar_json=$(printf '%s' "$sidecar_json" | jq -c '.complexity = null')
        else
          sidecar_json=$(printf '%s' "$sidecar_json" | jq -c --arg v "$value" '.complexity = $v')
        fi
        ;;
      *)
        if [ "$value" = "NULL" ] || [ "$value" = "null" ]; then
          sidecar_json=$(printf '%s' "$sidecar_json" | jq -c --arg k "$field" '.[$k] = null')
        else
          sidecar_json=$(printf '%s' "$sidecar_json" | jq -c --arg k "$field" --arg v "$value" '.[$k] = $v')
        fi
        ;;
    esac
  done

  # Apply GitHub patches
  if [ ${#gh_patches[@]} -gt 0 ]; then
    _gh_ensure_repo || return 0
    gh_api "repos/$_GH_REPO/issues/$id" -X PATCH "${gh_patches[@]}" >/dev/null 2>&1 || true
  fi

  # Apply status label change
  if [ -n "$new_status" ]; then
    _gh_set_status_label "$id" "$new_status"
  fi

  # Merge sidecar data
  if [ "$sidecar_json" != '{}' ]; then
    _sidecar_merge "$id" "$sidecar_json"
  fi
}

# ============================================================
# Task loading (single API call + parse)
# ============================================================

# Load all task fields into exported shell variables.
db_load_task() {
  local id="$1"
  _gh_ensure_repo || return 1

  local json
  json=$(gh_api "repos/$_GH_REPO/issues/$id" 2>/dev/null) || return 1
  [ -z "$json" ] && return 1
  # Skip PRs
  local is_pr
  is_pr=$(printf '%s' "$json" | jq -r '.pull_request // empty')
  [ -n "$is_pr" ] && return 1

  local labels_json
  labels_json=$(printf '%s' "$json" | jq -c '[.labels[].name]')

  # Core GitHub fields
  export TASK_TITLE=$(printf '%s' "$json" | jq -r '.title // ""')
  export TASK_BODY=$(printf '%s' "$json" | jq -r '.body // ""')

  local gh_status
  gh_status=$(_gh_status_from_labels "$labels_json")
  export TASK_STATUS="${gh_status:-new}"

  export TASK_LABELS=$(printf '%s' "$labels_json" | jq -r 'join(",")' 2>/dev/null || true)
  export TASK_AGENT=$(_gh_agent_from_labels "$labels_json")
  export TASK_COMPLEXITY=$(_gh_complexity_from_labels "$labels_json")
  export GH_ISSUE_NUMBER="$id"
  export TASK_GH_URL=$(printf '%s' "$json" | jq -r '.html_url // ""')
  export TASK_GH_STATE=$(printf '%s' "$json" | jq -r '.state // ""')
  export TASK_UPDATED_AT=$(printf '%s' "$json" | jq -r '.updated_at // ""')
  export TASK_CREATED_AT=$(printf '%s' "$json" | jq -r '.created_at // ""')

  # Sidecar fields
  local sc
  sc=$(_sidecar_full "$id")
  export AGENT_MODEL=$(printf '%s' "$sc" | jq -r '.agent_model // empty')
  export AGENT_PROFILE_JSON=$(printf '%s' "$sc" | jq -r '.agent_profile // "{}"')
  export ATTEMPTS=$(printf '%s' "$sc" | jq -r '.attempts // "0"')
  export TASK_PARENT_ID=$(printf '%s' "$sc" | jq -r '.parent_id // empty')
  export TASK_DIR=$(printf '%s' "$sc" | jq -r '.dir // empty')
  export TASK_BRANCH=$(printf '%s' "$sc" | jq -r '.branch // empty')
  export TASK_WORKTREE=$(printf '%s' "$sc" | jq -r '.worktree // empty')
  export TASK_SUMMARY=$(printf '%s' "$sc" | jq -r '.summary // empty')
  export TASK_REASON=$(printf '%s' "$sc" | jq -r '.reason // empty')
  export TASK_LAST_ERROR=$(printf '%s' "$sc" | jq -r '.last_error // empty')
  export TASK_PROMPT_HASH=$(printf '%s' "$sc" | jq -r '.prompt_hash // empty')
  export TASK_GH_SYNCED_AT=$(printf '%s' "$sc" | jq -r '.gh_synced_at // empty')
  export TASK_GH_STATE=$(printf '%s' "$json" | jq -r '.state // empty')
  export TASK_GH_UPDATED_AT=$(printf '%s' "$json" | jq -r '.updated_at // empty')
  export TASK_GH_LAST_FEEDBACK_AT=$(printf '%s' "$sc" | jq -r '.gh_last_feedback_at // empty')
  export TASK_GH_PROJECT_ITEM_ID=$(printf '%s' "$sc" | jq -r '.gh_project_item_id // empty')
  export TASK_GH_ARCHIVED=$(printf '%s' "$sc" | jq -r '.gh_archived // "false"')
  export TASK_NEEDS_HELP=$(printf '%s' "$sc" | jq -r '.needs_help // "false"')
  export TASK_LAST_COMMENT_HASH=$(printf '%s' "$sc" | jq -r '.last_comment_hash // empty')
  export TASK_REVIEW_DECISION=$(printf '%s' "$sc" | jq -r '.review_decision // empty')
  export TASK_REVIEW_NOTES=$(printf '%s' "$sc" | jq -r '.review_notes // empty')
  export TASK_RETRY_AT=$(printf '%s' "$sc" | jq -r '.retry_at // empty')
  export TASK_WORKTREE_CLEANED=$(printf '%s' "$sc" | jq -r '.worktree_cleaned // "false"')
  export TASK_ROUTE_REASON=$(printf '%s' "$sc" | jq -r '.route_reason // empty')
  export TASK_ROUTE_WARNING=$(printf '%s' "$sc" | jq -r '.route_warning // empty')
  export TASK_DURATION=$(printf '%s' "$sc" | jq -r '.duration // "0"')
  export TASK_INPUT_TOKENS=$(printf '%s' "$sc" | jq -r '.input_tokens // "0"')
  export TASK_OUTPUT_TOKENS=$(printf '%s' "$sc" | jq -r '.output_tokens // "0"')
  export TASK_STDERR_SNIPPET=$(printf '%s' "$sc" | jq -r '.stderr_snippet // empty')
  export TASK_GH_SYNCED_STATUS=$(printf '%s' "$sc" | jq -r '.gh_synced_status // empty')
  export TASK_DECOMPOSE=$(printf '%s' "$sc" | jq -r '.decompose // "0"')

  # Role from agent profile
  ROLE=$(printf '%s' "$AGENT_PROFILE_JSON" | jq -r '.role // "general"' 2>/dev/null || echo "general")
  export ROLE

  # Selected skills
  export SELECTED_SKILLS
  SELECTED_SKILLS=$(printf '%s' "$sc" | jq -r '.selected_skills // ""')
}

# ============================================================
# Task array fields (labels, history, files, etc.)
# ============================================================

# Append a history entry as an issue comment.
db_append_history() {
  local task_id="$1" _ah_status="$2" note="$3"
  _gh_ensure_repo || return 0
  local now
  now=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  gh_api "repos/$_GH_REPO/issues/$task_id/comments" \
    -f body="[$now] $_ah_status: $note" >/dev/null 2>&1 || true
}

# Get task history from comments.
db_task_history() {
  local task_id="$1"
  _gh_ensure_repo || return 0
  gh_api -X GET "repos/$_GH_REPO/issues/$task_id/comments" -f per_page=100 2>/dev/null \
    | jq -r '.[].body' 2>/dev/null || true
}

# Get task history as formatted strings (last N entries).
db_task_history_formatted() {
  local id="$1" limit="${2:-5}"
  _gh_ensure_repo || return 0
  gh_api -X GET "repos/$_GH_REPO/issues/$id/comments" -f per_page=100 2>/dev/null \
    | jq -r ".[-${limit}:] | .[].body" 2>/dev/null || true
}

# Get task labels as newline-separated list.
db_task_labels() {
  local task_id="$1"
  _gh_ensure_repo || return 0
  gh_api "repos/$_GH_REPO/issues/$task_id" --cache 60s -q '[.labels[].name] | .[]' 2>/dev/null || true
}

# Get task labels as comma-separated string.
db_task_labels_csv() {
  local task_id="$1"
  _gh_ensure_repo || return 0
  gh_api "repos/$_GH_REPO/issues/$task_id" --cache 60s -q '[.labels[].name] | join(",")' 2>/dev/null || true
}

# Get task labels as JSON array.
db_task_labels_json() {
  local task_id="$1"
  _gh_ensure_repo || return 0
  gh_api "repos/$_GH_REPO/issues/$task_id" --cache 60s -q '[.labels[].name]' 2>/dev/null || echo '[]'
}

# Set labels for a task (replaces existing).
db_set_labels() {
  local task_id="$1" labels_csv="$2"
  _gh_ensure_repo || return 0
  local labels_json='[]'
  if [ -n "$labels_csv" ]; then
    labels_json=$(printf '%s' "$labels_csv" | jq -Rc 'split(",") | map(select(length > 0) | gsub("^\\s+|\\s+$"; ""))')
  fi
  gh_api "repos/$_GH_REPO/issues/$task_id" -X PATCH \
    --input - <<< "{\"labels\":$(printf '%s' "$labels_json")}" >/dev/null 2>&1 || true
}

# Add a label to a task.
db_add_label() {
  local task_id="$1" label="$2"
  _gh_validate_label "$label" || return 1
  _gh_ensure_repo || return 0
  _gh_ensure_label "$label" "c5def5" ""
  gh_api "repos/$_GH_REPO/issues/$task_id/labels" \
    --input - <<< "{\"labels\":[\"$label\"]}" >/dev/null 2>&1 || true
}

# Remove a label from a task.
db_remove_label() {
  local task_id="$1" label="$2"
  _gh_ensure_repo || return 0
  local encoded
  encoded=$(printf '%s' "$label" | jq -sRr @uri)
  gh_api "repos/$_GH_REPO/issues/$task_id/labels/$encoded" -X DELETE >/dev/null 2>&1 || true
}

# Check if a task has a specific label.
db_task_has_label() {
  local task_id="$1" label="$2"
  local labels
  labels=$(db_task_labels_csv "$task_id")
  printf '%s' ",$labels," | grep -q ",$label,"
}

# Set files_changed (in sidecar).
db_set_files() {
  local task_id="$1" files_csv="$2"
  local files_json
  files_json=$(printf '%s' "$files_csv" | jq -Rc 'split(",") | map(select(length > 0) | gsub("^\\s+|\\s+$"; ""))')
  _sidecar_write_json "$task_id" "files_changed" "$files_json"
}

# Get files_changed as CSV.
db_task_files_csv() {
  local task_id="$1"
  local files
  files=$(_sidecar_read "$task_id" "files_changed")
  if [ -n "$files" ]; then
    printf '%s' "$files" | jq -r 'if type == "array" then join(", ") else . end' 2>/dev/null || echo "$files"
  fi
}

# Get child task IDs (via sub-issues API).
db_task_children() {
  local parent_id="$1"
  _gh_ensure_repo || return 0
  local node_id
  node_id=$(gh_api "repos/$_GH_REPO/issues/$parent_id" -q '.node_id' 2>/dev/null || true)
  [ -z "$node_id" ] && return 0
  gh api graphql \
    -H GraphQL-Features:sub_issues \
    -f parentId="$node_id" \
    -f query='query($parentId: ID!) {
      node(id: $parentId) {
        ... on Issue {
          subIssues(first: 50) {
            nodes { number }
          }
        }
      }
    }' 2>/dev/null | jq -r '.data.node.subIssues.nodes[].number' 2>/dev/null || true
}

# Set accomplished items (in sidecar).
db_set_accomplished() {
  local task_id="$1"; shift
  local items_json='[]'
  for item in "$@"; do
    [ -z "$item" ] && continue
    items_json=$(printf '%s' "$items_json" | jq -c --arg i "$item" '. + [$i]')
  done
  _sidecar_write_json "$task_id" "accomplished" "$items_json"
}

# Set remaining items (in sidecar).
db_set_remaining() {
  local task_id="$1"; shift
  local items_json='[]'
  for item in "$@"; do
    [ -z "$item" ] && continue
    items_json=$(printf '%s' "$items_json" | jq -c --arg i "$item" '. + [$i]')
  done
  _sidecar_write_json "$task_id" "remaining" "$items_json"
}

# Set blockers (in sidecar).
db_set_blockers() {
  local task_id="$1"; shift
  local items_json='[]'
  for item in "$@"; do
    [ -z "$item" ] && continue
    items_json=$(printf '%s' "$items_json" | jq -c --arg i "$item" '. + [$i]')
  done
  _sidecar_write_json "$task_id" "blockers" "$items_json"
}

# Set selected_skills (in sidecar).
db_set_selected_skills() {
  local task_id="$1" skills_csv="$2"
  _sidecar_write "$task_id" "selected_skills" "$skills_csv"
}

# ============================================================
# Agent response storage
# ============================================================

# Store agent response — updates sidecar + posts structured comment.
db_store_agent_response() {
  local id="$1" status="$2" summary="$3" reason="$4"
  local needs_help="$5" agent_model="$6" duration="$7"
  local input_tokens="$8" output_tokens="$9"
  shift 9
  local stderr_snippet="${1:-}" prompt_hash="${2:-}"

  # Update sidecar
  local meta
  meta=$(jq -nc --arg s "$summary" --arg r "$reason" --arg nh "$needs_help" \
    --arg am "$agent_model" --arg d "$duration" --arg it "$input_tokens" \
    --arg ot "$output_tokens" --arg ss "$stderr_snippet" --arg ph "$prompt_hash" \
    '{summary: $s, reason: $r, needs_help: $nh, agent_model: $am,
      duration: $d, input_tokens: $it, output_tokens: $ot,
      stderr_snippet: $ss, prompt_hash: $ph,
      last_error: null, retry_at: null, status: "'"$status"'"}')
  _sidecar_merge "$id" "$meta"

  # Set status label
  _gh_set_status_label "$id" "$status"
}

# Store array data from agent response.
db_store_agent_arrays() {
  local id="$1" accomplished="$2" remaining="$3" blockers="$4" files_changed="$5"
  local meta='{}'

  if [ -n "$accomplished" ]; then
    meta=$(printf '%s' "$meta" | jq -c --arg a "$accomplished" '.accomplished = ($a | split("\n") | map(select(length > 0)))')
  else
    meta=$(printf '%s' "$meta" | jq -c '.accomplished = []')
  fi

  if [ -n "$remaining" ]; then
    meta=$(printf '%s' "$meta" | jq -c --arg r "$remaining" '.remaining = ($r | split("\n") | map(select(length > 0)))')
  else
    meta=$(printf '%s' "$meta" | jq -c '.remaining = []')
  fi

  if [ -n "$blockers" ]; then
    meta=$(printf '%s' "$meta" | jq -c --arg b "$blockers" '.blockers = ($b | split("\n") | map(select(length > 0)))')
  else
    meta=$(printf '%s' "$meta" | jq -c '.blockers = []')
  fi

  if [ -n "$files_changed" ]; then
    meta=$(printf '%s' "$meta" | jq -c --arg f "$files_changed" '.files_changed = ($f | split("\n") | map(select(length > 0)))')
  else
    meta=$(printf '%s' "$meta" | jq -c '.files_changed = []')
  fi

  _sidecar_merge "$id" "$meta"

  # Post structured comment to GitHub
  _gh_post_agent_comment "$id"
}

# Post the structured agent response comment on the issue.
_gh_post_agent_comment() {
  local id="$1"
  _gh_ensure_repo || return 0

  local sc
  sc=$(_sidecar_full "$id")

  local status summary agent agent_model duration input_tokens output_tokens prompt_hash
  local reason last_error needs_help
  status=$(printf '%s' "$sc" | jq -r '.status // ""')
  summary=$(printf '%s' "$sc" | jq -r '.summary // ""')
  agent=$(printf '%s' "$sc" | jq -r '.agent // ""')
  agent_model=$(printf '%s' "$sc" | jq -r '.agent_model // ""')
  duration=$(printf '%s' "$sc" | jq -r '.duration // "0"')
  input_tokens=$(printf '%s' "$sc" | jq -r '.input_tokens // "0"')
  output_tokens=$(printf '%s' "$sc" | jq -r '.output_tokens // "0"')
  prompt_hash=$(printf '%s' "$sc" | jq -r '.prompt_hash // ""')
  reason=$(printf '%s' "$sc" | jq -r '.reason // ""')
  last_error=$(printf '%s' "$sc" | jq -r '.last_error // ""')
  needs_help=$(printf '%s' "$sc" | jq -r '.needs_help // "false"')
  local attempts
  attempts=$(printf '%s' "$sc" | jq -r '.attempts // "0"')

  local badge
  badge=$(agent_badge "$agent")
  local is_blocked=false
  if [ "$status" = "blocked" ] || [ "$status" = "needs_review" ]; then
    is_blocked=true
  fi

  # Only post comment if there's meaningful content
  if [ "$is_blocked" = false ] && [ -z "$summary" ]; then
    return 0
  fi

  local comment="${_GH_COMMENT_MARKER}
"

  if [ "$is_blocked" = true ]; then
    comment+="## ${badge} Needs Help
"
  elif [ -n "$summary" ]; then
    comment+="## ${badge} ${summary}
"
  fi

  comment+="
| | |
|---|---|
| **Status** | \`${status}\` |
| **Agent** | ${agent:-unknown} |"
  [ -n "$agent_model" ] && comment+="
| **Model** | \`${agent_model}\` |"
  comment+="
| **Attempt** | ${attempts} |"
  if [ "$duration" -gt 0 ] 2>/dev/null; then
    comment+="
| **Duration** | $(duration_fmt "$duration") |"
  fi
  if [ "$input_tokens" -gt 0 ] 2>/dev/null; then
    local in_fmt="$input_tokens" out_fmt="$output_tokens"
    [ "$input_tokens" -ge 1000 ] && in_fmt="$((input_tokens / 1000))k"
    [ "$output_tokens" -ge 1000 ] && out_fmt="$((output_tokens / 1000))k"
    comment+="
| **Tokens** | ${in_fmt} in / ${out_fmt} out |"
  fi
  [ -n "$prompt_hash" ] && comment+="
| **Prompt** | \`${prompt_hash}\` |"

  if [ "$is_blocked" = true ] && [ -n "$summary" ]; then
    comment+="

${summary}"
  fi

  # Errors & Blockers section
  local blockers_list=""
  blockers_list=$(printf '%s' "$sc" | jq -r '.blockers // [] | .[] | "- " + .' 2>/dev/null || true)
  if [ -n "$reason" ] || [ -n "$last_error" ] || [ -n "$blockers_list" ]; then
    comment+="

### Errors & Blockers"
    [ -n "$reason" ] && comment+="

**Reason:** ${reason}"
    [ -n "$last_error" ] && comment+="

> \`${last_error}\`"
    [ -n "$blockers_list" ] && comment+="

${blockers_list}"
  fi

  # Accomplished
  local accomplished_list=""
  accomplished_list=$(printf '%s' "$sc" | jq -r '.accomplished // [] | .[] | "- " + .' 2>/dev/null || true)
  [ -n "$accomplished_list" ] && comment+="

### Accomplished

${accomplished_list}"

  # Remaining
  local remaining_list=""
  remaining_list=$(printf '%s' "$sc" | jq -r '.remaining // [] | .[] | "- " + .' 2>/dev/null || true)
  [ -n "$remaining_list" ] && comment+="

### Remaining

${remaining_list}"

  # Files Changed
  local files_list=""
  files_list=$(printf '%s' "$sc" | jq -r '.files_changed // [] | .[] | "- `" + . + "`"' 2>/dev/null || true)
  [ -n "$files_list" ] && comment+="

### Files Changed

${files_list}"

  if [ "$is_blocked" = true ]; then
    comment+="

> ⚠️ This task needs your attention."
  fi

  # Footer
  local model_suffix=""
  [ -n "$agent_model" ] && model_suffix=" using model \`${agent_model}\`"
  comment+="

---
*By ${agent:-orchestrator}[bot]${model_suffix} via [Orchestrator](https://github.com/gabrielkoerich/orchestrator)*"

  # Dedup check
  local new_hash
  new_hash=$(printf '%s' "$comment" | shasum -a 256 | cut -c1-16)
  local old_hash
  old_hash=$(_sidecar_read "$id" "last_comment_hash")
  if [ "$new_hash" = "$old_hash" ]; then
    return 0
  fi

  gh_api "repos/$_GH_REPO/issues/$id/comments" -f body="$comment" >/dev/null 2>&1 || true
  _sidecar_write "$id" "last_comment_hash" "$new_hash"

  # If this task was created from a GitHub mention, also mirror the result back
  # to the original issue/PR thread (deduped separately from the task issue).
  local mention_target_issue mention_target_repo
  mention_target_issue=$(printf '%s' "$sc" | jq -r '.mention_target_issue // empty' 2>/dev/null || true)
  mention_target_repo=$(printf '%s' "$sc" | jq -r '.mention_target_repo // empty' 2>/dev/null || true)
  if [ -n "$mention_target_issue" ] && [ "$mention_target_issue" != "null" ] && [ "$mention_target_issue" != "$id" ]; then
    local target_repo="${mention_target_repo:-$_GH_REPO}"
    if [ -n "$target_repo" ] && [ "$target_repo" != "null" ]; then
      local mirror_comment
      mirror_comment="${_GH_COMMENT_MARKER}
> Mention task: #${id}
"
      # Reuse the generated report without repeating the marker line.
      mirror_comment+=$(printf '%s' "$comment" | sed '1d')

      local mirror_hash old_mirror_hash
      mirror_hash=$(printf '%s' "$mirror_comment" | shasum -a 256 | cut -c1-16)
      old_mirror_hash=$(_sidecar_read "$id" "mention_last_mirror_hash")
      if [ "$mirror_hash" != "$old_mirror_hash" ]; then
        gh_api "repos/${target_repo}/issues/${mention_target_issue}/comments" -f body="$mirror_comment" >/dev/null 2>&1 || true
        _sidecar_write "$id" "mention_last_mirror_hash" "$mirror_hash"
      fi
    fi
  fi
}

# ============================================================
# Query helpers
# ============================================================

# Get a full task as JSON.
db_task_json() {
  local id="$1"
  _gh_ensure_repo || return 1
  local json
  json=$(gh_api "repos/$_GH_REPO/issues/$id" 2>/dev/null) || return 1
  local sc
  sc=$(_sidecar_full "$id")
  printf '%s' "$json" | jq --argjson sc "$sc" '. + {sidecar: $sc}'
}

# Find task ID by GitHub issue number (identity for GitHub backend).
db_task_id_by_gh_issue() {
  local issue_num="$1"
  echo "$issue_num"
}

# Find task ID by branch + dir (search sidecar files).
db_task_id_by_branch() {
  local branch="$1" dir="$2"
  _sidecar_ensure
  for f in "$(_gh_sidecar_dir)"/*.json; do
    [ -f "$f" ] || continue
    local b d
    b=$(jq -r '.branch // empty' "$f" 2>/dev/null || true)
    d=$(jq -r '.dir // empty' "$f" 2>/dev/null || true)
    if [ "$b" = "$branch" ] && [ "$d" = "$dir" ]; then
      basename "$f" .json
      return
    fi
  done
}

# Get dirty tasks that need sync (not needed — GitHub IS the source of truth).
db_dirty_task_ids() { :; }
db_dirty_task_count() { echo 0; }

# Mark a task as synced (no-op for GitHub backend — already synced).
db_task_set_synced() { :; }

# Get all task IDs (open issues).
db_all_task_ids() {
  _gh_ensure_repo || return 0
  gh_api -X GET "repos/$_GH_REPO/issues" -f state=open -f per_page=100 2>/dev/null \
    | jq -r '.[] | select(.pull_request == null) | .number' 2>/dev/null || true
}

# Total task count.
db_total_task_count() {
  _gh_ensure_repo || { echo 0; return; }
  local count
  count=$(gh_api -X GET "repos/$_GH_REPO/issues" -f state=all -f per_page=1 2>/dev/null \
    | jq 'length' 2>/dev/null || echo 0)
  # For total count, use search API
  gh_api -X GET "search/issues" -f q="repo:$_GH_REPO is:issue" -f per_page=1 -q '.total_count' 2>/dev/null || echo 0
}

# Comment dedup check.
db_should_skip_comment() {
  local task_id="$1" body="$2"
  local new_hash
  new_hash=$(printf '%s' "$body" | shasum -a 256 | cut -c1-16)
  local old_hash
  old_hash=$(_sidecar_read "$task_id" "last_comment_hash")
  [ "$new_hash" = "$old_hash" ]
}

# Store comment hash after posting.
db_store_comment_hash() {
  local task_id="$1" body="$2"
  local hash
  hash=$(printf '%s' "$body" | shasum -a 256 | cut -c1-16)
  _sidecar_write "$task_id" "last_comment_hash" "$hash"
}

# Get accomplished items as newline-separated list.
db_task_accomplished() {
  local id="$1"
  printf '%s' "$(_sidecar_full "$id")" | jq -r '.accomplished // [] | .[]' 2>/dev/null || true
}

db_task_remaining() {
  local id="$1"
  printf '%s' "$(_sidecar_full "$id")" | jq -r '.remaining // [] | .[]' 2>/dev/null || true
}

db_task_blockers() {
  local id="$1"
  printf '%s' "$(_sidecar_full "$id")" | jq -r '.blockers // [] | .[]' 2>/dev/null || true
}

db_task_files() {
  local id="$1"
  printf '%s' "$(_sidecar_full "$id")" | jq -r '.files_changed // [] | .[]' 2>/dev/null || true
}

# Display helpers — formatted lists
db_task_accomplished_list() {
  local id="$1"
  printf '%s' "$(_sidecar_full "$id")" | jq -r '.accomplished // [] | .[] | "- " + .' 2>/dev/null || true
}

db_task_remaining_list() {
  local id="$1"
  printf '%s' "$(_sidecar_full "$id")" | jq -r '.remaining // [] | .[] | "- " + .' 2>/dev/null || true
}

db_task_blockers_list() {
  local id="$1"
  printf '%s' "$(_sidecar_full "$id")" | jq -r '.blockers // [] | .[] | "- " + .' 2>/dev/null || true
}

db_task_files_list() {
  local id="$1"
  printf '%s' "$(_sidecar_full "$id")" | jq -r '.files_changed // [] | .[] | "- `" + . + "`"' 2>/dev/null || true
}

# Create task from GitHub issue.
# In the GitHub backend, issues ARE the tasks. This is an identity operation.
db_create_task_from_gh() {
  local title="$1" body="$2" labels_csv="$3" gh_issue_number="$4"
  local gh_state="$5" gh_url="$6" gh_updated_at="$7" dir="${8:-${PROJECT_DIR:-}}"

  # Initialize sidecar for this existing issue
  _sidecar_ensure
  local sidecar
  sidecar=$(jq -nc --arg dir "$dir" --arg ghs "$gh_state" --arg ghu "$gh_url" --arg ghup "$gh_updated_at" \
    '{dir: $dir, gh_state: $ghs, gh_url: $ghu, gh_updated_at: $ghup,
      attempts: "0", needs_help: "false", worktree_cleaned: "false",
      gh_archived: "false", status: "new"}')
  printf '%s' "$sidecar" > "$(_sidecar_path "$gh_issue_number")"

  echo "$gh_issue_number"
}

# ============================================================
# Display / query helpers
# ============================================================

# DIR filter is simpler for GitHub — we filter by sidecar dir field
db_dir_where() {
  local project_dir="${PROJECT_DIR:-}"
  local orch_home="${ORCH_HOME:-$HOME/.orchestrator}"
  if [ -z "$project_dir" ] || [ "$project_dir" = "$orch_home" ]; then
    echo ""  # no filter
  else
    echo "$project_dir"
  fi
}

# Count tasks by status.
db_status_count() {
  local status="$1"
  _gh_ensure_repo || { echo 0; return; }
  local state="open"
  [ "$status" = "done" ] && state="closed"
  local query="repo:$_GH_REPO is:issue is:$state label:${_GH_STATUS_PREFIX}${status}"
  gh_api -X GET "search/issues" -f q="$query" -f per_page=1 -q '.total_count' 2>/dev/null || echo 0
}

# Total filtered count.
db_total_filtered_count() {
  _gh_ensure_repo || { echo 0; return; }
  gh_api -X GET "search/issues" -f q="repo:$_GH_REPO is:issue is:open" -f per_page=1 -q '.total_count' 2>/dev/null || echo 0
}

# List tasks as TSV for table display.
db_task_display_tsv() {
  local extra_filter="${1:-true}" order="${2:-id}" limit="${3:-}"
  _gh_ensure_repo || return 0

  local json
  json=$(gh_api -X GET "repos/$_GH_REPO/issues" -f state=open -f per_page="${limit:-100}" -f sort=created -f direction=desc 2>/dev/null || echo '[]')
  json=$(printf '%s' "$json" | jq -s 'if type == "array" and length > 0 and (.[0] | type) == "array" then [.[][]] else . end' 2>/dev/null || echo "$json")

  printf '%s' "$json" | jq -r '.[] | select(.pull_request == null) | [
    (.number | tostring),
    ((.labels // []) | map(.name) | map(select(startswith("status:"))) | .[0] // "status:new" | ltrimstr("status:")),
    ((.labels // []) | map(.name) | map(select(startswith("agent:"))) | .[0] // "-" | ltrimstr("agent:")),
    "#\(.number)",
    .title
  ] | @tsv' 2>/dev/null || true
}

# List tasks as TSV with project column (global view).
db_task_display_tsv_global() {
  local extra_filter="${1:-true}" order="${2:-updated_at}" limit="${3:-10}"
  # For global view, same as display_tsv but with project column
  _gh_ensure_repo || return 0

  local json
  json=$(gh_api -X GET "repos/$_GH_REPO/issues" -f state=open -f per_page="$limit" -f sort=updated -f direction=desc 2>/dev/null || echo '[]')
  json=$(printf '%s' "$json" | jq -s 'if type == "array" and length > 0 and (.[0] | type) == "array" then [.[][]] else . end' 2>/dev/null || echo "$json")

  local repo_name
  repo_name=$(printf '%s' "$_GH_REPO" | cut -d/ -f2)

  printf '%s' "$json" | jq -r --arg repo "$repo_name" '.[] | select(.pull_request == null) | [
    (.number | tostring),
    ((.labels // []) | map(.name) | map(select(startswith("status:"))) | .[0] // "status:new" | ltrimstr("status:")),
    ((.labels // []) | map(.name) | map(select(startswith("agent:"))) | .[0] // "-" | ltrimstr("agent:")),
    "#\(.number)",
    $repo,
    .title
  ] | @tsv' 2>/dev/null || true
}

# Token usage TSV (from sidecar data).
db_task_usage_tsv() {
  _sidecar_ensure
  for f in "$(_gh_sidecar_dir)"/*.json; do
    [ -f "$f" ] || continue
    local it ot dur model
    it=$(jq -r '.input_tokens // "0"' "$f" 2>/dev/null)
    ot=$(jq -r '.output_tokens // "0"' "$f" 2>/dev/null)
    dur=$(jq -r '.duration // "0"' "$f" 2>/dev/null)
    model=$(jq -r '.agent_model // "sonnet"' "$f" 2>/dev/null)
    if [ "$it" != "0" ] && [ "$it" != "null" ] && [ -n "$it" ]; then
      printf '%s\t%s\t%s\t%s\n' "$it" "$ot" "$dur" "$model"
    fi
  done
}

# Unique project dirs.
db_task_projects() {
  {
    _read_jobs | jq -r '.[].dir // empty' 2>/dev/null || true
    [ -n "${PROJECT_DIR:-}" ] && echo "$PROJECT_DIR"
  } | sort -u | while IFS= read -r d; do
    [ -n "$d" ] && [ "$d" != "null" ] && [ -d "$d" ] && echo "$d"
  done
}

# Active task count for a specific dir.
db_task_active_count_for_dir() {
  local dir="$1"
  _sidecar_ensure
  local count=0
  for f in "$(_gh_sidecar_dir)"/*.json; do
    [ -f "$f" ] || continue
    local d s
    d=$(jq -r '.dir // empty' "$f" 2>/dev/null)
    s=$(jq -r '.status // "new"' "$f" 2>/dev/null)
    if [ "$d" = "$dir" ] && [ "$s" != "done" ]; then
      count=$((count + 1))
    fi
  done
  echo "$count"
}

# Root task IDs (no parent — issues without sub-issue parent).
db_task_roots() {
  _gh_ensure_repo || return 0
  # For simplicity, return all open issues (sub-issue detection is expensive)
  gh_api -X GET "repos/$_GH_REPO/issues" -f state=open -f per_page=100 2>/dev/null \
    | jq -r '.[] | select(.pull_request == null) | .number' 2>/dev/null || true
}

# Status JSON output.
db_status_json() {
  local _dummy="${1:-}"
  _gh_ensure_repo || { echo '{"total":0,"counts":{},"recent":[]}'; return; }

  local new routed in_progress in_review blocked done needs_review
  new=$(db_status_count "new")
  routed=$(db_status_count "routed")
  in_progress=$(db_status_count "in_progress")
  in_review=$(db_status_count "in_review")
  blocked=$(db_status_count "blocked")
  done=$(db_status_count "done")
  needs_review=$(db_status_count "needs_review")
  local open=$((new + routed + in_progress + in_review + blocked + needs_review))
  local total=$((open + done))

  # Recent tasks
  local recent_json
  recent_json=$(gh_api -X GET "repos/$_GH_REPO/issues" -f state=open -f per_page=10 -f sort=updated -f direction=desc 2>/dev/null || echo '[]')
  recent_json=$(printf '%s' "$recent_json" | jq -c '[.[] | select(.pull_request == null) | {
    id: (.number | tostring),
    title: .title,
    status: ((.labels // []) | map(.name) | map(select(startswith("status:"))) | .[0] // "status:new" | ltrimstr("status:")),
    agent: ((.labels // []) | map(.name) | map(select(startswith("agent:"))) | .[0] // null | if . then ltrimstr("agent:") else null end),
    gh_issue_number: (.number | tostring),
    updated_at: .updated_at
  }]' 2>/dev/null || echo '[]')

  jq -nc --argjson t "$total" --argjson o "$open" \
    --argjson n "$new" --argjson r "$routed" --argjson ip "$in_progress" --argjson ir "$in_review" \
    --argjson b "$blocked" --argjson d "$done" --argjson nr "$needs_review" \
    --argjson recent "$recent_json" \
    '{total: $t, open: $o, counts: {new: $n, routed: $r, in_progress: $ip, in_review: $ir, blocked: $b, done: $d, needs_review: $nr}, recent: $recent}'
}

# Tasks with branches (for review_prs.sh).
db_tasks_with_branches() {
  _sidecar_ensure
  for f in "$(_gh_sidecar_dir)"/*.json; do
    [ -f "$f" ] || continue
    local id branch status title summary worktree agent gh_num
    id=$(basename "$f" .json)
    branch=$(jq -r '.branch // empty' "$f" 2>/dev/null)
    [ -z "$branch" ] || [ "$branch" = "main" ] || [ "$branch" = "master" ] && continue
    status=$(jq -r '.status // "new"' "$f" 2>/dev/null)
    case "$status" in
      done|in_review|in_progress|blocked) ;;
      *) continue ;;
    esac
    title=$(jq -r '.title // ""' "$f" 2>/dev/null)
    summary=$(jq -r '.summary // ""' "$f" 2>/dev/null)
    worktree=$(jq -r '.worktree // ""' "$f" 2>/dev/null)
    agent=$(jq -r '.agent // ""' "$f" 2>/dev/null)
    gh_num="$id"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$id" "$branch" "$status" "$title" "$summary" "$worktree" "$agent" "$gh_num"
  done
}

# ============================================================
# Projects V2 support
# ============================================================

map_status_to_project() {
  local status="$1"
  case "$status" in
    new|routed)        echo "backlog" ;;
    in_progress|blocked) echo "in_progress" ;;
    in_review|needs_review) echo "review" ;;
    done)              echo "done" ;;
    *)                 echo "backlog" ;;
  esac
}

sync_project_status() {
  local issue_number="$1" status="$2"
  _gh_ensure_repo || return 0

  local project_id project_status_field_id project_status_map_json
  project_id=$(config_get '.gh.project_id // ""' 2>/dev/null || true)
  project_status_field_id=$(config_get '.gh.project_status_field_id // ""' 2>/dev/null || true)
  project_status_map_json=$(yq -o=json -r '.gh.project_status_map // {}' "${CONFIG_PATH:-}" 2>/dev/null | jq -c '.' 2>/dev/null || echo '{}')

  [ -z "$project_id" ] || [ -z "$project_status_field_id" ] || [ "$project_status_map_json" = "{}" ] && return 0

  local key
  key=$(map_status_to_project "$status")
  local option_id
  option_id=$(printf '%s' "$project_status_map_json" | jq -r ".\"$key\" // \"\"")
  [ -z "$option_id" ] || [ "$option_id" = "null" ] && return 0

  local issue_node
  issue_node=$(gh_api "repos/$_GH_REPO/issues/$issue_number" -q .node_id 2>/dev/null || true)
  [ -z "$issue_node" ] && return 0

  local items_json
  items_json=$(gh_api graphql -f query='query($project:ID!){ node(id:$project){ ... on ProjectV2 { items(first:100){ nodes{ id content{ ... on Issue { id } } } } } } }' -f project="$project_id" 2>/dev/null || true)

  local item_id
  item_id=$(printf '%s' "$items_json" | jq -r ".data.node.items.nodes[] | select(.content.id == \"$issue_node\") | .id" 2>/dev/null | head -n1)
  if [ -z "$item_id" ] || [ "$item_id" = "null" ]; then
    local add_json
    add_json=$(gh_api graphql \
      -f query='mutation($project:ID!,$contentId:ID!){ addProjectV2ItemById(input:{projectId:$project, contentId:$contentId}){ item{ id } } }' \
      -f project="$project_id" -f contentId="$issue_node" 2>/dev/null || true)
    item_id=$(printf '%s' "$add_json" | jq -r '.data.addProjectV2ItemById.item.id // ""' 2>/dev/null)
    [ -z "$item_id" ] || [ "$item_id" = "null" ] && return 0
  fi

  gh_api graphql -f query='mutation($project:ID!, $item:ID!, $field:ID!, $option:String!){ updateProjectV2ItemFieldValue(input:{projectId:$project, itemId:$item, fieldId:$field, value:{singleSelectOptionId:$option}}){ projectV2Item{id} } }' \
    -f project="$project_id" -f item="$item_id" -f field="$project_status_field_id" -f option="$option_id" >/dev/null 2>&1 || true
}

archive_project_item() {
  local project_id="$1" item_id="$2"
  gh_api graphql -f query='
    mutation($projectId: ID!, $itemId: ID!) {
      archiveProjectV2Item(input: { projectId: $projectId, itemId: $itemId }) {
        item { id }
      }
    }
  ' -f projectId="$project_id" -f itemId="$item_id" 2>/dev/null || true
}

# Ensure label helper (public, used by some scripts).
ensure_label() {
  _gh_ensure_label "$@"
}

# Read the saved prompt file for a task.
read_prompt_file() {
  local task_dir="$1" task_id="$2"
  local state_dir="${task_dir:+${task_dir}/.orchestrator}"
  state_dir="${state_dir:-${STATE_DIR:-.orchestrator}}"
  local prompt_file="${state_dir}/prompt-${task_id}.txt"
  if [ -f "$prompt_file" ]; then
    head -c 10000 "$prompt_file"
  fi
}

# Build a condensed tool activity summary.
read_tool_summary() {
  local task_dir="$1" task_id="$2"
  local state_dir="${task_dir:+${task_dir}/.orchestrator}"
  state_dir="${state_dir:-${STATE_DIR:-.orchestrator}}"
  local tools_file="${state_dir}/tools-${task_id}.json"
  if [ ! -f "$tools_file" ] || [ ! -s "$tools_file" ]; then return; fi

  python3 - "$tools_file" <<'PY' 2>/dev/null || true
import json, sys
try:
    with open(sys.argv[1]) as f:
        history = json.load(f)
except Exception:
    sys.exit(0)
if not history:
    sys.exit(0)

counts = {}
errors = []
for h in history:
    tool = h.get("tool", "?")
    counts[tool] = counts.get(tool, 0) + 1
    if h.get("error"):
        inp = h.get("input", {})
        if tool == "Bash":
            detail = inp.get("command", "?")[:120]
        elif tool in ("Edit", "Write", "Read"):
            detail = inp.get("file_path", "?")
        else:
            detail = tool
        errors.append(f"- `{detail}`")

lines = ["| Tool | Calls |", "|------|-------|"]
for tool in sorted(counts, key=lambda t: -counts[t]):
    lines.append(f"| {tool} | {counts[tool]} |")
print("\n".join(lines))

if errors:
    print("")
    print(f"<details><summary>Failed tool calls ({len(errors)})</summary>")
    print("")
    print("\n".join(errors[:10]))
    print("")
    print("</details>")
PY
}

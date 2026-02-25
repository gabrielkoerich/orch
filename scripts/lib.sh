#!/usr/bin/env bash
set -euo pipefail

ORCH_HOME="${ORCH_HOME:-$HOME/.orchestrator}"
mkdir -p "$ORCH_HOME"

ORCH_WORKTREES="${ORCH_WORKTREES:-${ORCH_HOME}/worktrees}"
LOCK_PATH=${LOCK_PATH:-"${ORCH_HOME}/.orchestrator/locks"}
CONTEXTS_DIR=${CONTEXTS_DIR:-"${ORCH_HOME}/contexts"}
CONFIG_PATH=${CONFIG_PATH:-"${ORCH_HOME}/config.yml"}
STATE_DIR=${STATE_DIR:-"${ORCH_HOME}/.orchestrator"}
# Source backend layer (GitHub, etc.)
_LIB_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
if [ -f "${_LIB_DIR}/backend.sh" ]; then
  source "${_LIB_DIR}/backend.sh"
fi

now_iso() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

now_epoch() {
  date -u +"%s"
}

_log_prefix() {
  if [ -n "${ORCH_VERSION:-}" ]; then
    echo "$(now_iso) [v${ORCH_VERSION}]"
  else
    echo "$(now_iso)"
  fi
}

log() {
  echo "$(_log_prefix) $*"
}

# Log to stderr (for scripts whose stdout is consumed by callers)
log_err() {
  echo "$(_log_prefix) $*" >&2
}

# Log to the error log file (agent errors, stuck agents, auth issues)
error_log() {
  local error_file="${STATE_DIR}/orchestrator.error.log"
  echo "$(_log_prefix) $*" >> "$error_file"
  log_err "$@"
}

duration_fmt() {
  local secs="${1:-0}"
  if [ "$secs" -lt 60 ]; then
    echo "${secs}s"
  elif [ "$secs" -lt 3600 ]; then
    echo "$((secs / 60))m $((secs % 60))s"
  else
    echo "$((secs / 3600))h $((secs % 3600 / 60))m"
  fi
}

ensure_state_dir() {
  mkdir -p "$STATE_DIR"
}

gh_backoff_path() {
  echo "${GH_BACKOFF_PATH:-${STATE_DIR}/gh_backoff}"
}

gh_backoff_reset() {
  rm -f "$(gh_backoff_path)" 2>/dev/null || true
}

gh_backoff_read() {
  local path
  path=$(gh_backoff_path)
  if [ ! -f "$path" ]; then
    echo "0 0"
    return 0
  fi
  local until delay
  until=$(awk -F= '/^until=/{print $2}' "$path" 2>/dev/null | tail -n1)
  delay=$(awk -F= '/^delay=/{print $2}' "$path" 2>/dev/null | tail -n1)
  until=${until:-0}
  delay=${delay:-0}
  echo "$until $delay"
}

gh_backoff_active() {
  local now until delay
  read -r until delay < <(gh_backoff_read)
  now=$(now_epoch)
  if [ "$until" -gt "$now" ]; then
    echo $((until - now))
    return 0
  fi
  return 1
}

gh_backoff_set() {
  local delay="$1"
  local reason="${2:-rate_limit}"
  ensure_state_dir
  local until
  until=$(( $(now_epoch) + delay ))
  {
    echo "until=$until"
    echo "delay=$delay"
    echo "reason=$reason"
  } > "$(gh_backoff_path)"
}

gh_backoff_next_delay() {
  local base="${1:-30}"
  local max="${2:-900}"
  local until last_delay
  read -r until last_delay < <(gh_backoff_read)
  local next
  if [ "${last_delay:-0}" -gt 0 ]; then
    next=$((last_delay * 2))
  else
    next=$base
  fi
  if [ "$next" -gt "$max" ]; then
    next=$max
  fi
  echo "$next"
}

gh_api() {
  local mode=${GH_BACKOFF_MODE:-wait}
  local base=${GH_BACKOFF_BASE_SECONDS:-30}
  local max=${GH_BACKOFF_MAX_SECONDS:-900}
  local errexit_enabled=0
  if [[ $- == *e* ]]; then
    errexit_enabled=1
  fi

  local remaining
  if remaining=$(gh_backoff_active); then
    if [ "$mode" = "wait" ]; then
      sleep "$remaining"
    else
      log_err "[gh] backoff active for ${remaining}s; skipping request."
      return 75
    fi
  fi

  local out err rc
  out=$(mktemp)
  err=$(mktemp)
  set +e
  command gh api "$@" >"$out" 2>"$err"
  rc=$?
  if [ "$errexit_enabled" -eq 1 ]; then
    set -e
  else
    set +e
  fi

  if [ "$rc" -eq 0 ]; then
    gh_backoff_reset
    cat "$out"
    rm -f "$out" "$err"
    return 0
  fi

  if rg -qi "secondary rate limit|rate limit|API rate limit|abuse detection|HTTP 403" "$err"; then
    local delay
    delay=$(gh_backoff_next_delay "$base" "$max")
    gh_backoff_set "$delay" "rate_limit"
    log_err "[gh] rate limit detected; backing off for ${delay}s."
    if [ "$mode" = "wait" ]; then
      sleep "$delay"
      set +e
      command gh api "$@" >"$out" 2>"$err"
      rc=$?
      if [ "$errexit_enabled" -eq 1 ]; then
        set -e
      else
        set +e
      fi
      if [ "$rc" -eq 0 ]; then
        gh_backoff_reset
        cat "$out"
        rm -f "$out" "$err"
        return 0
      fi
    else
      rm -f "$out" "$err"
      return 75
    fi
  fi

  cat "$err" >&2
  rm -f "$out" "$err"
  return "$rc"
}

render_template() {
  local template_path="$1"
  if [ ! -f "$template_path" ]; then
    log_err "[render_template] template not found: $template_path"
    return 1
  fi
  local output
  output=$(orch-core template "$template_path" 2>&1)
  local rc=$?
  if [ "$rc" -ne 0 ]; then
    log_err "[render_template] orch-core template failed (exit $rc) for $template_path: $output"
    return "$rc"
  fi
  if [ -z "$output" ]; then
    log_err "[render_template] empty output for $template_path"
    return 1
  fi
  printf '%s' "$output"
}

configure_project_status_field() {
  local project_id="$1"
  # Find the Status field ID
  local fields_json status_field_id
  fields_json=$(gh api graphql \
    -f query='query($project:ID!){ node(id:$project){ ... on ProjectV2 { fields(first:100){ nodes{ ... on ProjectV2SingleSelectField { id name } } } } } }' \
    -f project="$project_id" 2>/dev/null || true)
  [ -n "$fields_json" ] || return 0
  status_field_id=$(printf '%s' "$fields_json" | yq -r '.data.node.fields.nodes[] | select(.name == "Status") | .id' 2>/dev/null | head -n1)
  if [ -z "$status_field_id" ] || [ "$status_field_id" = "null" ]; then
    return 0
  fi
  # Update Status field with orchestrator columns
  gh api graphql \
    -f query="mutation(\$fieldId:ID!){ updateProjectV2Field(input:{fieldId:\$fieldId, singleSelectOptions:[{name:\"Backlog\",color:GRAY,description:\"\"},{name:\"In Progress\",color:YELLOW,description:\"\"},{name:\"Review\",color:ORANGE,description:\"\"},{name:\"Done\",color:GREEN,description:\"\"}]}){ projectV2Field{ ... on ProjectV2SingleSelectField { id name options { id name } } } } }" \
    -f fieldId="$status_field_id" >/dev/null 2>&1 || true
  echo "Configured project status columns: Backlog, In Progress, Review, Done"
}

create_project_board_view() {
  local project_num="$1" owner="$2" owner_type="$3"
  local endpoint
  if [ "$owner_type" = "Organization" ]; then
    endpoint="orgs/$owner/projectsV2/$project_num/views"
  else
    endpoint="users/$owner/projectsV2/$project_num/views"
  fi
  gh api "$endpoint" -X POST \
    -f name="Board" -f layout="board" \
    --header "X-GitHub-Api-Version:2022-11-28" >/dev/null 2>&1 || true
  echo "Created Board view"
}

link_project_to_repo() {
  local project_id="$1" repo="$2"
  local repo_node_id
  repo_node_id=$(gh api "repos/$repo" -q '.node_id' 2>/dev/null || true)
  if [ -n "$repo_node_id" ] && [ "$repo_node_id" != "null" ]; then
    gh api graphql \
      -f query='mutation($projectId:ID!,$repoId:ID!){ linkProjectV2ToRepository(input:{projectId:$projectId, repositoryId:$repoId}){ repository{ id } } }' \
      -f projectId="$project_id" \
      -f repoId="$repo_node_id" >/dev/null 2>&1 || true
    echo "Linked project to $repo"
  fi
}

is_bare_repo() {
  local dir="${1:-$PROJECT_DIR}"
  [ -d "$dir" ] && git -C "$dir" rev-parse --is-bare-repository 2>/dev/null | grep -q true
}

require_yq() {
  if ! command -v yq >/dev/null 2>&1; then
    echo "yq is required but not found in PATH." >&2
    exit 1
  fi
}

require_jq() {
  if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required but not found in PATH." >&2
    exit 1
  fi
}

require_rg() {
  if ! command -v rg >/dev/null 2>&1; then
    echo "rg (ripgrep) is required but not found in PATH." >&2
    exit 1
  fi
}

require_fd() {
  if ! command -v fd >/dev/null 2>&1; then
    echo "fd is required but not found in PATH." >&2
    exit 1
  fi
}

require_agent() {
  local agent="$1"
  if ! command -v "$agent" >/dev/null 2>&1; then
    echo "$agent is required but not found in PATH." >&2
    echo "Install it or choose a different agent." >&2
    exit 1
  fi
}

available_agents() {
  local agents=""
  local disabled=""
  disabled=$(yq -r '.router.disabled_agents // [] | join(",")' "$CONFIG_PATH" 2>/dev/null || true)
  for agent in claude codex opencode; do
    if command -v "$agent" >/dev/null 2>&1; then
      # Skip disabled agents
      if [ -n "$disabled" ] && printf '%s' ",$disabled," | grep -q ",$agent,"; then
        continue
      fi
      if [ -n "$agents" ]; then
        agents="$agents,$agent"
      else
        agents="$agent"
      fi
    fi
  done
  echo "$agents"
}

is_usage_limit_error() {
  local text="${1:-}"
  [ -n "$text" ] || return 1
  # Match AI provider rate/quota errors. Patterns are intentionally specific to
  # avoid false positives from generic network errors (e.g. 503 Service Unavailable,
  # SSH timeouts). "service overloaded" is Anthropic-specific. "service unavailable"
  # and bare "temporarily unavailable" are omitted — too common in unrelated errors.
  printf '%s' "$text" | rg -qi '(?:\b429\b|too many requests|rate[ _-]?limit|usage[ _-]?limit|\bquota\b|insufficient[_ -]?quota|exceeded[_ -]?quota|limit (?:reached|exceeded)|overloaded[_ -]?error|service overloaded)'
}

# Redact common API key/token patterns before publishing text to GitHub comments.
redact_snippet() {
  printf '%s' "${1:-}" \
    | sed -E 's/(sk|pk)-[A-Za-z0-9_-]{10,}/[REDACTED]/g' \
    | sed -E 's/Bearer [A-Za-z0-9._~+/=-]{8,}/Bearer [REDACTED]/g' \
    | sed -E 's/token=[A-Za-z0-9._~+%=-]{8,}/token=[REDACTED]/g'
}

# Pick a fallback agent from the locally available agents, rotating after the
# current agent and skipping any agents present in exclude_csv (comma-separated).
# Prints the chosen agent or nothing if none available.
pick_fallback_agent() {
  local current="${1:-}"
  local exclude_csv="${2:-}"
  local agents
  agents=$(available_agents)
  [ -n "$agents" ] || return 1

  local list=()
  IFS=',' read -ra list <<< "$agents"

  local n="${#list[@]}"
  # Default start so that when current is not found we begin from list[0].
  local start=$(( n - 1 ))
  for i in "${!list[@]}"; do
    if [ "${list[$i]}" = "$current" ]; then
      start="$i"
      break
    fi
  done

  local offset idx candidate
  for offset in $(seq 1 "$n"); do
    idx=$(((start + offset) % n))
    candidate="${list[$idx]}"
    [ -n "$candidate" ] || continue
    if [ -n "$current" ] && [ "$candidate" = "$current" ]; then
      continue
    fi
    if [ -n "$exclude_csv" ] && printf ',%s,' "$exclude_csv" | grep -qF ",${candidate},"; then
      continue
    fi
    printf '%s' "$candidate"
    return 0
  done
  return 1
}

opposite_agent() {
  local task_agent="${1:-}"
  local agents
  agents=$(available_agents)
  # Pick first available agent that differs from the task's agent
  IFS=',' read -ra agent_list <<< "$agents"
  for a in "${agent_list[@]}"; do
    if [ "$a" != "$task_agent" ]; then
      echo "$a"
      return
    fi
  done
  # Fallback to configured review_agent
  local configured
  configured=$(config_get '.workflow.review_agent // ""')
  if [ -n "$configured" ] && [ "$configured" != "$task_agent" ]; then
    echo "$configured"
    return
  fi
  # Last resort: same agent
  echo "$task_agent"
}

normalize_json_response() {
  local raw="$1"
  if command -v python3 >/dev/null 2>&1; then
    local script_dir
    script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
    RAW_RESPONSE="$raw" python3 "${script_dir}/normalize_json.py"
    return $?
  fi

  return 1
}

# Legacy compat — ensures backend is initialized.
init_tasks_file() {
  db_init
}

# Legacy compat — ensures jobs file exists.
init_jobs_file() {
  backend_init_jobs
}

init_config_file() {
  if [ ! -f "$CONFIG_PATH" ]; then
    if [ -f "config.example.yml" ]; then
      cp "config.example.yml" "$CONFIG_PATH"
    else
      cat > "$CONFIG_PATH" <<'YAML'
workflow:
  auto_close: true
  review_owner: ""
gh:
  repo: ""
  sync_label: ""
  project_id: ""
  project_status_field_id: ""
  project_status_names: {}
  project_status_map:
    backlog: ""
    in_progress: ""
    review: ""
    done: ""
YAML
    fi
  fi
}

GLOBAL_CONFIG_PATH="${GLOBAL_CONFIG_PATH:-${ORCH_HOME}/config.yml}"

load_project_config() {
  local project_config="${PROJECT_DIR:+${PROJECT_DIR}/orchestrator.yml}"
  # Backwards compat: also check old .orchestrator.yml name
  if [ -z "$project_config" ] || [ ! -f "$project_config" ]; then
    project_config="${PROJECT_DIR:+${PROJECT_DIR}/.orchestrator.yml}"
  fi
  if [ -z "$project_config" ] || [ ! -f "$project_config" ]; then
    return 0
  fi

  # Set project-local state dir
  if [ -n "${PROJECT_DIR:-}" ]; then
    STATE_DIR="${PROJECT_DIR}/.orchestrator"
    ORCH_WORKTREES="${PROJECT_DIR}/.orchestrator/worktrees"
    # Also scope locks + contexts to the project to avoid collisions when
    # multiple repos have overlapping issue numbers (e.g. issue #13 in two repos).
    LOCK_PATH="${PROJECT_DIR}/.orchestrator/locks"
    CONTEXTS_DIR="${PROJECT_DIR}/.orchestrator/contexts"
  fi
  ensure_state_dir
  local merged="${STATE_DIR}/config-merged.yml"
  # Deep merge: global config * project config (project wins)
  yq eval-all 'select(fileIndex == 0) * select(fileIndex == 1)' \
    "$GLOBAL_CONFIG_PATH" "$project_config" > "$merged"
  CONFIG_PATH="$merged"
}

projects_path() {
  echo "${PROJECTS_PATH:-${ORCH_HOME}/projects.yml}"
}

# List project directories from projects.yml.
#
# Supported formats:
# 1) projects: ["/path/a", "/path/b"]
# 2) projects:
#      - dir: "/path/a"
#      - dir: "/path/b"
# 3) top-level YAML sequence of strings or maps with {dir,path}
projects_list() {
  local path
  path=$(projects_path)
  [ -f "$path" ] || return 0

  local json
  json=$(yq -o=json '.' "$path" 2>/dev/null || true)
  [ -n "$json" ] || return 0

  printf '%s' "$json" | jq -r '
    (if type == "array" then . else (.projects // []) end)
    | .[]
    | if type == "string" then .
      elif type == "object" then (.dir // .path // empty)
      else empty
      end
    | select(length > 0)
  ' 2>/dev/null || true
}

config_get() {
  local key="$1"
  if [ -f "$CONFIG_PATH" ]; then
    yq -r "$key" "$CONFIG_PATH"
  else
    echo ""
  fi
}

# Lifecycle hooks: run user-defined commands at key points
# Usage: run_hook <hook_name> [extra_args...]
# Hooks are configured in config.yml under 'hooks:' key.
# Hook commands run async (non-blocking) with ORCH_* env vars for context.
run_hook() {
  local hook_name="${1:-}"; shift 2>/dev/null || true
  local cmd
  cmd=$(config_get ".hooks.${hook_name} // \"\"" 2>/dev/null || true)
  [ -z "$cmd" ] || [ "$cmd" = "null" ] && return 0

  # Export context env vars for the hook
  export ORCH_HOOK="$hook_name"
  export ORCH_TASK_ID="${TASK_ID:-}"
  export ORCH_TASK_TITLE="${TASK_TITLE:-}"
  export ORCH_TASK_AGENT="${TASK_AGENT:-}"
  export ORCH_TASK_STATUS="${AGENT_STATUS:-}"
  export ORCH_PROJECT_DIR="${PROJECT_DIR:-}"
  export ORCH_WORKTREE_DIR="${WORKTREE_DIR:-}"
  export ORCH_BRANCH="${BRANCH_NAME:-}"
  export ORCH_PR_URL="${PR_URL:-}"
  export ORCH_TMUX_SESSION="${TMUX_SESSION:-}"
  export ORCH_GH_ISSUE="${GH_ISSUE_NUMBER:-}"

  # Run async — don't block the main flow
  (eval "$cmd" "$@" >>"${STATE_DIR:-/tmp}/hooks.log" 2>&1 &) || true
}

repo_owner() {
  local repo="${1:-}"
  if [ -z "$repo" ]; then
    repo=$(config_get '.gh.repo // ""')
  fi
  if [ -n "$repo" ] && [ "$repo" != "null" ]; then
    printf '%s' "$repo" | cut -d'/' -f1
  fi
}

# Legacy compat — no local locking needed.
acquire_lock() { :; }
release_lock() { :; }

lock_mtime() {
  local path="$1" mtime
  mtime=$(stat -f %m "$path" 2>/dev/null) && { echo "$mtime"; return 0; }
  mtime=$(stat -c %Y "$path" 2>/dev/null) && { echo "$mtime"; return 0; }
  echo 0
}

lock_is_stale() {
  local path="$1"
  local stale_seconds=${LOCK_STALE_SECONDS:-600}
  local mtime
  mtime=$(lock_mtime "$path")
  if [ -z "$mtime" ] || [ "$mtime" -eq 0 ] 2>/dev/null; then
    return 1
  fi
  local now
  now=$(date +%s)
  if [ $((now - mtime)) -ge "$stale_seconds" ]; then
    return 0
  fi
  return 1
}

# Legacy compat — runs command directly.
with_lock() { "$@"; }

append_history() {
  local task_id="$1" _hist_status="$2" note="$3"
  db_append_history "$task_id" "$_hist_status" "$note"
}

load_task_context() {
  local task_id="$1"
  local role="$2"
  local task_ctx_file="${CONTEXTS_DIR}/task-${task_id}.md"
  local profile_ctx_file="${CONTEXTS_DIR}/profile-${role}.md"

  local out=""
  if [ -f "$profile_ctx_file" ]; then
    out+="[profile:${role}]\n"
    out+="$(cat "$profile_ctx_file")\n\n"
  fi
  if [ -f "$task_ctx_file" ]; then
    out+="[task:${task_id}]\n"
    out+="$(cat "$task_ctx_file")\n"
  fi
  printf '%b' "$out"
}

append_task_context() {
  local task_id="$1"
  local content="$2"
  local task_ctx_file="${CONTEXTS_DIR}/task-${task_id}.md"

  mkdir -p "$CONTEXTS_DIR"
  printf '%s\n' "$content" >> "$task_ctx_file"
}

retry_delay_seconds() {
  local attempts=$1
  local base=${RETRY_BASE_SECONDS:-60}
  local max=${RETRY_MAX_SECONDS:-3600}
  local exp=$((attempts - 1))
  local delay=$base
  if [ "$exp" -gt 0 ]; then
    delay=$((base * (2 ** exp)))
  fi
  if [ "$delay" -gt "$max" ]; then
    delay=$max
  fi
  echo "$delay"
}

build_repo_tree() {
  local dir="${1:-$PROJECT_DIR}"
  if [ -z "$dir" ] || [ ! -d "$dir" ]; then return; fi
  if command -v git >/dev/null 2>&1 && (cd "$dir" && git rev-parse --is-inside-work-tree >/dev/null 2>&1); then
    (cd "$dir" && git ls-files --cached --others --exclude-standard | head -200 | sort)
  else
    (cd "$dir" && find . -type f \
      -not -path './.git/*' \
      -not -path './node_modules/*' \
      -not -path './vendor/*' \
      -not -path './.orchestrator/*' \
      -not -path '*/target/*' \
      -not -path './__pycache__/*' \
      -not -path './.venv/*' \
      | head -200 | sort)
  fi
}

build_project_instructions() {
  local dir="${1:-$PROJECT_DIR}"
  if [ -z "$dir" ] || [ ! -d "$dir" ]; then return; fi
  local out=""
  for f in CLAUDE.md AGENTS.md README.md; do
    if [ -f "$dir/$f" ]; then
      out+="## $f\n$(cat "$dir/$f")\n\n"
    fi
  done
  printf '%b' "$out"
}

# Build skills catalog JSON from SKILL.md frontmatter on disk.
# Returns a JSON array: [{id, name, description}, ...]
build_skills_catalog() {
  local skills_dir="${1:-skills}"
  if [ ! -d "$skills_dir" ]; then echo "[]"; return; fi
  python3 - "$skills_dir" <<'PY'
import json, os, sys, re
skills_dir = sys.argv[1]
catalog = []
for root, dirs, files in sorted(os.walk(skills_dir)):
    if "SKILL.md" not in files:
        continue
    skill_id = os.path.basename(root)
    path = os.path.join(root, "SKILL.md")
    try:
        with open(path, encoding="utf-8") as f:
            content = f.read()
    except Exception as e:
        sys.stderr.write(f"Warning: {path}: {e}\n")
        continue
    # Parse YAML frontmatter between --- markers
    name, desc = skill_id, ""
    m = re.match(r'^---\s*\n(.*?)\n---', content, re.DOTALL)
    if m:
        for line in m.group(1).splitlines():
            if line.startswith("name:"):
                name = line.split(":", 1)[1].strip().strip("'\"")
            elif line.startswith("description:"):
                desc = line.split(":", 1)[1].strip().strip("'\"")
    catalog.append({"id": skill_id, "name": name, "description": desc})
print(json.dumps(catalog))
PY
}

build_skills_docs() {
  local skills_csv="$1"
  if [ -z "$skills_csv" ]; then return; fi

  local required_csv
  required_csv=$(config_get '.workflow.required_skills // [] | join(",")')
  local required_map=","
  if [ -n "$required_csv" ]; then
    required_map=",${required_csv},"
  fi

  local required_out=""
  local reference_out=""
  IFS=',' read -ra skills <<< "$skills_csv"
  for skill_id in "${skills[@]}"; do
    local skill_file
    local orch_skills="${ORCH_HOME:-$HOME/.orchestrator}/skills"
    skill_file=$(find skills/ "$orch_skills" -path "*/${skill_id}/SKILL.md" 2>/dev/null | head -1)
    if [ -n "$skill_file" ] && [ -f "$skill_file" ]; then
      if printf '%s' "$required_map" | grep -q ",${skill_id},"; then
        required_out+="### [REQUIRED] ${skill_id}\nYou MUST follow this skill's workflow exactly. Do not skip any steps.\n\n$(cat "$skill_file")\n\n"
      else
        reference_out+="### ${skill_id}\n$(cat "$skill_file")\n\n"
      fi
    fi
  done

  local out=""
  if [ -n "$required_out" ]; then
    out+="## Required Skills (MANDATORY — follow these workflows exactly)\n\n${required_out}"
  fi
  if [ -n "$reference_out" ]; then
    out+="## Reference Skills\n\n${reference_out}"
  fi
  printf '%b' "$out"
}

build_parent_context() {
  local task_id="$1"
  local parent_id
  parent_id=$(db_task_field "$task_id" "parent_id")
  if [ -z "$parent_id" ] || [ "$parent_id" = "null" ]; then return; fi

  local out=""
  local parent_title parent_summary
  parent_title=$(db_task_field "$parent_id" "title")
  parent_summary=$(db_task_field "$parent_id" "summary")

  if [ -n "$parent_title" ] && [ "$parent_title" != "null" ]; then
    out+="Parent task #${parent_id}: ${parent_title}\n"
    if [ -n "$parent_summary" ] && [ "$parent_summary" != "null" ]; then
      out+="Parent summary: ${parent_summary}\n"
    fi

    local siblings=""
    local child_ids
    child_ids=$(db_task_children "$parent_id" 2>/dev/null || true)
    while IFS= read -r _cid; do
      [ -n "$_cid" ] || continue
      [ "$_cid" = "$task_id" ] && continue
      local _ct _cs
      _ct=$(db_task_field "$_cid" "title" 2>/dev/null || true)
      _cs=$(db_task_field "$_cid" "status" 2>/dev/null || true)
      siblings+="${_cid}: ${_ct} [${_cs}]"$'\n'
    done <<< "$child_ids"
    if [ -n "$siblings" ]; then
      out+="\nSibling tasks:\n${siblings}\n"
    fi
  fi
  printf '%b' "$out"
}

build_git_diff() {
  local dir="${1:-$PROJECT_DIR}"
  local base="${2:-main}"
  if [ -z "$dir" ] || [ ! -d "$dir" ]; then return; fi

  local stat diff_content log_content

  # Always show stat of uncommitted changes
  stat=$(cd "$dir" && git diff --stat HEAD 2>/dev/null | head -50) || true
  if [ -n "$stat" ]; then
    printf '%s\n' "Uncommitted changes:"
    printf '%s\n' "$stat"
    printf '%s\n' ""
  fi

  # Show diff against base branch (truncated to 200 lines)
  diff_content=$(cd "$dir" && git diff "$base"...HEAD 2>/dev/null | head -200) || true
  if [ -n "$diff_content" ]; then
    printf '%s\n' "Diff against $base:"
    printf '%s\n' "$diff_content"
    printf '%s\n' ""
  fi

  # Show commit log since base branch
  log_content=$(cd "$dir" && git log "$base"..HEAD --oneline 2>/dev/null | head -20) || true
  if [ -n "$log_content" ]; then
    printf '%s\n' "Commits since $base:"
    printf '%s\n' "$log_content"
  fi
}

load_task() {
  local task_id="$1"
  db_load_task "$task_id"
}

# --- Task helpers ---
# Read a single field from a task by ID.
# Usage: task_field <id> <field>
# Example: task_field 3 .status  →  "done"
#   Accepts yq-style ".status" or plain "status" — the leading dot is stripped.
task_field() {
  local id="$1" field="$2"
  local col="${field#.}"
  db_task_field "$id" "$col"
}

# Update a single task field.
# Usage: task_set <id> <field> <value>
task_set() {
  local id="$1" field="$2" value="$3"
  local col="${field#.}"
  db_task_set "$id" "$col" "$value"
}

# Count tasks matching a filter (uses dir-based filtering).
# Usage: task_count [status]
task_count() {
  local status="${1:-}"
  local project_dir="${PROJECT_DIR:-}"
  local orch_home="${ORCH_HOME:-$HOME/.orchestrator}"
  local dir_arg=""
  if [ -n "$project_dir" ] && [ "$project_dir" != "$orch_home" ]; then
    dir_arg="$project_dir"
  fi
  db_task_count "$status" "$dir_arg"
}

# Resolve the agent-specific model for a complexity level.
# Reads from config model_map; returns empty string if not configured.
# Usage: model_for_complexity "claude" "complex"  →  "opus"
model_for_complexity() {
  local agent="$1" complexity="$2"
  if [ -z "$complexity" ] || [ "$complexity" = "null" ]; then
    complexity="medium"
  fi
  config_get ".model_map.${complexity}.${agent} // \"\""
}

task_timeout_seconds() {
  local complexity="${1:-medium}"
  if [ -z "$complexity" ] || [ "$complexity" = "null" ]; then
    complexity="medium"
  fi

  local v=""
  v=$(config_get ".workflow.timeout_by_complexity.${complexity} // \"\"" 2>/dev/null || true)
  if [ -n "$v" ] && [ "$v" != "null" ]; then
    echo "$v"
    return 0
  fi

  v=$(config_get '.workflow.timeout_seconds // ""' 2>/dev/null || true)
  if [ -n "$v" ] && [ "$v" != "null" ]; then
    echo "$v"
    return 0
  fi

  # Default task timeout: 30 minutes (increased from 15).
  echo "1800"
}

max_attempts() {
  local max
  max=$(config_get '.workflow.max_attempts // ""')
  if [ -z "$max" ] || [ "$max" = "null" ]; then
    echo "${MAX_ATTEMPTS:-10}"
  else
    echo "$max"
  fi
}

# Shared task creation — used by add_task.sh and run_task.sh delegations.
# Args: id title body labels_csv [parent_id] [suggested_agent]
# Expects PROJECT_DIR in environment. The id arg is ignored (SQLite auto-increments).
# Returns the new task ID.
create_task_entry() {
  local _id="$1" title="$2" body="$3" labels_csv="$4"
  local parent_id="${5:-}" suggested_agent="${6:-}"
  db_create_task "$title" "$body" "${PROJECT_DIR:-}" "$labels_csv" "$parent_id" "$suggested_agent"
}

# Simple spinner for long-running operations.
# Usage: start_spinner "message"; long_command; stop_spinner
SPINNER_PID=""
start_spinner() {
  local msg="${1:-Working}"
  if [ ! -t 2 ]; then return; fi  # skip if not a terminal
  (
    chars='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'
    i=0
    while true; do
      printf '\r\033[K  %s %s' "${chars:i%${#chars}:1}" "$msg" >&2
      i=$((i + 1))
      sleep 0.1
    done
  ) &
  SPINNER_PID=$!
  disown "$SPINNER_PID" 2>/dev/null || true
}

stop_spinner() {
  if [ -n "${SPINNER_PID:-}" ]; then
    kill "$SPINNER_PID" 2>/dev/null || true
    wait "$SPINNER_PID" 2>/dev/null || true
    SPINNER_PID=""
    printf '\r\033[K' >&2
  fi
}

# Post a comment on a linked GitHub issue.
# Usage: comment_on_issue TASK_ID "comment body"
# Requires GH_ISSUE_NUMBER and gh.repo in config. Silently skips if unavailable.
comment_on_issue() {
  local task_id="$1" body="$2"
  local issue_number="${GH_ISSUE_NUMBER:-}"
  if [ -z "$issue_number" ] || [ "$issue_number" = "null" ] || [ "$issue_number" = "0" ]; then
    return 0
  fi
  local repo
  repo=$(config_get '.gh.repo // ""')
  if [ -z "$repo" ] || [ "$repo" = "null" ]; then
    return 0
  fi
  if ! command -v gh >/dev/null 2>&1; then
    return 0
  fi
  gh_api "repos/${repo}/issues/${issue_number}/comments" \
    -f body="$body" >/dev/null 2>&1 || true
}

# Add a label to a linked GitHub issue.
# Usage: label_issue TASK_ID "label"
label_issue() {
  local task_id="$1" label="$2"
  local issue_number="${GH_ISSUE_NUMBER:-}"
  if [ -z "$issue_number" ] || [ "$issue_number" = "null" ] || [ "$issue_number" = "0" ]; then
    return 0
  fi
  local repo
  repo=$(config_get '.gh.repo // ""')
  if [ -z "$repo" ] || [ "$repo" = "null" ]; then
    return 0
  fi
  if ! command -v gh >/dev/null 2>&1; then
    return 0
  fi
  gh_api "repos/${repo}/issues/${issue_number}/labels" \
    --input - <<< "{\"labels\":[\"$label\"]}" >/dev/null 2>&1 || true
}

# Mark a task as needs_review — requires human attention.
# The task will not be retried until manually addressed.
# GitHub labels and comments are updated via the backend.
# Usage: mark_needs_review TASK_ID ATTEMPTS "error message" ["history note"]
mark_needs_review() {
  local task_id="$1" attempts="$2" error="$3" note="${4:-$3}"
  db_task_update "$task_id" status=needs_review "last_error=$error"
  append_history "$task_id" "needs_review" "$note"
}

fetch_issue_comments() {
  local repo="$1" issue_num="$2" max="${3:-10}"
  if [ -z "$issue_num" ] || [ "$issue_num" = "null" ] || [ "$issue_num" = "0" ]; then return 0; fi
  if [ -z "$repo" ] || [ "$repo" = "null" ]; then return 0; fi
  gh_api "repos/${repo}/issues/${issue_num}/comments" \
    -q ".[-${max}:] | .[] | \"### \" + .user.login + \" (\" + .created_at + \")\\n\" + .body + \"\\n---\"" 2>/dev/null || true
}

# Fetch issue/PR comments from the repo owner since a given timestamp.
# Returns JSON array of {login, created_at, body} objects.
# Usage: fetch_owner_feedback REPO ISSUE_NUM OWNER_LOGIN SINCE_TIMESTAMP
fetch_owner_feedback() {
  local repo="$1" issue_num="$2" owner_login="$3" since="${4:-}"
  if [ -z "$issue_num" ] || [ "$issue_num" = "null" ] || [ "$issue_num" = "0" ]; then echo "[]"; return 0; fi
  if [ -z "$repo" ] || [ "$repo" = "null" ]; then echo "[]"; return 0; fi
  if [ -z "$owner_login" ] || [ "$owner_login" = "null" ]; then echo "[]"; return 0; fi

  local api_url="repos/${repo}/issues/${issue_num}/comments"
  local since_param=""
  if [ -n "$since" ] && [ "$since" != "null" ]; then
    since_param="-f since=${since}"
  fi

  local raw
  # shellcheck disable=SC2086
  raw=$(gh_api -X GET "$api_url" $since_param 2>/dev/null) || { echo "[]"; return 0; }

  printf '%s' "$raw" | jq -c \
    --arg owner "$owner_login" --arg since "${since:-}" \
    '[.[]
      | select(.user.login == $owner)
      # Exclude orchestrator-generated comments (agent response, acks, etc.)
      | select((.body | test("via \\[Orchestrator\\]")) | not)
      | select((.body | contains("<!-- orch:")) | not)
      # Exclude lightweight history entries appended by orchestrator (avoid feedback loops)
      | select((.body | test("^\\[\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}:\\d{2}Z\\] ")) | not)
      | select($since == "" or .created_at > $since)
      | {login: .user.login, created_at: .created_at, body: .body}
    ]' \
    2>/dev/null || echo "[]"
}

# Parse a slash command from the first line of a comment body.
# Returns 0 if the first line is a slash command and echoes:
#   line1: command (lowercased, without leading "/")
#   line2: raw args from the first line (original casing/spacing preserved)
# Usage: _parse_owner_slash_command "$body" && read -r cmd; read -r args
_parse_owner_slash_command() {
  local body="${1:-}"
  local first_line trimmed lower cmd args
  first_line=$(printf '%s' "$body" | head -n 1 | tr -d '\r')
  trimmed=$(printf '%s' "$first_line" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
  [ -n "$trimmed" ] || return 1
  [[ "$trimmed" == /* ]] || return 1

  lower=$(printf '%s' "$trimmed" | tr '[:upper:]' '[:lower:]')
  cmd="${lower%%[[:space:]]*}"
  cmd="${cmd#/}"

  # Extract args from original trimmed line (not lowercased)
  args=$(printf '%s' "$trimmed" | sed -E 's#^/[[:alnum:]_-]+[[:space:]]*##')

  printf '%s\n' "$cmd"
  printf '%s\n' "$args"
  return 0
}

# Process a single owner comment object {login, created_at, body} for a task.
# Supports slash commands; non-commands fall back to process_owner_feedback().
# Usage: process_owner_comment TASK_ID REPO COMMENT_JSON
process_owner_comment() {
  local task_id="$1" repo="$2" comment_json="$3"

  local login created_at body
  login=$(printf '%s' "$comment_json" | jq -r '.login // ""' 2>/dev/null || true)
  created_at=$(printf '%s' "$comment_json" | jq -r '.created_at // ""' 2>/dev/null || true)
  body=$(printf '%s' "$comment_json" | jq -r '.body // ""' 2>/dev/null || true)

  [ -n "$task_id" ] || return 0
  [ -n "$repo" ] || return 0
  [ -n "$created_at" ] || created_at="$(now_iso)"

  local cmd args
  local parsed=""
  if parsed=$(_parse_owner_slash_command "$body" 2>/dev/null); then
    cmd=$(printf '%s' "$parsed" | sed -n '1p' 2>/dev/null || true)
    args=$(printf '%s' "$parsed" | sed -n '2p' 2>/dev/null || true)
  else
    cmd=""
    args=""
  fi

  # Helper: post an acknowledgement comment (excluded from feedback scans)
  _owner_cmd_ack() {
    local msg="$1"
    local ack_body
    ack_body=$(printf '%s\n%s\n\n---\n*By Orchestrator via [Orchestrator](https://github.com/gabrielkoerich/orchestrator)*' \
      "<!-- orch:owner-command -->" \
      "${msg}")
    gh_api "repos/${repo}/issues/${task_id}/comments" \
      -f body="$ack_body" \
      >/dev/null 2>&1 || true
  }

  # Helper: optional trailing context (lines after first line)
  _trailing_context() {
    printf '%s' "$body" | tail -n +2 | sed 's/\r$//' || true
  }

  # Always advance last feedback timestamp to avoid re-processing/spam.
  # Command handlers may overwrite status, but should not change created_at tracking.
  local advance_ts="gh_last_feedback_at=$created_at"

  if [ -z "$cmd" ]; then
    # Non-command: preserve existing behavior.
    local single
    single=$(jq -nc --arg login "$login" --arg created_at "$created_at" --arg body "$body" \
      '[{login: $login, created_at: $created_at, body: $body}]')
    process_owner_feedback "$task_id" "$single"
    return 0
  fi

  case "$cmd" in
    retry)
      local extra
      extra=$(_trailing_context)
      if [ -n "$extra" ]; then
        append_task_context "$task_id" "### Owner context (${login:-owner} ${created_at})"$'\n'"${extra}"$'\n---\n'
      fi
      db_task_update "$task_id" \
        "status=new" \
        "agent=NULL" \
        "attempts=0" \
        "needs_help=0" \
        "reason=NULL" \
        "last_error=NULL" \
        "$advance_ts"
      append_history "$task_id" "new" "owner command: /retry"
      _owner_cmd_ack "✅ Applied `/retry` — reset task to `status:new` (agent cleared, attempts reset)."
      ;;
    assign)
      local agent
      agent=$(printf '%s' "$args" | awk '{print $1}' | tr '[:upper:]' '[:lower:]' | sed 's/[^a-z]//g')
      case "$agent" in
        claude|codex|opencode)
          db_task_update "$task_id" \
            "agent=$agent" \
            "status=routed" \
            "attempts=0" \
            "needs_help=0" \
            "reason=NULL" \
            "last_error=NULL" \
            "$advance_ts"
          append_history "$task_id" "routed" "owner command: /assign $agent"
          _owner_cmd_ack "✅ Applied `/assign ${agent}` — set agent and moved task to `status:routed`."
          ;;
        *)
          db_task_update "$task_id" "$advance_ts"
          _owner_cmd_ack "❌ Invalid agent for `/assign`: \`${agent:-}\`. Allowed: \`claude\`, \`codex\`, \`opencode\`."
          ;;
      esac
      ;;
    unblock)
      db_task_update "$task_id" \
        "status=new" \
        "attempts=0" \
        "needs_help=0" \
        "reason=NULL" \
        "last_error=NULL" \
        "$advance_ts"
      append_history "$task_id" "new" "owner command: /unblock"
      _owner_cmd_ack "✅ Applied `/unblock` — reset task to `status:new`."
      ;;
    close)
      db_task_update "$task_id" \
        "status=done" \
        "needs_help=0" \
        "reason=NULL" \
        "last_error=NULL" \
        "$advance_ts"
      gh_api "repos/${repo}/issues/${task_id}" -X PATCH -f state=closed >/dev/null 2>&1 || true
      append_history "$task_id" "done" "owner command: /close"
      _owner_cmd_ack "✅ Applied `/close` — marked `status:done` and closed the issue."
      ;;
    context)
      local ctx
      ctx=$(printf '%s' "$args" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
      if [ -z "$ctx" ]; then
        ctx=$(_trailing_context)
      fi
      if [ -z "$ctx" ]; then
        db_task_update "$task_id" "$advance_ts"
        _owner_cmd_ack "❌ Missing text for \`/context\`. Usage: \`/context <text>\` (or put text on following lines)."
      else
        append_task_context "$task_id" "### Owner context (${login:-owner} ${created_at})"$'\n'"${ctx}"$'\n---\n'
        db_task_update "$task_id" \
          "status=routed" \
          "attempts=0" \
          "needs_help=0" \
          "reason=NULL" \
          "last_error=NULL" \
          "$advance_ts"
        append_history "$task_id" "routed" "owner command: /context"
        _owner_cmd_ack "✅ Applied `/context` — appended text to task context and moved task to `status:routed`."
      fi
      ;;
    priority)
      local prio complexity
      prio=$(printf '%s' "$args" | awk '{print $1}' | tr '[:upper:]' '[:lower:]')
      complexity="$prio"
      case "$prio" in
        high) complexity="complex" ;;
        med|medium) complexity="medium" ;;
        low) complexity="simple" ;;
      esac
      case "$complexity" in
        simple|medium|complex)
          db_task_update "$task_id" \
            "complexity=$complexity" \
            "status=routed" \
            "attempts=0" \
            "needs_help=0" \
            "reason=NULL" \
            "last_error=NULL" \
            "$advance_ts"
          append_history "$task_id" "routed" "owner command: /priority $complexity"
          _owner_cmd_ack "✅ Applied `/priority ${prio}` — set complexity to \`${complexity}\` and moved task to `status:routed`."
          ;;
        *)
          db_task_update "$task_id" "$advance_ts"
          _owner_cmd_ack "❌ Invalid value for `/priority`: \`${prio:-}\`. Allowed: \`low\`, \`medium\`, \`high\` (or \`simple|medium|complex\`)."
          ;;
      esac
      ;;
    help)
      db_task_update "$task_id" "$advance_ts"
      _owner_cmd_ack $'Supported commands:\n\n- `/retry`\n- `/assign claude|codex|opencode`\n- `/unblock`\n- `/close`\n- `/context <text>`\n- `/priority low|medium|high`\n- `/help`'
      ;;
    *)
      db_task_update "$task_id" "$advance_ts"
      _owner_cmd_ack "❌ Unknown command: \`/${cmd}\`. Use \`/help\` for supported commands."
      ;;
  esac
}

# Check a task for new owner feedback/comments since last scan and apply them.
# This is intended to be called from poll/serve loops.
# Usage: process_owner_feedback_for_task REPO TASK_ID OWNER_LOGIN
process_owner_feedback_for_task() {
  local repo="$1" task_id="$2" owner_login="$3"
  [ -n "$repo" ] || return 0
  [ -n "$task_id" ] || return 0
  [ -n "$owner_login" ] || return 0

  # Avoid double-processing when multiple poll loops overlap.
  local fb_lock="${LOCK_PATH}.owner_feedback.${task_id}"
  local fb_lock_owned=false
  if ! mkdir "$fb_lock" 2>/dev/null; then
    if lock_is_stale "$fb_lock"; then
      rmdir "$fb_lock" 2>/dev/null || true
    fi
    if ! mkdir "$fb_lock" 2>/dev/null; then
      return 0
    fi
  fi
  fb_lock_owned=true
  trap 'if [ "${fb_lock_owned:-false}" = true ]; then rmdir "$fb_lock" 2>/dev/null || true; fi' RETURN

  local since
  since=$(db_task_field "$task_id" "gh_last_feedback_at" 2>/dev/null || true)
  local feedback_json
  feedback_json=$(fetch_owner_feedback "$repo" "$task_id" "$owner_login" "${since:-}")

  local count
  count=$(printf '%s' "$feedback_json" | jq -r 'length' 2>/dev/null || echo "0")
  [ "$count" -gt 0 ] || return 0

  # Process in order to preserve intent if multiple comments arrive.
  local i
  for i in $(seq 0 $((count - 1))); do
    local item
    item=$(printf '%s' "$feedback_json" | jq -c ".[$i]" 2>/dev/null || echo '{}')
    process_owner_comment "$task_id" "$repo" "$item"
  done
}

# Apply owner feedback to a task: append to context, reset status to routed.
# Caller must hold the lock (or pass from an unlocked context).
# Usage: process_owner_feedback TASK_ID FEEDBACK_JSON
process_owner_feedback() {
  local task_id="$1" feedback_json="$2"

  local count
  count=$(printf '%s' "$feedback_json" | jq -r 'length' 2>/dev/null || echo "0")
  if [ "$count" -le 0 ]; then return 0; fi

  # Build feedback text for context file
  local feedback_text=""
  local latest_ts=""
  for i in $(seq 0 $((count - 1))); do
    local login created_at body
    login=$(printf '%s' "$feedback_json" | jq -r ".[$i].login")
    created_at=$(printf '%s' "$feedback_json" | jq -r ".[$i].created_at")
    body=$(printf '%s' "$feedback_json" | jq -r ".[$i].body")
    feedback_text+="### Owner feedback from ${login} (${created_at})"$'\n'"${body}"$'\n---\n'
    latest_ts="$created_at"
  done

  # Append feedback to task context file
  append_task_context "$task_id" "$feedback_text"

  # Truncate for last_error (first 200 chars of combined feedback bodies)
  local combined_bodies
  combined_bodies=$(printf '%s' "$feedback_json" | jq -r '.[].body' | head -c 200)

  db_task_update "$task_id" \
    "status=routed" \
    "last_error=$combined_bodies" \
    "needs_help=0" \
    "gh_last_feedback_at=$latest_ts"
  db_append_history "$task_id" "routed" "owner feedback received"
}

run_with_timeout() {
  local timeout_seconds=${AGENT_TIMEOUT_SECONDS:-1800}
  if [ -z "$timeout_seconds" ] || [ "$timeout_seconds" = "0" ]; then
    "$@"
    return $?
  fi
  if command -v timeout >/dev/null 2>&1; then
    timeout "$timeout_seconds" "$@"
    return $?
  fi
  if command -v gtimeout >/dev/null 2>&1; then
    gtimeout "$timeout_seconds" "$@"
    return $?
  fi
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$timeout_seconds" "$@" <<'PY'
import subprocess
import sys

timeout_seconds = float(sys.argv[1])
cmd = sys.argv[2:]

try:
    result = subprocess.run(cmd, timeout=timeout_seconds)
    sys.exit(result.returncode)
except subprocess.TimeoutExpired:
    sys.exit(124)
PY
    return $?
  fi
  "$@"
}

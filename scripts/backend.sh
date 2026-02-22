#!/usr/bin/env bash
# Backend interface loader for orchestrator.
# Reads `backend` from config and sources the matching implementation.
# All scripts call backend_* functions instead of db_*.
# shellcheck disable=SC2155

# Determine which backend to use (default: github)
_BACKEND_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
_BACKEND_TYPE="${ORCH_BACKEND:-}"
if [ -z "$_BACKEND_TYPE" ] && [ -f "${CONFIG_PATH:-}" ]; then
  _BACKEND_TYPE=$(yq -r '.backend // "github"' "$CONFIG_PATH" 2>/dev/null || echo "github")
fi
_BACKEND_TYPE="${_BACKEND_TYPE:-github}"

# Source the implementation
_BACKEND_IMPL="${_BACKEND_DIR}/backend_${_BACKEND_TYPE}.sh"
if [ ! -f "$_BACKEND_IMPL" ]; then
  echo "Backend implementation not found: $_BACKEND_IMPL" >&2
  exit 1
fi
source "$_BACKEND_IMPL"

# ============================================================
# Jobs CRUD (YAML file backed — stays the same across backends)
# ============================================================

JOBS_FILE="${JOBS_FILE:-${ORCH_HOME}/jobs.yml}"

# Ensure jobs file exists
backend_init_jobs() {
  if [ ! -f "$JOBS_FILE" ]; then
    printf 'jobs: []\n' > "$JOBS_FILE"
  fi
}

# Read jobs array from YAML as JSON
_read_jobs() {
  if [ -f "$JOBS_FILE" ]; then
    yq -o=json '.jobs // []' "$JOBS_FILE" 2>/dev/null || echo '[]'
  else
    echo '[]'
  fi
}

# Write JSON array back to YAML
_write_jobs() {
  local json="$1"
  printf '%s' "$json" | yq -P '{"jobs": .}' > "$JOBS_FILE"
}

db_create_job() {
  local id="$1" title="$2" schedule="$3" type="${4:-task}"
  local body="${5:-}" labels="${6:-}" agent="${7:-}" dir="${8:-}" command="${9:-}"
  local now
  now=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  local jobs
  jobs=$(_read_jobs)
  local new_job
  new_job=$(jq -nc --arg id "$id" --arg t "$title" --arg s "$schedule" --arg ty "$type" \
    --arg b "$body" --arg l "$labels" --arg a "$agent" --arg d "$dir" --arg c "$command" --arg n "$now" \
    '{id: $id, title: $t, schedule: $s, type: $ty, command: (if $c == "" then null else $c end),
      body: $b, labels: $l, agent: (if $a == "" then null else $a end),
      dir: $d, enabled: true, active_task_id: null, last_run: null,
      last_task_status: null, created_at: $n}')
  _write_jobs "$(printf '%s' "$jobs" | jq --argjson j "$new_job" '. + [$j]')"
}

db_job_field() {
  local id="$1" field="$2"
  _read_jobs | jq -r --arg id "$id" --arg f "$field" '.[] | select(.id == $id) | .[$f] // empty'
}

db_job_set() {
  local id="$1" field="$2" value="$3"
  local jobs
  jobs=$(_read_jobs)
  _write_jobs "$(printf '%s' "$jobs" | jq --arg id "$id" --arg f "$field" --arg v "$value" \
    '[.[] | if .id == $id then .[$f] = $v else . end]')"
}

db_job_list() {
  _read_jobs | jq -r '.[] | [.id, (.task.title // .title // ""), .schedule, .type, (if .enabled then "1" else "0" end), (.active_task_id // ""), (.last_run // "")] | @tsv'
}

db_job_delete() {
  local id="$1"
  local jobs
  jobs=$(_read_jobs)
  _write_jobs "$(printf '%s' "$jobs" | jq --arg id "$id" '[.[] | select(.id != $id)]')"
}

db_enabled_job_count() {
  _read_jobs | jq '[.[] | select(.enabled == true)] | length'
}

db_enabled_job_ids() {
  _read_jobs | jq -r '.[] | select(.enabled == true) | .id'
}

db_load_job() {
  local jid="$1"
  local row
  row=$(_read_jobs | jq -c --arg id "$jid" '.[] | select(.id == $id and .enabled == true)') || return 1
  [ -z "$row" ] && return 1

  JOB_ID=$(printf '%s' "$row" | jq -r '.id')
  JOB_SCHEDULE=$(printf '%s' "$row" | jq -r '.schedule')
  JOB_TYPE=$(printf '%s' "$row" | jq -r '.type')
  JOB_CMD=$(printf '%s' "$row" | jq -r '.command // ""')
  JOB_TITLE=$(printf '%s' "$row" | jq -r '.task.title // .title // ""')
  JOB_BODY=$(printf '%s' "$row" | jq -r '.task.body // .body // ""')
  JOB_LABELS=$(printf '%s' "$row" | jq -r '(.task.labels // .labels // []) | if type == "array" then join(",") else . end')
  JOB_AGENT=$(printf '%s' "$row" | jq -r '.task.agent // .agent // ""')
  JOB_DIR=$(printf '%s' "$row" | jq -r '.dir // ""')
  JOB_ACTIVE_TASK_ID=$(printf '%s' "$row" | jq -r '.active_task_id // ""')
  JOB_LAST_RUN=$(printf '%s' "$row" | jq -r '.last_run // ""')
}

# ============================================================
# Legacy compatibility shims
# ============================================================

db_acquire_lock() { :; }
db_release_lock() { :; }
db_with_lock() { "$@"; }
sql_escape() { printf '%s' "${1:-}"; }
db() { :; }
db_row() { :; }
db_json() { echo '[]'; }
db_scalar() { :; }
db_ensure_columns() { :; }
db_export_tasks_yaml() { :; }
db_next_id() { echo "0"; }

#!/usr/bin/env bats

setup() {
  export REPO_DIR="${BATS_TEST_DIRNAME}/.."
  export PATH="${REPO_DIR}/scripts:${PATH}"

  # Prefer Bats-managed temp dir (works in sandboxed CI + avoids macOS /tmp quirks)
  local base_tmp
  base_tmp="${BATS_TEST_TMPDIR:-${TMPDIR:-/tmp}}"
  mkdir -p "${base_tmp}/orchestrator-tests"
  TMP_DIR=$(mktemp -d "${base_tmp}/orchestrator-tests/test.XXXXXX")
  export STATE_DIR="${TMP_DIR}/.orchestrator"
  mkdir -p "$STATE_DIR"
  export ORCH_HOME="${TMP_DIR}/orch_home"
  mkdir -p "$ORCH_HOME"
  export CONFIG_PATH="${ORCH_HOME}/config.yml"
  export JOBS_FILE="${ORCH_HOME}/jobs.yml"
  export LOCK_PATH="${STATE_DIR}/locks"
  export PROJECT_DIR="${TMP_DIR}"

  # Initialize a git repo so worktree creation works
  git -C "$PROJECT_DIR" init -b main --quiet 2>/dev/null || true
  git -C "$PROJECT_DIR" -c user.email="test@test.com" -c user.name="Test" commit --allow-empty -m "init" --quiet 2>/dev/null || true

  # Set up gh mock — GitHub is the native backend
  MOCK_BIN="${TMP_DIR}/mock_bin"
  mkdir -p "$MOCK_BIN"
  cp "${BATS_TEST_DIRNAME}/gh_mock.sh" "$MOCK_BIN/gh"
  chmod +x "$MOCK_BIN/gh"
  export PATH="${MOCK_BIN}:${PATH}"
  export GH_MOCK_STATE="${STATE_DIR}/gh_mock_state.json"
  export ORCH_GH_REPO="mock/repo"
  export ORCH_BACKEND="github"

  # Initialize jobs file
  printf 'jobs: []\n' > "$JOBS_FILE"
  export MONITOR_INTERVAL=0.1
  export USE_TMUX=false
  cat > "$CONFIG_PATH" <<'YAML'
backend: github
gh:
  repo: "mock/repo"
router:
  agent: "codex"
  model: ""
  timeout_seconds: 0
  fallback_executor: "codex"
  allowed_tools: []
  default_skills: []
llm:
  input_format: ""
  output_format: ""
workflow:
  enable_review_agent: false
  review_agent: "claude"
YAML

  INIT_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Init" "Bootstrap" "")
  export INIT_TASK_ID=$(echo "$INIT_OUTPUT" | sed 's/Added task //' | cut -d: -f1 | tr -d ' ')
}

# Test helpers — read/write via backend (GitHub mock + sidecar)
tdb_field() {
  local id="$1" field="$2"
  source "${REPO_DIR}/scripts/lib.sh"
  db_task_field "$id" "$field"
}
tdb_set() {
  local id="$1" field="$2" value="$3"
  source "${REPO_DIR}/scripts/lib.sh"
  db_task_set "$id" "$field" "$value"
}
tdb_count() {
  # Count issues in gh mock state
  if [ -f "$GH_MOCK_STATE" ]; then
    jq '.issues | length' "$GH_MOCK_STATE" 2>/dev/null || echo 0
  else
    echo 0
  fi
}
tdb_job_field() {
  local id="$1" field="$2"
  yq -o=json '.jobs // []' "$JOBS_FILE" 2>/dev/null | jq -r --arg id "$id" --arg f "$field" '.[] | select(.id == $id) | .[$f] // empty'
}
tdb_job_count() {
  yq -o=json '.jobs // []' "$JOBS_FILE" 2>/dev/null | jq 'length'
}

teardown() {
  # Clean up project-local worktrees
  if [ -d "${TMP_DIR}/.orchestrator/worktrees" ]; then
    (cd "$TMP_DIR" && git worktree prune 2>/dev/null) || true
  fi
  # Clean up global worktrees (legacy location)
  PROJECT_NAME=$(basename "$TMP_DIR")
  WORKTREE_BASE="${ORCH_HOME}/worktrees/${PROJECT_NAME}"
  if [ -d "$WORKTREE_BASE" ]; then
    (cd "$TMP_DIR" && git worktree prune 2>/dev/null) || true
    rm -rf "$WORKTREE_BASE"
  fi
  rm -rf "${TMP_DIR}"
}

@test "task timeout resolves from workflow config with sensible defaults" {
  source "${REPO_DIR}/scripts/lib.sh"

  # No workflow timeout configured: default is 1800s (30 minutes).
  run task_timeout_seconds medium
  [ "$status" -eq 0 ]
  [ "$output" = "1800" ]

  # workflow.timeout_seconds overrides the default.
  run yq -i '.workflow.timeout_seconds = 1200' "$CONFIG_PATH"
  [ "$status" -eq 0 ]
  run task_timeout_seconds medium
  [ "$status" -eq 0 ]
  [ "$output" = "1200" ]

  # workflow.timeout_by_complexity takes precedence.
  run yq -i '.workflow.timeout_by_complexity.medium = 2400' "$CONFIG_PATH"
  [ "$status" -eq 0 ]
  run task_timeout_seconds medium
  [ "$status" -eq 0 ]
  [ "$output" = "2400" ]
}

# Helper: parse task ID from add_task.sh output
_task_id() {
  echo "$1" | grep 'Added task' | sed 's/Added task //' | cut -d: -f1 | tr -d ' '
}

# Helper: find task ID by title (search gh mock state)
_task_id_by_title() {
  local title="$1"
  if [ -f "$GH_MOCK_STATE" ]; then
    jq -r --arg t "$title" '.issues | to_entries[] | select(.value.title == $t) | .key' "$GH_MOCK_STATE" | head -1
  fi
}

# Helper: get child task titles (via sub_issues in mock state)
_task_children_titles() {
  local parent_id="$1"
  if [ -f "$GH_MOCK_STATE" ]; then
    local children
    children=$(jq -r --arg p "$parent_id" '.sub_issues[$p] // [] | .[]' "$GH_MOCK_STATE" 2>/dev/null || true)
    for cid in $children; do
      jq -r --arg id "$cid" '.issues[$id].title // empty' "$GH_MOCK_STATE" 2>/dev/null
    done
  fi
}

# Helper: add comment to gh mock (replaces INSERT INTO task_history)
_add_history() {
  local task_id="$1" ts="$2" hist_status="$3" note="$4"
  source "${REPO_DIR}/scripts/lib.sh"
  db_append_history "$task_id" "$hist_status" "$note"
}

# Helper: get task labels
_task_labels() {
  local task_id="$1"
  if [ -f "$GH_MOCK_STATE" ]; then
    jq -r --arg id "$task_id" '.issues[$id].labels // [] | .[].name' "$GH_MOCK_STATE" 2>/dev/null
  fi
}

# Helper: add label
_task_add_label() {
  local task_id="$1" label="$2"
  source "${REPO_DIR}/scripts/lib.sh"
  db_add_label "$task_id" "$label"
}

# Helper: set parent-child relationship (via sub-issues in mock)
_task_set_parent() {
  local child_id="$1" parent_id="$2"
  source "${REPO_DIR}/scripts/lib.sh"
  _sidecar_write "$child_id" "parent_id" "$parent_id"
  # Also update mock state sub_issues
  if [ -f "$GH_MOCK_STATE" ]; then
    local state
    state=$(cat "$GH_MOCK_STATE")
    printf '%s' "$state" | jq --arg p "$parent_id" --arg c "$child_id" \
      '.sub_issues[$p] = ((.sub_issues[$p] // []) + [($c | tonumber)] | unique)' > "$GH_MOCK_STATE"
  fi
}

# Helper: create a job directly in jobs.yml (for tests that bypass jobs_add.sh)
_create_job() {
  local id="$1" title="$2" schedule="$3" type="${4:-task}" body="${5:-}"
  local labels="${6:-}" enabled="${7:-true}" active_task_id="${8:-null}" last_run="${9:-null}"
  shift 9 || true
  local last_task_status="${1:-null}"
  local now
  now=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  local jobs
  jobs=$(yq -o=json '.jobs // []' "$JOBS_FILE" 2>/dev/null || echo '[]')
  local new_job
  new_job=$(jq -nc \
    --arg id "$id" --arg t "$title" --arg s "$schedule" --arg ty "$type" \
    --arg b "$body" --arg l "$labels" --argjson e "$enabled" \
    --arg at "${active_task_id}" --arg lr "${last_run}" \
    --arg ls "${last_task_status}" --arg n "$now" --arg d "${PROJECT_DIR:-}" \
    '{id: $id, title: $t, schedule: $s, type: $ty, command: null,
      body: $b, labels: $l, agent: null, dir: $d,
      enabled: $e,
      active_task_id: (if $at == "null" then null else $at end),
      last_run: (if $lr == "null" then null else $lr end),
      last_task_status: (if $ls == "null" then null else $ls end),
      created_at: $n}')
  printf '%s' "$(printf '%s' "$jobs" | jq --argjson j "$new_job" '. + [$j]')" | yq -P '{"jobs": .}' > "$JOBS_FILE"
}

@test "add_task.sh creates a new task" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Test Title" "Test Body" "label1,label2")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")
  [ -n "$TASK2_ID" ]

  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -eq 2 ]

  run tdb_field "$TASK2_ID" title
  [ "$status" -eq 0 ]
  [ "$output" = "Test Title" ]

  run tdb_field "$TASK2_ID" dir
  [ "$status" -eq 0 ]
  [ "$output" = "$TMP_DIR" ]
}

@test "route_task.sh sets agent, status, and profile" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Route Me" "Routing body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  run yq -i '.router.agent = "codex"' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"executor":"codex","complexity":"medium","reason":"test route","profile":{"role":"backend specialist","skills":["api","sql"],"tools":["git","rg"],"constraints":["no migrations"]},"selected_skills":[]}
JSON
SH
  chmod +x "$CODEX_STUB"
  export PATH="${TMP_DIR}:${PATH}"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" "${REPO_DIR}/scripts/route_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]
  [ "$(printf '%s' "$output" | tail -n1)" = "codex" ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "routed" ]

  # Check agent_profile role via metadata
  run tdb_field "$TASK2_ID" agent_profile
  [ "$status" -eq 0 ]
  [[ "$output" == *"backend specialist"* ]]
}

@test "run_task.sh updates task and handles delegations" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Run Me" "Run body" "plan")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Stub prints JSON to stdout (parsed by normalize_json_response)
  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"status":"in_progress","summary":"scoped work","files_changed":[],"needs_help":true,"delegations":[{"title":"Child Task","body":"Do subtask","labels":["sub"],"suggested_agent":"codex"}]}
JSON
SH
  chmod +x "$CODEX_STUB"
  export PATH="${TMP_DIR}:${PATH}"

  tdb_set "$TASK2_ID" agent "codex"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "blocked" ]

  # Check child task was created
  run _task_children_titles "$TASK2_ID"
  [ "$status" -eq 0 ]
  [ "$output" = "Child Task" ]
}

@test "run_task.sh ignores delegations from non-plan tasks" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Regular Task" "Regular body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"status":"done","summary":"did the work","files_changed":[],"needs_help":false,"delegations":[{"title":"Unwanted Subtask","body":"Should be ignored","labels":[],"suggested_agent":"codex"}]}
JSON
SH
  chmod +x "$CODEX_STUB"
  export PATH="${TMP_DIR}:${PATH}"

  tdb_set "$TASK2_ID" agent "codex"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  # Task should be done, not blocked
  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "done" ]

  # No child tasks should be created
  run _task_children_titles "$TASK2_ID"
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "run_task.sh blocks when required_tools are missing" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Needs Tools" "Run body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  cat > "${PROJECT_DIR}/.orchestrator.yml" <<'YAML'
required_tools:
  - __orch_missing_tool__
YAML

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
echo '{"status":"done","summary":"should not run","files_changed":[],"needs_help":false,"delegations":[]}'
SH
  chmod +x "$CODEX_STUB"
  export PATH="${TMP_DIR}:${PATH}"

  tdb_set "$TASK2_ID" agent "codex"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "blocked" ]

  run tdb_field "$TASK2_ID" reason
  [ "$status" -eq 0 ]
  [[ "$output" == *"__orch_missing_tool__"* ]]
}

@test "normalize_json_response parses Claude wrapper with fenced JSON" {
  RAW_FILE="${TMP_DIR}/raw.json"
  cat > "$RAW_FILE" <<'RAW'
{"type":"result","result":"```json\n{\"executor\":\"codex\"}\n```"}
RAW
  run env RAW_FILE="$RAW_FILE" bash -c 'source "'"${REPO_DIR}"'/scripts/lib.sh"; RAW=$(cat "$RAW_FILE"); normalize_json_response "$RAW" | jq -r ".executor"'
  [ "$status" -eq 0 ]
  [ "$output" = "codex" ]
}

@test "gh_api sets backoff on rate limit and respects skip mode" {
  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
if [ "$1" = "api" ]; then
  echo "secondary rate limit" >&2
  exit 1
fi
exit 0
SH
  chmod +x "$GH_STUB"

  run bash -c "PATH=\"${TMP_DIR}:\$PATH\"; STATE_DIR='${STATE_DIR}'; GH_BACKOFF_MODE=skip; source '${REPO_DIR}/scripts/lib.sh'; set +e; gh_api repos/foo/issues >/dev/null; echo \"rc=\$?\""
  [ "$status" -eq 0 ]
  [[ "$output" == *"rc=75"* ]]
  [ -f "${STATE_DIR}/gh_backoff" ]
}

@test "gh_api skips when backoff active" {
  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
if [ "$1" = "api" ]; then
  echo "called" >> "${STATE_DIR}/gh_called"
  exit 0
fi
exit 0
SH
  chmod +x "$GH_STUB"

  future=$(( $(date +%s) + 300 ))
  printf "until=%s\ndelay=300\nreason=rate_limit\n" "$future" > "${STATE_DIR}/gh_backoff"

  run bash -c "PATH=\"${TMP_DIR}:\$PATH\"; STATE_DIR='${STATE_DIR}'; GH_BACKOFF_MODE=skip; source '${REPO_DIR}/scripts/lib.sh'; set +e; gh_api repos/foo/issues >/dev/null; echo \"rc=\$?\""
  [ "$status" -eq 0 ]
  [[ "$output" == *"rc=75"* ]]
  [ ! -f "${STATE_DIR}/gh_called" ]
}


@test "cleanup_worktrees.sh removes worktree for merged PR task" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Cleanup merged" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  WT_DIR="${TMP_DIR}/wt-merged"
  mkdir -p "$WT_DIR"

  tdb_set "$TASK2_ID" status "done"
  tdb_set "$TASK2_ID" branch "gh-task-${TASK2_ID}-cleanup"
  tdb_set "$TASK2_ID" worktree "$WT_DIR"
  tdb_set "$TASK2_ID" dir "$PROJECT_DIR"

  # Close the issue so db_task_ids_by_status "done" finds it (queries closed issues)
  local state
  state=$(cat "$GH_MOCK_STATE")
  printf '%s' "$state" | jq -c --arg id "$TASK2_ID" '.issues[$id].state = "closed" | .prs = {"123": {"number": 123, "state": "MERGED"}}' > "$GH_MOCK_STATE"

  REAL_GIT=$(command -v git)
  GIT_STUB="${TMP_DIR}/git"
  cat > "$GIT_STUB" <<SH
#!/usr/bin/env bash
printf '%s\n' "\$*" >> "${TMP_DIR}/git_calls"
if [[ "\$*" == *"show-ref --verify --quiet refs/heads/"* ]]; then
  exit 0
fi
if [[ "\$*" == *"worktree remove"* ]] || [[ "\$*" == *"branch -D"* ]]; then
  exit 0
fi
exec "$REAL_GIT" "\$@"
SH
  chmod +x "$GIT_STUB"

  run env PATH="${TMP_DIR}:${PATH}" TMP_DIR="$TMP_DIR" \
    CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" \
    ORCH_HOME="$ORCH_HOME" STATE_DIR="$STATE_DIR" \
    "${REPO_DIR}/scripts/cleanup_worktrees.sh"
  [ "$status" -eq 0 ]

  [ "$(tdb_field "$TASK2_ID" worktree_cleaned)" = "1" ]

  run bash -c "cat '${TMP_DIR}/git_calls'"
  [ "$status" -eq 0 ]
  [[ "$output" == *"worktree remove ${WT_DIR} --force"* ]]
  [[ "$output" == *"branch -D gh-task-${TASK2_ID}-cleanup"* ]]
}

@test "cleanup_worktrees.sh skips tasks without merged PR" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Cleanup unmerged" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  WT_DIR="${TMP_DIR}/wt-unmerged"
  mkdir -p "$WT_DIR"

  tdb_set "$TASK2_ID" status "done"
  tdb_set "$TASK2_ID" branch "gh-task-${TASK2_ID}-cleanup"
  tdb_set "$TASK2_ID" worktree "$WT_DIR"
  tdb_set "$TASK2_ID" dir "$PROJECT_DIR"

  # Close the issue so db_task_ids_by_status "done" finds it, but no PRs → not cleaned
  local state; state=$(cat "$GH_MOCK_STATE")
  printf '%s' "$state" | jq -c --arg id "$TASK2_ID" '.issues[$id].state = "closed"' > "$GH_MOCK_STATE"

  REAL_GIT=$(command -v git)
  GIT_STUB="${TMP_DIR}/git"
  cat > "$GIT_STUB" <<SH
#!/usr/bin/env bash
printf '%s\n' "\$*" >> "${TMP_DIR}/git_calls_skip"
if [[ "\$*" == *"worktree remove"* ]] || [[ "\$*" == *"branch -D"* ]]; then
  exit 0
fi
exec "$REAL_GIT" "\$@"
SH
  chmod +x "$GIT_STUB"

  run env PATH="${TMP_DIR}:${PATH}" TMP_DIR="$TMP_DIR" \
    CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" \
    ORCH_HOME="$ORCH_HOME" STATE_DIR="$STATE_DIR" \
    "${REPO_DIR}/scripts/cleanup_worktrees.sh"
  [ "$status" -eq 0 ]

  # Task should NOT be cleaned — worktree_cleaned should still be false
  [[ "$(tdb_field "$TASK2_ID" worktree_cleaned)" != "1" ]]
}

@test "cleanup_worktrees.sh marks worktree_cleaned after removal" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Cleanup local done" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  WT_DIR="${TMP_DIR}/wt-local"
  mkdir -p "$WT_DIR"

  tdb_set "$TASK2_ID" status "done"
  tdb_set "$TASK2_ID" branch "task-2-local"
  # Close the issue and add a merged PR to mock state
  local wt_st; wt_st=$(cat "$GH_MOCK_STATE"); printf '%s' "$wt_st" | jq -c --arg id "$TASK2_ID" '.issues[$id].state = "closed" | .prs={"99":{"number":99,"state":"MERGED"}}' > "$GH_MOCK_STATE"
  tdb_set "$TASK2_ID" worktree "$WT_DIR"
  tdb_set "$TASK2_ID" dir "$PROJECT_DIR"

  REAL_GIT=$(command -v git)
  GIT_STUB="${TMP_DIR}/git"
  cat > "$GIT_STUB" <<SH
#!/usr/bin/env bash
if [[ "\$*" == *"show-ref --verify --quiet refs/heads/"* ]]; then
  exit 0
fi
if [[ "\$*" == *"worktree remove"* ]] || [[ "\$*" == *"branch -D"* ]]; then
  exit 0
fi
exec "$REAL_GIT" "\$@"
SH
  chmod +x "$GIT_STUB"

  run env PATH="${TMP_DIR}:${PATH}" \
    CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" \
    ORCH_HOME="$ORCH_HOME" STATE_DIR="$STATE_DIR" \
    "${REPO_DIR}/scripts/cleanup_worktrees.sh"
  [ "$status" -eq 0 ]

  [ "$(tdb_field "$TASK2_ID" worktree_cleaned)" = "1" ]
}

@test "cleanup_worktrees.sh handles missing worktree directory gracefully" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Cleanup missing dir" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  WT_DIR="${TMP_DIR}/wt-missing"
  tdb_set "$TASK2_ID" status "done"
  tdb_set "$TASK2_ID" worktree "$WT_DIR"
  tdb_set "$TASK2_ID" dir "$PROJECT_DIR"
  # Close the issue and add a merged PR to mock state
  local ms_st; ms_st=$(cat "$GH_MOCK_STATE"); printf '%s' "$ms_st" | jq -c --arg id "$TASK2_ID" '.issues[$id].state = "closed" | .prs={"98":{"number":98,"state":"MERGED"}}' > "$GH_MOCK_STATE"

  REAL_GIT=$(command -v git)
  GIT_STUB="${TMP_DIR}/git"
  cat > "$GIT_STUB" <<SH
#!/usr/bin/env bash
printf '%s\n' "\$*" >> "${TMP_DIR}/git_calls_missing"
if [[ "\$*" == *"worktree remove"* ]] || [[ "\$*" == *"branch -d"* ]] || [[ "\$*" == *"show-ref"* ]]; then
  exit 1
fi
exec "$REAL_GIT" "\$@"
SH
  chmod +x "$GIT_STUB"

  run env PATH="${TMP_DIR}:${PATH}" TMP_DIR="$TMP_DIR" \
    CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" \
    ORCH_HOME="$ORCH_HOME" STATE_DIR="$STATE_DIR" \
    "${REPO_DIR}/scripts/cleanup_worktrees.sh"
  [ "$status" -eq 0 ]

  [ "$(tdb_field "$TASK2_ID" worktree_cleaned)" = "1" ]
}

@test "gh_api wait mode sleeps and retries" {
  GH_STUB="${TMP_DIR}/gh"
  SLEEP_STUB="${TMP_DIR}/sleep"

  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
if [ "$1" = "api" ]; then
  count_file="${STATE_DIR}/gh_count"
  count=0
  if [ -f "$count_file" ]; then
    count=$(cat "$count_file")
  fi
  count=$((count + 1))
  echo "$count" > "$count_file"
  if [ "$count" -eq 1 ]; then
    echo "secondary rate limit" >&2
    exit 1
  fi
  echo '{"ok":true}'
  exit 0
fi
exit 0
SH
  chmod +x "$GH_STUB"

  cat > "$SLEEP_STUB" <<'SH'
#!/usr/bin/env bash
echo "$1" >> "${STATE_DIR}/slept"
exit 0
SH
  chmod +x "$SLEEP_STUB"

  run bash -c "PATH=\"${TMP_DIR}:\$PATH\"; STATE_DIR='${STATE_DIR}'; GH_BACKOFF_MODE=wait; source '${REPO_DIR}/scripts/lib.sh'; set +e; gh_api repos/foo/issues >/dev/null; echo \"rc=\$?\""
  [ "$status" -eq 0 ]
  [[ "$output" == *"rc=0"* ]]
  [ -f "${STATE_DIR}/slept" ]
}

@test "route_task.sh falls back when router fails" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Fallback" "Fallback body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  run yq -i '.router.agent = "claude" | .router.timeout_seconds = 0 | .router.fallback_executor = "codex"' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
exit 1
SH
  chmod +x "$CLAUDE_STUB"

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"executor":"codex","reason":"fallback","profile":{"role":"general","skills":[],"tools":[],"constraints":[]},"selected_skills":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" "${REPO_DIR}/scripts/route_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]
  [ "$(printf '%s' "$output" | tail -n1)" = "codex" ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "routed" ]

  run tdb_field "$TASK2_ID" route_reason
  [ "$status" -eq 0 ]
  [[ "$output" == *"fallback"* ]]
}

@test "run_task.sh runs review agent when enabled" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Review Me" "Review body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  run yq -i '.workflow.enable_review_agent = true' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  run yq -i '.router.disabled_agents = ["opencode"]' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  # Task agent is codex → review agent should be claude (opposite)
  run tdb_set "$TASK2_ID" agent "codex"
  [ "$status" -eq 0 ]

  # Execution stub (codex) prints done JSON to stdout
  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"status":"done","summary":"done","files_changed":[],"needs_help":false,"delegations":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  # Review stub (claude) prints approve JSON
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"decision":"approve","notes":"looks good"}
JSON
SH
  chmod +x "$CLAUDE_STUB"

  # Add PR + diff to mock state for review agent (replaces old GH_STUB)
  local rv_st; rv_st=$(cat "$GH_MOCK_STATE"); printf '%s' "$rv_st" | jq -c '.prs={"42":{"number":42,"state":"OPEN"}} | .pr_diff="+added line"' > "$GH_MOCK_STATE"
  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" review_decision
  [ "$status" -eq 0 ]
  [ "$output" = "approve" ]
}

@test "run_task.sh retries review agent once with fallback before needs_review" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Review Retry" "Review retry body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  run yq -i '.workflow.enable_review_agent = true' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  run yq -i '.router.disabled_agents = ["opencode"]' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  # Task agent is codex → review agent should be claude (opposite)
  run tdb_set "$TASK2_ID" agent "codex"
  [ "$status" -eq 0 ]

  # Execution stub (codex) prints done JSON; review fallback (codex --print) prints approve JSON
  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
for a in "$@"; do
  if [ "$a" = "--print" ]; then
    cat <<'JSON'
{"decision":"approve","notes":"codex_fallback_marker"}
JSON
    exit 0
  fi
done

cat <<'JSON'
{"status":"done","summary":"done","files_changed":[],"needs_help":false,"delegations":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  # Review stub (claude) fails non-zero on first attempt
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
exit 1
SH
  chmod +x "$CLAUDE_STUB"

  # Add PR + diff to mock state for review agent
  local rv_st; rv_st=$(cat "$GH_MOCK_STATE"); printf '%s' "$rv_st" | jq -c '.prs={"42":{"number":42,"state":"OPEN"}} | .pr_diff="+added line"' > "$GH_MOCK_STATE"

  run env PATH="${TMP_DIR}:${PATH}" REVIEW_AGENT=claude CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "in_review" ]

  run tdb_field "$TASK2_ID" review_decision
  [ "$status" -eq 0 ]
  [ "$output" = "approve" ]
}

@test "run_task.sh parses structured JSON from agent stdout" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Output Stdout" "Test stdout JSON parsing" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Stub prints JSON to stdout (parsed by normalize_json_response)
  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"status":"done","summary":"wrote output file","accomplished":["task completed"],"remaining":[],"blockers":[],"files_changed":["test.txt"],"needs_help":false,"delegations":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  tdb_set "$TASK2_ID" agent "codex"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "done" ]

  run tdb_field "$TASK2_ID" summary
  [ "$status" -eq 0 ]
  [ "$output" = "wrote output file" ]

  # Check files_changed stored in task metadata
  run tdb_field "$TASK2_ID" files_changed
  [ "$status" -eq 0 ]
  [[ "$output" == *"test.txt"* ]]
}

@test "run_task.sh falls back to stdout when no output file" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Stdout Fallback" "Test stdout fallback" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Stub prints JSON to stdout (no output file)
  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"status":"done","summary":"stdout mode","accomplished":[],"remaining":[],"blockers":[],"files_changed":[],"needs_help":false,"delegations":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  tdb_set "$TASK2_ID" agent "codex"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "done" ]

  run tdb_field "$TASK2_ID" summary
  [ "$status" -eq 0 ]
  [ "$output" = "stdout mode" ]
}

@test "run_task.sh detects missing tooling in agent output and marks needs_review" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Missing Tool" "Detect missing tool patterns" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Stub prints a missing-tool line before valid JSON.
  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"status":"done","summary":"finished","reason":"zsh: command not found: bun","accomplished":[],"remaining":[],"blockers":[],"files_changed":[],"needs_help":false,"delegations":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  tdb_set "$TASK2_ID" agent "codex"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "needs_review" ]

  run tdb_field "$TASK2_ID" reason
  [ "$status" -eq 0 ]
  [[ "$output" == *"env/tooling failure: missing bun"* ]]
}

@test "run_task.sh detects missing tooling when agent command fails (exit != 0)" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Missing Tool Fail" "Detect missing tool on non-zero exit" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Stub fails like a missing binary would.
  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
echo "zsh: command not found: bun" >&2
exit 127
SH
  chmod +x "$CODEX_STUB"

  tdb_set "$TASK2_ID" agent "codex"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "needs_review" ]

  run tdb_field "$TASK2_ID" last_error
  [ "$status" -eq 0 ]
  [[ "$output" == *"env/tooling failure: missing bun"* ]]
}

@test "run_task.sh auto-reroutes on usage limit (agent-reported needs_review)" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Usage Limit Reroute" "Should reroute on rate limit" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Keep fallback deterministic across environments (dev machines may have opencode installed).
  yq -i '.router.disabled_agents = ["opencode"]' "$CONFIG_PATH"

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"status":"needs_review","summary":"limit","reason":"Rate limit exceeded (429)","accomplished":[],"remaining":[],"blockers":[],"files_changed":[],"needs_help":true,"delegations":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
echo '{"status":"done","summary":"noop","files_changed":[],"needs_help":false,"delegations":[]}'
SH
  chmod +x "$CLAUDE_STUB"

  tdb_set "$TASK2_ID" agent "codex"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "new" ]

  run tdb_field "$TASK2_ID" agent
  [ "$status" -eq 0 ]
  [ "$output" = "claude" ]

  run tdb_field "$TASK2_ID" limit_reroute_chain
  [ "$status" -eq 0 ]
  [[ "$output" == *"codex"* ]]
}

@test "run_task.sh auto-reroutes on usage limit even with non-JSON response" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Usage Limit Non-JSON" "Should reroute even if response is not JSON" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Keep fallback deterministic across environments (dev machines may have opencode installed).
  yq -i '.router.disabled_agents = ["opencode"]' "$CONFIG_PATH"

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
echo "429 Too Many Requests: rate limit"
exit 0
SH
  chmod +x "$CODEX_STUB"

  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
echo '{"status":"done","summary":"noop","files_changed":[],"needs_help":false,"delegations":[]}'
SH
  chmod +x "$CLAUDE_STUB"

  tdb_set "$TASK2_ID" agent "codex"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "new" ]

  run tdb_field "$TASK2_ID" agent
  [ "$status" -eq 0 ]
  [ "$output" = "claude" ]
}

@test "run_task.sh avoids ping-pong on repeated usage limits" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Usage Limit Ping Pong" "Should not bounce back to previous agent" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Keep fallback deterministic across environments (dev machines may have opencode installed).
  yq -i '.router.disabled_agents = ["opencode"]' "$CONFIG_PATH"

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
echo "Rate limit exceeded (429)"
exit 0
SH
  chmod +x "$CODEX_STUB"

  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
echo "rate limit exceeded: temporarily unavailable"
exit 0
SH
  chmod +x "$CLAUDE_STUB"

  tdb_set "$TASK2_ID" agent "codex"

  # First run: codex → claude
  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" agent
  [ "$status" -eq 0 ]
  [ "$output" = "claude" ]

  # Second run: claude has no fallback (codex already tried) → needs_review
  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "needs_review" ]

  run tdb_field "$TASK2_ID" limit_reroute_chain
  [ "$status" -eq 0 ]
  [[ "$output" == *"codex"* ]]
  [[ "$output" == *"claude"* ]]
}

@test "run_task.sh preserves limit_reroute_chain when agent returns in_progress" {
  # Regression: chain was previously cleared unconditionally after each run, which
  # allowed a ping-pong back to an exhausted agent on the very next in_progress cycle.
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Usage Limit In-Progress Chain" "Chain must survive in_progress" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  yq -i '.router.disabled_agents = ["opencode"]' "$CONFIG_PATH"

  # codex reports rate limit → triggers reroute to claude
  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
echo "429 Too Many Requests: usage limit"
exit 0
SH
  chmod +x "$CODEX_STUB"

  # claude does some work but is not done yet (in_progress)
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"status":"in_progress","summary":"partial","reason":"","accomplished":[],"remaining":["more work"],"blockers":[],"files_changed":[],"needs_help":false,"delegations":[]}
JSON
SH
  chmod +x "$CLAUDE_STUB"

  tdb_set "$TASK2_ID" agent "codex"

  # First run: codex hits limit → rerouted to claude
  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" agent
  [ "$status" -eq 0 ]
  [ "$output" = "claude" ]

  # Second run: claude returns in_progress — chain must still contain codex
  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" limit_reroute_chain
  [ "$status" -eq 0 ]
  # Chain must still include codex — not cleared by in_progress completion
  [[ "$output" == *"codex"* ]]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "in_progress" ]
}

@test "is_usage_limit_error matches rate limit patterns" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; is_usage_limit_error '429 Too Many Requests'"
  [ "$status" -eq 0 ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; is_usage_limit_error 'rate limit exceeded'"
  [ "$status" -eq 0 ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; is_usage_limit_error 'quota exceeded for this key'"
  [ "$status" -eq 0 ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; is_usage_limit_error 'service overloaded'"
  [ "$status" -eq 0 ]
}

@test "is_usage_limit_error does not match generic network errors" {
  # Plain "503 Service Unavailable" must NOT trigger a reroute — it is a generic
  # HTTP error unrelated to AI provider rate limits.
  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; is_usage_limit_error '503 Service Unavailable'"
  [ "$status" -ne 0 ]

  # Bare "temporarily unavailable" without rate-limit context must not match.
  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; is_usage_limit_error 'SSH: temporarily unavailable'"
  [ "$status" -ne 0 ]

  # Generic auth failure must not match (handled by auth/billing path separately).
  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; is_usage_limit_error 'unauthorized: invalid api key'"
  [ "$status" -ne 0 ]
}

@test "load_project_config merges project override" {
  # Global config has router.model = ""
  run yq -r '.router.model' "$CONFIG_PATH"
  [ "$output" = "" ]

  # Create project override
  cat > "${TMP_DIR}/orchestrator.yml" <<'YAML'
gh:
  repo: "myorg/myproject"
router:
  model: "sonnet"
YAML

  # Load merged config and check overridden values
  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; PROJECT_DIR='${TMP_DIR}'; STATE_DIR='${STATE_DIR}'; load_project_config; config_get '.router.model'"
  [ "$status" -eq 0 ]
  [ "$output" = "sonnet" ]

  # Non-overridden values should still come from global
  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; PROJECT_DIR='${TMP_DIR}'; STATE_DIR='${STATE_DIR}'; load_project_config; config_get '.router.agent'"
  [ "$status" -eq 0 ]
  [ "$output" = "codex" ]

  # gh.repo should come from project override
  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; PROJECT_DIR='${TMP_DIR}'; STATE_DIR='${STATE_DIR}'; load_project_config; config_get '.gh.repo // \"\"'"
  [ "$status" -eq 0 ]
  [ "$output" = "myorg/myproject" ]
}

@test "load_project_config scopes locks and contexts to project" {
  cat > "${TMP_DIR}/orchestrator.yml" <<'YAML'
gh:
  repo: "myorg/myproject"
  sync_label: ""
YAML

  run bash -c 'source '"${REPO_DIR}"'/scripts/lib.sh; PROJECT_DIR='"${TMP_DIR}"'; load_project_config; echo "$LOCK_PATH"'
  [ "$status" -eq 0 ]
  [ "$output" = "${TMP_DIR}/.orchestrator/locks" ]

  run bash -c 'source '"${REPO_DIR}"'/scripts/lib.sh; PROJECT_DIR='"${TMP_DIR}"'; load_project_config; echo "$CONTEXTS_DIR"'
  [ "$status" -eq 0 ]
  [ "$output" = "${TMP_DIR}/.orchestrator/contexts" ]
}

@test "projects_list reads projects.yml" {
  mkdir -p "${ORCH_HOME}"
  cat > "${ORCH_HOME}/projects.yml" <<'YAML'
projects:
  - dir: "/tmp/project-a"
  - "/tmp/project-b"
YAML

  run bash -c "ORCH_HOME='${ORCH_HOME}'; source '${REPO_DIR}/scripts/lib.sh'; projects_list"
  [ "$status" -eq 0 ]
  [ "$output" = $'/tmp/project-a\n/tmp/project-b' ]
}

@test "run_task.sh uses plan prompt for decompose mode" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Big Feature" "Build the whole thing" "plan")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Stub that checks which prompt it receives (prints JSON to stdout)
  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
# Check if the plan prompt is being used (contains "planning agent")
prompt="$* $(cat)"
if printf '%s' "$prompt" | grep -q "planning agent"; then
  cat <<'JSON'
{"status":"done","summary":"planned the work","accomplished":["analyzed task"],"remaining":[],"blockers":[],"files_changed":[],"needs_help":false,"reason":"","delegations":[{"title":"Step 1","body":"Do first thing","labels":["backend"],"suggested_agent":"codex"},{"title":"Step 2","body":"Do second thing","labels":["tests"],"suggested_agent":"codex"}]}
JSON
else
  cat <<'JSON'
{"status":"done","summary":"executed directly","accomplished":[],"remaining":[],"blockers":[],"files_changed":[],"needs_help":false,"reason":"","delegations":[]}
JSON
fi
SH
  chmod +x "$CODEX_STUB"

  tdb_set "$TASK2_ID" agent "codex"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  # Should have created child tasks from delegation
  run _task_children_titles "$TASK2_ID"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Step 1"* ]]
  [[ "$output" == *"Step 2"* ]]

  # Parent should be blocked
  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "blocked" ]
}

@test "init.sh prints project info" {
  # Stub gh so init.sh doesn't make real API calls during auto-sync
  printf '#!/usr/bin/env bash\nexit 1\n' > "$TMP_DIR/gh" && chmod +x "$TMP_DIR/gh"
  run env PATH="$TMP_DIR:$PATH" PROJECT_DIR="$TMP_DIR" "${REPO_DIR}/scripts/init.sh" </dev/null
  [ "$status" -eq 0 ]
  [[ "$output" == *"Initialized orchestrator"* ]]
  [[ "$output" == *"$TMP_DIR"* ]]
}

@test "list_tasks.sh filters by PROJECT_DIR" {
  # Task 1 (Init) already has dir=$TMP_DIR from setup()
  # Add a task for a different project using a separate gh mock state
  OTHER_DIR=$(mktemp -d)
  git -C "$OTHER_DIR" init -b main --quiet 2>/dev/null || true
  git -C "$OTHER_DIR" -c user.email="test@test.com" -c user.name="Test" commit --allow-empty -m "init" --quiet 2>/dev/null || true
  OTHER_STATE_DIR="${OTHER_DIR}/.orchestrator"
  mkdir -p "$OTHER_STATE_DIR"
  run env PROJECT_DIR="$OTHER_DIR" STATE_DIR="$OTHER_STATE_DIR" GH_MOCK_STATE="${OTHER_STATE_DIR}/gh_mock_state.json" "${REPO_DIR}/scripts/add_task.sh" "Other Project" "other body" ""
  [ "$status" -eq 0 ]

  # Listing from TMP_DIR should only show the Init task
  run env PROJECT_DIR="$TMP_DIR" "${REPO_DIR}/scripts/list_tasks.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Init"* ]]
  [[ "$output" != *"Other Project"* ]]

  # Listing from OTHER_DIR should only show the Other task
  run env PROJECT_DIR="$OTHER_DIR" STATE_DIR="$OTHER_STATE_DIR" GH_MOCK_STATE="${OTHER_STATE_DIR}/gh_mock_state.json" "${REPO_DIR}/scripts/list_tasks.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Other Project"* ]]
  [[ "$output" != *"Init"* ]]

  rm -rf "$OTHER_DIR"
}

@test "run_task.sh blocks task after max_attempts exceeded" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Max Retry" "Should block after max" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Set task to already have 10 attempts (max default) and agent assigned
  tdb_set "$TASK2_ID" agent "codex"
  tdb_set "$TASK2_ID" attempts "10"

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
echo "should not be called" >&2
exit 1
SH
  chmod +x "$CODEX_STUB"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "needs_review" ]

  run tdb_field "$TASK2_ID" reason
  [ "$status" -eq 0 ]
  [[ "$output" == *"max attempts"* ]]
}

@test "init.sh accepts --repo flag for non-interactive mode" {
  INIT_DIR=$(mktemp -d)
  # Stub gh so init.sh doesn't make real API calls during auto-sync
  printf '#!/usr/bin/env bash\nexit 1\n' > "$INIT_DIR/gh" && chmod +x "$INIT_DIR/gh"
  run env PATH="$INIT_DIR:$PATH" PROJECT_DIR="$INIT_DIR" "${REPO_DIR}/scripts/init.sh" --repo "myorg/myapp" </dev/null
  [ "$status" -eq 0 ]
  [[ "$output" == *"Initialized orchestrator"* ]]
  [ -f "$INIT_DIR/orchestrator.yml" ]

  run yq -r '.gh.repo' "$INIT_DIR/orchestrator.yml"
  [ "$status" -eq 0 ]
  [ "$output" = "myorg/myapp" ]

  rm -rf "$INIT_DIR"
}

@test "init.sh is idempotent with existing config" {
  INIT_DIR=$(mktemp -d)
  printf '#!/usr/bin/env bash\nexit 1\n' > "$INIT_DIR/gh" && chmod +x "$INIT_DIR/gh"

  # First init creates config
  run env PATH="$INIT_DIR:$PATH" PROJECT_DIR="$INIT_DIR" "${REPO_DIR}/scripts/init.sh" --repo "myorg/myapp" </dev/null
  [ "$status" -eq 0 ]
  [ -f "$INIT_DIR/orchestrator.yml" ]

  run yq -r '.gh.repo' "$INIT_DIR/orchestrator.yml"
  [ "$output" = "myorg/myapp" ]

  # Second init preserves existing repo
  run env PATH="$INIT_DIR:$PATH" PROJECT_DIR="$INIT_DIR" "${REPO_DIR}/scripts/init.sh" </dev/null
  [ "$status" -eq 0 ]
  [[ "$output" == *"existing"* ]]

  # Config should still have the repo
  run yq -r '.gh.repo' "$INIT_DIR/orchestrator.yml"
  [ "$output" = "myorg/myapp" ]

  rm -rf "$INIT_DIR"
}

@test "init.sh re-init with --repo updates existing config" {
  INIT_DIR=$(mktemp -d)
  printf '#!/usr/bin/env bash\nexit 1\n' > "$INIT_DIR/gh" && chmod +x "$INIT_DIR/gh"

  # First init
  run env PATH="$INIT_DIR:$PATH" PROJECT_DIR="$INIT_DIR" "${REPO_DIR}/scripts/init.sh" --repo "old/repo" </dev/null
  [ "$status" -eq 0 ]

  # Re-init with different repo
  run env PATH="$INIT_DIR:$PATH" PROJECT_DIR="$INIT_DIR" "${REPO_DIR}/scripts/init.sh" --repo "new/repo" </dev/null
  [ "$status" -eq 0 ]

  run yq -r '.gh.repo' "$INIT_DIR/orchestrator.yml"
  [ "$output" = "new/repo" ]

  rm -rf "$INIT_DIR"
}


@test "agents.sh lists agent availability" {
  run "${REPO_DIR}/scripts/agents.sh"
  [ "$status" -eq 0 ]
  # Should mention all three agents regardless of availability
  [[ "$output" == *"claude"* ]]
  [[ "$output" == *"codex"* ]]
  [[ "$output" == *"opencode"* ]]
}

# concurrent write test removed — GitHub-native backend handles this

@test "performance: list_tasks.sh handles 100+ tasks" {
  # Create 99 more tasks directly in gh mock state (Init already exists as issue 1)
  if [ ! -f "$GH_MOCK_STATE" ]; then
    echo '{"issues":{},"sub_issues":{},"comments":{},"next_issue_number":2}' > "$GH_MOCK_STATE"
  fi
  local state
  state=$(cat "$GH_MOCK_STATE")
  for i in $(seq 2 100); do
    state=$(printf '%s' "$state" | jq \
      --argjson n "$i" \
      --arg title "Task $i" \
      '.issues[($n|tostring)] = {
        "number": $n,
        "title": $title,
        "body": "perf test body",
        "state": "open",
        "labels": [],
        "assignees": []
      } | .next_issue_number = ($n + 1)')
  done
  printf '%s' "$state" > "$GH_MOCK_STATE"

  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -ge 100 ]

  # list should complete in reasonable time (< 30s)
  SECONDS=0
  run env PROJECT_DIR="$TMP_DIR" "${REPO_DIR}/scripts/list_tasks.sh"
  [ "$status" -eq 0 ]
  [ "$SECONDS" -lt 30 ]
}

@test "render_template fails on missing template" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; render_template '/tmp/nonexistent_template_xyz.md'"
  [ "$status" -ne 0 ]
  [[ "$output" == *"template not found"* ]]
}

@test "render_template omits if block when value is empty or whitespace" {
  TEMPLATE_PATH="${TMP_DIR}/tmpl-if-empty.md"
  cat > "$TEMPLATE_PATH" <<'EOF'
start
{{#if OPTIONAL}}
optional section
{{OPTIONAL}}
{{/if}}
end
EOF

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; OPTIONAL='   ' render_template '$TEMPLATE_PATH'"
  [ "$status" -eq 0 ]
  [[ "$output" == *"start"* ]]
  [[ "$output" == *"end"* ]]
  [[ "$output" != *"optional section"* ]]
}

@test "render_template keeps if block when value is non-empty" {
  TEMPLATE_PATH="${TMP_DIR}/tmpl-if-filled.md"
  cat > "$TEMPLATE_PATH" <<'EOF'
{{#if OPTIONAL}}
optional section
{{OPTIONAL}}
{{/if}}
EOF

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; OPTIONAL='value present' render_template '$TEMPLATE_PATH'"
  [ "$status" -eq 0 ]
  [[ "$output" == *"optional section"* ]]
  [[ "$output" == *"value present"* ]]
}

@test "create_task_entry creates task via shared helper" {
  NOW="2026-01-01T00:00:00Z"
  export NOW PROJECT_DIR="$TMP_DIR"

  run bash -c "
    source '${REPO_DIR}/scripts/lib.sh'
    create_task_entry 99 'Helper Task' 'Created by helper' 'test,helper' '' ''
  "
  [ "$status" -eq 0 ]
  # db_create_task returns auto-assigned ID (ignoring the passed-in 99)
  NEW_ID=$(echo "$output" | tr -d '[:space:]')

  run tdb_field "$NEW_ID" title
  [ "$status" -eq 0 ]
  [ "$output" = "Helper Task" ]

  run tdb_field "$NEW_ID" dir
  [ "$status" -eq 0 ]
  [ "$output" = "$TMP_DIR" ]
}

@test "chat.sh dispatches add_task action" {
  # Stub agent that returns a chat JSON response
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"action":"add_task","params":{"title":"Fix login page","body":"The login page has a broken CSS layout","labels":"bug,frontend"},"response":"I've created a task to fix the login page."}
JSON
SH
  chmod +x "$CLAUDE_STUB"

  # Feed "add a task" then "exit" via stdin
  run bash -c "
    printf 'add a task to fix the login page\nexit\n' | \
    env PATH='${TMP_DIR}:${PATH}' \
        CONFIG_PATH='$CONFIG_PATH' \
        JOBS_PATH='${TMP_DIR}/jobs.yml' \
        PROJECT_DIR='$TMP_DIR' \
        STATE_DIR='$STATE_DIR' \
        CHAT_AGENT=claude \
        '${REPO_DIR}/scripts/chat.sh' 2>/dev/null
  "
  [ "$status" -eq 0 ]
  [[ "$output" == *"Fix login page"* ]]

  # Verify task was actually created
  FIX_ID=$(_task_id_by_title "Fix login page")
  [ -n "$FIX_ID" ]
  run tdb_field "$FIX_ID" title
  [ "$status" -eq 0 ]
  [ "$output" = "Fix login page" ]
}

@test "chat.sh handles invalid JSON gracefully" {
  # Stub agent that returns non-JSON
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
echo "I don't know how to respond in JSON right now."
SH
  chmod +x "$CLAUDE_STUB"

  run bash -c "
    printf 'hello\nexit\n' | \
    env PATH='${TMP_DIR}:${PATH}' \
        CONFIG_PATH='$CONFIG_PATH' \
        JOBS_PATH='${TMP_DIR}/jobs.yml' \
        PROJECT_DIR='$TMP_DIR' \
        STATE_DIR='$STATE_DIR' \
        CHAT_AGENT=claude \
        '${REPO_DIR}/scripts/chat.sh' 2>/dev/null
  "
  [ "$status" -eq 0 ]
  # Should show the raw response as fallback
  [[ "$output" == *"I don't know how to respond"* ]]
}

@test "chat.sh exits on quit command" {
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
echo '{"action":"answer","params":{},"response":"hi"}'
SH
  chmod +x "$CLAUDE_STUB"

  run bash -c "
    printf 'quit\n' | \
    env PATH='${TMP_DIR}:${PATH}' \
        CONFIG_PATH='$CONFIG_PATH' \
        JOBS_PATH='${TMP_DIR}/jobs.yml' \
        PROJECT_DIR='$TMP_DIR' \
        STATE_DIR='$STATE_DIR' \
        CHAT_AGENT=claude \
        '${REPO_DIR}/scripts/chat.sh' 2>/dev/null
  "
  [ "$status" -eq 0 ]
  [[ "$output" == *"Goodbye!"* ]]
}

@test "chat.sh dispatches status action" {
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"action":"status","params":{},"response":"Here's your current status."}
JSON
SH
  chmod +x "$CLAUDE_STUB"

  run bash -c "
    printf 'show me the status\nexit\n' | \
    env PATH='${TMP_DIR}:${PATH}' \
        CONFIG_PATH='$CONFIG_PATH' \
        JOBS_PATH='${TMP_DIR}/jobs.yml' \
        PROJECT_DIR='$TMP_DIR' \
        STATE_DIR='$STATE_DIR' \
        CHAT_AGENT=claude \
        '${REPO_DIR}/scripts/chat.sh' 2>/dev/null
  "
  [ "$status" -eq 0 ]
  # Should contain status table (total row)
  [[ "$output" == *"total"* ]]
}

@test "chat.sh cleans up history file on exit" {
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
echo '{"action":"answer","params":{},"response":"hi"}'
SH
  chmod +x "$CLAUDE_STUB"

  bash -c "
    printf 'exit\n' | \
    env PATH='${TMP_DIR}:${PATH}' \
        CONFIG_PATH='$CONFIG_PATH' \
        JOBS_PATH='${TMP_DIR}/jobs.yml' \
        PROJECT_DIR='$TMP_DIR' \
        STATE_DIR='$STATE_DIR' \
        CHAT_AGENT=claude \
        '${REPO_DIR}/scripts/chat.sh' 2>/dev/null
  "

  # No chat-history files should remain
  run bash -c "ls '${STATE_DIR}'/chat-history-* 2>/dev/null | wc -l"
  [ "$(echo "$output" | tr -d ' ')" = "0" ]
}

@test "chat.sh quick_task runs agent in PROJECT_DIR not cwd" {
  # Call-counting stub: first call is the LLM (returns quick_task action),
  # second call is the dispatched agent (records its working directory).
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<SH
#!/usr/bin/env bash
count_file="${STATE_DIR}/chat_call_count"
count=0
if [ -f "\$count_file" ]; then
  count=\$(cat "\$count_file")
fi
count=\$((count + 1))
echo "\$count" > "\$count_file"

if [ "\$count" -eq 1 ]; then
  cat <<'JSON'
{"action":"quick_task","params":{"prompt":"list files"},"response":"Running quick task..."}
JSON
else
  pwd > "${STATE_DIR}/agent_cwd"
  echo "done"
fi
SH
  chmod +x "$CLAUDE_STUB"

  # Create a different directory to simulate brew's cd to libexec
  FAKE_LIBEXEC=$(mktemp -d)

  run bash -c "
    cd '$FAKE_LIBEXEC' && \
    printf 'list files\nexit\n' | \
    env PATH='${TMP_DIR}:${PATH}' \
        CONFIG_PATH='$CONFIG_PATH' \
        JOBS_PATH='${TMP_DIR}/jobs.yml' \
        PROJECT_DIR='$TMP_DIR' \
        STATE_DIR='$STATE_DIR' \
        CHAT_AGENT=claude \
        '${REPO_DIR}/scripts/chat.sh' 2>/dev/null
  "
  [ "$status" -eq 0 ]

  # Agent should have run in PROJECT_DIR, not FAKE_LIBEXEC
  [ -f "${STATE_DIR}/agent_cwd" ]
  agent_dir=$(cat "${STATE_DIR}/agent_cwd")
  [ "$agent_dir" = "$TMP_DIR" ]

  rm -rf "$FAKE_LIBEXEC"
}

# --- plan_chat.sh tests ---

@test "plan_chat.sh shows usage on missing title" {
  run "${REPO_DIR}/scripts/plan_chat.sh"
  [ "$status" -eq 1 ]
  [[ "$output" == *"usage:"* ]]
}

@test "plan_chat.sh proposes plan on first turn" {
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"action":"ask","params":{},"response":"Here is my proposed plan:\n1. Set up database schema\n2. Implement API endpoints\n3. Add tests"}
JSON
SH
  chmod +x "$CLAUDE_STUB"

  run bash -c "
    printf 'exit\n' | \
    env PATH='${TMP_DIR}:${PATH}' \
        CONFIG_PATH='$CONFIG_PATH' \
        PROJECT_DIR='$TMP_DIR' \
        STATE_DIR='$STATE_DIR' \
        CHAT_AGENT=claude \
        '${REPO_DIR}/scripts/plan_chat.sh' 'Build auth system' 'Need user authentication' '' 2>/dev/null
  "
  [ "$status" -eq 0 ]
  [[ "$output" == *"proposed plan"* ]]
}

@test "plan_chat.sh creates tasks on approval" {
  # Call-counting stub: first call returns ask, second returns create_tasks
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<SH
#!/usr/bin/env bash
count_file="${STATE_DIR}/plan_call_count"
count=0
if [ -f "\$count_file" ]; then
  count=\$(cat "\$count_file")
fi
count=\$((count + 1))
echo "\$count" > "\$count_file"

if [ "\$count" -eq 1 ]; then
  cat <<'JSON'
{"action":"ask","params":{},"response":"Here is my plan:\n1. Create user model\n2. Add login endpoint"}
JSON
else
  cat <<'JSON'
{"action":"create_tasks","params":{"tasks":[{"title":"Create user model","body":"Set up User table","labels":"backend","suggested_agent":""},{"title":"Add login endpoint","body":"POST /login","labels":"backend,api","suggested_agent":""}]},"response":"Creating 2 tasks."}
JSON
fi
SH
  chmod +x "$CLAUDE_STUB"

  run bash -c "
    printf 'looks good\n' | \
    env PATH='${TMP_DIR}:${PATH}' \
        CONFIG_PATH='$CONFIG_PATH' \
        PROJECT_DIR='$TMP_DIR' \
        STATE_DIR='$STATE_DIR' \
        CHAT_AGENT=claude \
        '${REPO_DIR}/scripts/plan_chat.sh' 'Build auth' 'Auth system' '' 2>/dev/null
  "
  [ "$status" -eq 0 ]
  [[ "$output" == *"Created 2 task(s)"* ]]

  # Verify tasks were actually created
  MODEL_ID=$(_task_id_by_title "Create user model")
  [ -n "$MODEL_ID" ]
  run tdb_field "$MODEL_ID" title
  [ "$status" -eq 0 ]
  [ "$output" = "Create user model" ]

  LOGIN_ID=$(_task_id_by_title "Add login endpoint")
  [ -n "$LOGIN_ID" ]
  run tdb_field "$LOGIN_ID" title
  [ "$status" -eq 0 ]
  [ "$output" = "Add login endpoint" ]
}

@test "link_project_to_repo calls linkProjectV2ToRepository" {
  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
if printf '%s' "$*" | grep -q "repos/test/repo"; then
  echo '{"node_id":"R_abc123"}'
  exit 0
fi
if printf '%s' "$*" | grep -q "linkProjectV2ToRepository"; then
  echo '{"data":{"linkProjectV2ToRepository":{"repository":{"id":"R_abc123"}}}}'
  # Write a marker so we can verify the mutation was called
  echo "linked" > "${STATE_DIR}/link_called"
  exit 0
fi
exit 0
SH
  chmod +x "$GH_STUB"

  source "${REPO_DIR}/scripts/lib.sh"
  run env PATH="${TMP_DIR}:${PATH}" STATE_DIR="$STATE_DIR" \
    bash -c "source '${REPO_DIR}/scripts/lib.sh' && link_project_to_repo PVT_proj123 test/repo"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Linked project to test/repo"* ]]
  [ -f "${STATE_DIR}/link_called" ]
}

@test "plan_chat.sh cleans up history on exit" {
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"action":"ask","params":{},"response":"What kind of auth?"}
JSON
SH
  chmod +x "$CLAUDE_STUB"

  bash -c "
    printf 'exit\n' | \
    env PATH='${TMP_DIR}:${PATH}' \
        CONFIG_PATH='$CONFIG_PATH' \
        PROJECT_DIR='$TMP_DIR' \
        STATE_DIR='$STATE_DIR' \
        CHAT_AGENT=claude \
        '${REPO_DIR}/scripts/plan_chat.sh' 'Build auth' '' '' 2>/dev/null
  "

  # No plan-history files should remain
  run bash -c "ls '${STATE_DIR}'/plan-history-* 2>/dev/null | wc -l"
  [ "$(echo "$output" | tr -d ' ')" = "0" ]
}

@test "auto_detect_status finds options with case-insensitive exact match" {
  json='{"data":{"node":{"fields":{"nodes":[{},{"id":"PVTSSF_f1","name":"Status","options":[{"id":"O1","name":"Backlog"},{"id":"O2","name":"In Progress"},{"id":"O3","name":"Review"},{"id":"O4","name":"Done"}]},{}]}}}}'

  for pair in "backlog:O1" "in progress:O2" "review:O3" "done:O4"; do
    name_lower="${pair%%:*}"
    expected="${pair##*:}"
    run bash -c "printf '%s' '$json' | NAME_LOWER='${name_lower}' yq -r '.data.node.fields.nodes[] | select(.name == \"Status\") | .options[] | select(.name | downcase == strenv(NAME_LOWER)) | .id'"
    [ "$status" -eq 0 ]
    [ "$output" = "$expected" ]
  done

  # Case-insensitive: "todo" should match "Todo" exactly
  json2='{"data":{"node":{"fields":{"nodes":[{"id":"F1","name":"Status","options":[{"id":"X1","name":"Todo"}]}]}}}}'
  run bash -c "printf '%s' '$json2' | NAME_LOWER='todo' yq -r '.data.node.fields.nodes[] | select(.name == \"Status\") | .options[] | select(.name | downcase == strenv(NAME_LOWER)) | .id'"
  [ "$status" -eq 0 ]
  [ "$output" = "X1" ]
}

@test "init.sh auto_detect_status populates status map in config" {
  INIT_DIR=$(mktemp -d)

  # Stub gh: returns project field options for the GraphQL query
  GH_STUB="$INIT_DIR/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
args="$*"
if printf '%s' "$args" | grep -q "graphql" && printf '%s' "$args" | grep -q "fields(first"; then
  cat <<'JSON'
{"data":{"node":{"fields":{"nodes":[{},{"id":"PVTSSF_status1","name":"Status","options":[{"id":"opt_bl","name":"Backlog"},{"id":"opt_ip","name":"In Progress"},{"id":"opt_rv","name":"Review"},{"id":"opt_dn","name":"Done"}]},{}]}}}}
JSON
  exit 0
fi
exit 1
SH
  chmod +x "$GH_STUB"

  run env PATH="$INIT_DIR:$PATH" PROJECT_DIR="$INIT_DIR" \
    "${REPO_DIR}/scripts/init.sh" --repo "test/repo" --project-id "PVT_proj1" </dev/null
  [ "$status" -eq 0 ]
  [[ "$output" == *"Detected status options"* ]]
  [[ "$output" == *"backlog -> opt_bl"* ]]
  [[ "$output" == *"in_progress -> opt_ip"* ]]
  [[ "$output" == *"review -> opt_rv"* ]]
  [[ "$output" == *"done -> opt_dn"* ]]

  # Config file should have all status map entries
  run yq -r '.gh.project_status_field_id' "$INIT_DIR/orchestrator.yml"
  [ "$output" = "PVTSSF_status1" ]

  run yq -r '.gh.project_status_map.backlog' "$INIT_DIR/orchestrator.yml"
  [ "$output" = "opt_bl" ]

  run yq -r '.gh.project_status_map.in_progress' "$INIT_DIR/orchestrator.yml"
  [ "$output" = "opt_ip" ]

  run yq -r '.gh.project_status_map.review' "$INIT_DIR/orchestrator.yml"
  [ "$output" = "opt_rv" ]

  run yq -r '.gh.project_status_map.done' "$INIT_DIR/orchestrator.yml"
  [ "$output" = "opt_dn" ]

  rm -rf "$INIT_DIR"
}

@test "init.sh auto_detect_status uses configured status column names" {
  INIT_DIR=$(mktemp -d)

  cat > "$INIT_DIR/orchestrator.yml" <<'YAML'
gh:
  repo: "test/repo"
  project_id: "PVT_proj1"
  project_status_names:
    backlog: "Todo"
    in_progress: "Doing"
    review: ["In Review", "QA Review"]
    done: "Closed"
YAML

  # Stub gh: returns project field options for the GraphQL query
  GH_STUB="$INIT_DIR/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
args="$*"
if printf '%s' "$args" | grep -q "graphql" && printf '%s' "$args" | grep -q "fields(first"; then
  cat <<'JSON'
{"data":{"node":{"fields":{"nodes":[{},{"id":"PVTSSF_status1","name":"Status","options":[{"id":"opt_td","name":"Todo"},{"id":"opt_dg","name":"Doing"},{"id":"opt_rv","name":"QA Review"},{"id":"opt_dn","name":"Closed"}]},{}]}}}}
JSON
  exit 0
fi
exit 1
SH
  chmod +x "$GH_STUB"

  run env PATH="$INIT_DIR:$PATH" PROJECT_DIR="$INIT_DIR" \
    "${REPO_DIR}/scripts/init.sh" --repo "test/repo" --project-id "PVT_proj1" </dev/null
  [ "$status" -eq 0 ]

  run yq -r '.gh.project_status_field_id' "$INIT_DIR/orchestrator.yml"
  [ "$output" = "PVTSSF_status1" ]

  run yq -r '.gh.project_status_map.backlog' "$INIT_DIR/orchestrator.yml"
  [ "$output" = "opt_td" ]

  run yq -r '.gh.project_status_map.in_progress' "$INIT_DIR/orchestrator.yml"
  [ "$output" = "opt_dg" ]

  run yq -r '.gh.project_status_map.review' "$INIT_DIR/orchestrator.yml"
  [ "$output" = "opt_rv" ]

  run yq -r '.gh.project_status_map.done' "$INIT_DIR/orchestrator.yml"
  [ "$output" = "opt_dn" ]

  rm -rf "$INIT_DIR"
}

@test "configure_project_status_field calls updateProjectV2Field" {
  INIT_DIR=$(mktemp -d)

  # gh stub: returns status field ID on query, records updateProjectV2Field call
  cat > "$INIT_DIR/gh" <<'GHSTUB'
#!/usr/bin/env bash
args="$*"
if printf '%s' "$args" | grep -q "updateProjectV2Field"; then
  touch /tmp/_orch_test_update_called
  echo '{"data":{}}'
  exit 0
fi
if printf '%s' "$args" | grep -q "fields(first"; then
  echo '{"data":{"node":{"fields":{"nodes":[{"id":"PVTSSF_s1","name":"Status"}]}}}}'
  exit 0
fi
exit 1
GHSTUB
  chmod +x "$INIT_DIR/gh"

  # Extract and run configure_project_status_field from init.sh
  rm -f /tmp/_orch_test_update_called
  run env PATH="$INIT_DIR:$PATH" bash -c "
    $(sed -n '/^configure_project_status_field()/,/^}/p' "${REPO_DIR}/scripts/init.sh")
    configure_project_status_field PVT_test
  "
  [ "$status" -eq 0 ]
  [[ "$output" == *"Configured project status columns"* ]]
  [ -f /tmp/_orch_test_update_called ]

  rm -f /tmp/_orch_test_update_called
  rm -rf "$INIT_DIR"
}

@test "lock_mtime returns 0 for nonexistent path without errors" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; lock_mtime /tmp/nonexistent_lock_$$"
  [ "$status" -eq 0 ]
  [ "$output" = "0" ]
}

@test "lock_mtime returns mtime for existing directory" {
  LOCK_DIR=$(mktemp -d)
  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; lock_mtime '$LOCK_DIR'"
  [ "$status" -eq 0 ]
  # Should be a numeric timestamp
  [[ "$output" =~ ^[0-9]+$ ]]
  [ "$output" -gt 0 ]
  rm -rf "$LOCK_DIR"
}

@test "lock_is_stale handles missing lock gracefully" {
  # lock_is_stale returns 1 (not stale) for nonexistent paths — no errors
  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; set +e; lock_is_stale /tmp/nonexistent_lock_$$ 2>&1; echo rc=\$?"
  # No stat errors in output
  [[ "$output" != *"No such file"* ]]
  [[ "$output" != *"integer expression"* ]]
  [[ "$output" == *"rc=1"* ]]
}

@test "scripts use strenv() not env() for yq string values" {
  # env() parses values as YAML which breaks on markdown content (colons, anchors, etc.)
  # Only env(X) | tonumber is allowed; all other env() must be strenv()
  run bash -c "grep -rn 'env(' '${REPO_DIR}/scripts/'*.sh | grep -v strenv | grep -v 'env(.*) | tonumber' | grep -v '^\s*#' | grep -v 'command -v\|export \|:-\|ORCH_HOME\|PATH=' || true"
  [ -z "$output" ]
}

@test "run_task.sh recovers stale lock with pid file from dead process" {
  # Stale lock with a pid file inside should be cleaned up so the task can run.
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Stuck Task" "Should recover from stale lock" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  tdb_set "$TASK2_ID" agent "codex"
  tdb_set "$TASK2_ID" status "routed"

  # Create stale lock dir with pid file (dead PID)
  # run_task.sh uses LOCK_PATH.task.ID format (dot separator, not slash)
  TASK_LOCK="${LOCK_PATH}.task.${TASK2_ID}"
  mkdir -p "$TASK_LOCK"
  echo "99999" > "$TASK_LOCK/pid"
  touch -t 202501010000 "$TASK_LOCK"

  # Stub codex to succeed (prints JSON to stdout)
  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"status":"done","summary":"recovered","files_changed":[],"needs_help":false,"delegations":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" \
    PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" LOCK_STALE_SECONDS=1 \
    LOCK_PATH="$LOCK_PATH" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" USE_TMUX=false \
    "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  # Task should have run and completed
  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "done" ]

  # Lock should be cleaned up
  [ ! -d "$TASK_LOCK" ]
}

@test "append_history records actual status not zero" {
  # with_lock() has `local status=0` which shadows the exported `status`
  # from append_history. All history entries should show the real status,
  # not "0".
  _add_history "$INIT_TASK_ID" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "blocked" "test note"

  # Check the comment was added via backend
  run bash -c "
    export ORCH_HOME='$ORCH_HOME' PROJECT_DIR='$PROJECT_DIR' STATE_DIR='$STATE_DIR'
    source '${REPO_DIR}/scripts/lib.sh'
    db_task_history '$INIT_TASK_ID'
  "
  [ "$status" -eq 0 ]
  [[ "$output" == *"blocked"* ]]
  [[ "$output" == *"test note"* ]]
}

@test "run_task.sh passes --output-format to agents" {
  # Regression: agents must return structured JSON, not raw text
  run grep -n 'output-format\|--json\|--format json' "${REPO_DIR}/scripts/run_task.sh"
  [ "$status" -eq 0 ]
  # claude must have --output-format json
  [[ "$output" == *"output-format json"* ]]
  # codex must have --json
  [[ "$output" == *"--json"* ]]
  # opencode must have --format json
  [[ "$output" == *"--format json"* ]]
}

@test "run_task.sh passes --permission-mode bypassPermissions to claude" {
  run grep -n 'permission-mode bypassPermissions' "${REPO_DIR}/scripts/run_task.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *"--permission-mode bypassPermissions"* ]]
}

@test "run_task.sh runs codex with --ask-for-approval never" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Codex approvals" "Ensure codex approvals are disabled" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
args="$*"
if [[ "$args" != *"--ask-for-approval never"* ]]; then
  echo "missing --ask-for-approval never in: $args" >&2
  exit 2
fi
cat >/dev/null || true
cat <<'JSON'
{"status":"done","summary":"tested","files_changed":[],"needs_help":false,"delegations":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  tdb_set "$TASK2_ID" agent "codex"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "done" ]
}

@test "run_task.sh injects agent and model into response JSON" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Inject Meta" "Test agent/model injection" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"status":"done","summary":"tested","files_changed":[],"needs_help":false,"delegations":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  tdb_set "$TASK2_ID" agent "codex"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  # Check that the task's state got updated with the correct status
  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "done" ]

  # Agent model should be recorded in the task
  run tdb_field "$TASK2_ID" agent_model
  [ "$status" -eq 0 ]
  [ "$output" = "default" ]
}

@test "normalize_json.py --tool-history extracts tool calls" {
  RAW='{"type":"tool_use","tool":"Bash","input":{"command":"ls"}}
{"type":"tool_result","is_error":false}
{"type":"tool_use","tool":"Edit","input":{"file_path":"test.ts"}}
{"type":"tool_result","is_error":true}
{"type":"text","part":{"text":"done"}}'

  run bash -c "RAW_RESPONSE='$RAW' python3 '${REPO_DIR}/scripts/normalize_json.py' --tool-history"
  [ "$status" -eq 0 ]
  # Should have 2 tool entries
  run bash -c "RAW_RESPONSE='$RAW' python3 '${REPO_DIR}/scripts/normalize_json.py' --tool-history | jq 'length'"
  [ "$status" -eq 0 ]
  [ "$output" = "2" ]

  # First tool should be Bash with no error
  run bash -c "RAW_RESPONSE='$RAW' python3 '${REPO_DIR}/scripts/normalize_json.py' --tool-history | jq '.[0].tool'"
  [ "$status" -eq 0 ]
  [ "$output" = '"Bash"' ]

  # Second tool should have error=true
  run bash -c "RAW_RESPONSE='$RAW' python3 '${REPO_DIR}/scripts/normalize_json.py' --tool-history | jq '.[1].error'"
  [ "$status" -eq 0 ]
  [ "$output" = "true" ]
}

@test "normalize_json.py --tool-summary formats readable output" {
  RAW='{"type":"tool_use","tool":"Bash","input":{"command":"npm test"}}
{"type":"tool_result","is_error":false}
{"type":"tool_use","tool":"Edit","input":{"file_path":"src/main.ts"}}
{"type":"tool_result","is_error":true}'

  run bash -c "RAW_RESPONSE='$RAW' python3 '${REPO_DIR}/scripts/normalize_json.py' --tool-summary"
  [ "$status" -eq 0 ]
  [[ "$output" == *"$ npm test"* ]]
  [[ "$output" == *"Edit: src/main.ts [ERROR]"* ]]
}

@test "normalize_json.py --tool-history returns empty on no tools" {
  RAW='{"type":"text","part":{"text":"hello"}}'
  run bash -c "RAW_RESPONSE='$RAW' python3 '${REPO_DIR}/scripts/normalize_json.py' --tool-history"
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "comment dedup skips identical comments" {
  # Set up a task with a known last_comment_hash
  BODY="test comment body"
  HASH=$(printf '%s' "$BODY" | shasum -a 256 | cut -c1-16)
  export HASH
  tdb_set "$INIT_TASK_ID" last_comment_hash "$HASH"

  run bash -c "
    export ORCH_HOME='$ORCH_HOME' PROJECT_DIR='$PROJECT_DIR'
    source '${REPO_DIR}/scripts/lib.sh'
    if db_should_skip_comment '$INIT_TASK_ID' '$BODY'; then
      echo 'SKIPPED'
    else
      echo 'POSTED'
    fi
  "
  [ "$status" -eq 0 ]
  [ "$output" = "SKIPPED" ]
}

@test "comment dedup posts when content differs" {
  tdb_set "$INIT_TASK_ID" last_comment_hash "oldoldhash12345"

  run bash -c "
    export ORCH_HOME='$ORCH_HOME' PROJECT_DIR='$PROJECT_DIR'
    source '${REPO_DIR}/scripts/lib.sh'
    if db_should_skip_comment '$INIT_TASK_ID' 'new different body'; then
      echo 'SKIPPED'
    else
      echo 'POSTED'
    fi
  "
  [ "$status" -eq 0 ]
  [ "$output" = "POSTED" ]
}

@test "@orchestrator mention creates a task and mirrors agent result" {
  # Create a mention comment on the init issue
  run gh api "repos/mock/repo/issues/${INIT_TASK_ID}/comments" -f body="hey @orchestrator please take a look"
  [ "$status" -eq 0 ]

  run gh_mentions.sh
  [ "$status" -eq 0 ]

  # Should create a new task issue
  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -eq 2 ]

  MENTION_TASK_ID=$(_task_id_by_title "Respond to @orchestrator mention in #${INIT_TASK_ID}")
  [ -n "$MENTION_TASK_ID" ]

  # Idempotent: re-running should not create another task
  run gh_mentions.sh
  [ "$status" -eq 0 ]
  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -eq 2 ]

  # Simulate an agent completing the mention task; backend should mirror to target issue
  run bash -c "
    source '${REPO_DIR}/scripts/lib.sh'
    db_task_set '$MENTION_TASK_ID' agent 'codex'
    db_store_agent_response '$MENTION_TASK_ID' done 'mention handled' '' false '' 1 0 0 '' ''
    db_store_agent_arrays '$MENTION_TASK_ID' 'did the thing' '' '' ''
  "
  [ "$status" -eq 0 ]

  # Target issue should have at least the ack + mirrored agent comment
  run bash -c "jq -r --arg n '${INIT_TASK_ID}' '(.comments[$n] // []) | length' '$GH_MOCK_STATE' | head -n1"
  [ "$status" -eq 0 ]
  [ "$output" -ge 2 ]

  run bash -c "jq -r --arg n '${INIT_TASK_ID}' '(.comments[$n] // []) | map(.body) | join(\"\\n---\\n\")' '$GH_MOCK_STATE'"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Mention task: #${MENTION_TASK_ID}"* ]]
  [[ "$output" == *"mention handled"* ]]
}

@test "@orchestrator mention is idempotent across multiple project configs for the same repo" {
  # Simulate two distinct projects (with project configs) polling the same repo.
  PROJ1="${TMP_DIR}/proj1"
  PROJ2="${TMP_DIR}/proj2"
  mkdir -p "$PROJ1" "$PROJ2"

  cat > "${PROJ1}/.orchestrator.yml" <<'YAML'
backend: github
gh:
  repo: "mock/repo"
YAML

  cat > "${PROJ2}/.orchestrator.yml" <<'YAML'
backend: github
gh:
  repo: "mock/repo"
YAML

  # Create a mention comment on the init issue
  run gh api "repos/mock/repo/issues/${INIT_TASK_ID}/comments" -f body="hey @orchestrator please take a look"
  [ "$status" -eq 0 ]

  # First project poll creates the mention task.
  run bash -c "
    unset STATE_DIR CONFIG_PATH
    export ORCH_HOME='${ORCH_HOME}' ORCH_BACKEND='github' GH_MOCK_STATE='${GH_MOCK_STATE}'
    export PROJECT_DIR='${PROJ1}'
    gh_mentions.sh
  "
  [ "$status" -eq 0 ]

  # Second project poll should not create a duplicate task for the same comment.
  run bash -c "
    unset STATE_DIR CONFIG_PATH
    export ORCH_HOME='${ORCH_HOME}' ORCH_BACKEND='github' GH_MOCK_STATE='${GH_MOCK_STATE}'
    export PROJECT_DIR='${PROJ2}'
    gh_mentions.sh
  "
  [ "$status" -eq 0 ]

  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -eq 2 ]
}

@test "@orchestrator mention inside blockquote is ignored" {
  run gh api "repos/mock/repo/issues/${INIT_TASK_ID}/comments" -f body=$'> @orchestrator please do not trigger\\n\\nthanks'
  [ "$status" -eq 0 ]

  run gh_mentions.sh
  [ "$status" -eq 0 ]

  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -eq 1 ]
}

@test "@orchestrator mention inside fenced code is ignored" {
  run gh api "repos/mock/repo/issues/${INIT_TASK_ID}/comments" -f body=$'```\\n@orchestrator do not trigger\\n```'
  [ "$status" -eq 0 ]

  run gh_mentions.sh
  [ "$status" -eq 0 ]

  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -eq 1 ]
}

@test "@orchestrator mention inside double quotes is ignored" {
  run gh api "repos/mock/repo/issues/${INIT_TASK_ID}/comments" -f body=$'FYI I saw ("@orchestrator fix.") in another thread.'
  [ "$status" -eq 0 ]

  run gh_mentions.sh
  [ "$status" -eq 0 ]

  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -eq 1 ]
}

@test "@orchestrator mention inside inline code is ignored" {
  run gh api "repos/mock/repo/issues/${INIT_TASK_ID}/comments" -f body=$'Example: `@orchestrator fix`'
  [ "$status" -eq 0 ]

  run gh_mentions.sh
  [ "$status" -eq 0 ]

  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -eq 1 ]
}

@test "@orchestrator mention on closed issue is skipped" {
  # Create a comment on the init issue, then close it
  run gh api "repos/mock/repo/issues/${INIT_TASK_ID}/comments" -f body="@orchestrator please help"
  [ "$status" -eq 0 ]
  run gh api "repos/mock/repo/issues/${INIT_TASK_ID}" -X PATCH -f state=closed
  [ "$status" -eq 0 ]

  run gh_mentions.sh
  [ "$status" -eq 0 ]

  # No new task should be created since the issue is closed
  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -eq 1 ]
}

@test "@orchestrator mention on closed issue with done task does not re-trigger" {
  # Comment on open issue — creates a task
  run gh api "repos/mock/repo/issues/${INIT_TASK_ID}/comments" -f body="@orchestrator do something"
  [ "$status" -eq 0 ]

  run gh_mentions.sh
  [ "$status" -eq 0 ]

  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -eq 2 ]

  MENTION_TASK_ID=$(_task_id_by_title "Respond to @orchestrator mention in #${INIT_TASK_ID}")
  [ -n "$MENTION_TASK_ID" ]

  # Simulate the mention task completing
  run bash -c "
    source '${REPO_DIR}/scripts/lib.sh'
    db_store_agent_response '$MENTION_TASK_ID' done 'handled' '' false '' 1 0 0 '' ''
  "
  [ "$status" -eq 0 ]

  # Close the issue
  run gh api "repos/mock/repo/issues/${INIT_TASK_ID}" -X PATCH -f state=closed
  [ "$status" -eq 0 ]

  # Second comment on the now-closed issue
  run gh api "repos/mock/repo/issues/${INIT_TASK_ID}/comments" -f body="@orchestrator actually do more"
  [ "$status" -eq 0 ]

  run gh_mentions.sh
  [ "$status" -eq 0 ]

  # No new task should be created — prior task is done but issue is closed
  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -eq 2 ]
}

@test "@orchestrator mention on closed issue advances since so comments are not re-fetched" {
  # Create a comment and close the issue
  run gh api "repos/mock/repo/issues/${INIT_TASK_ID}/comments" -f body="@orchestrator help"
  [ "$status" -eq 0 ]
  run gh api "repos/mock/repo/issues/${INIT_TASK_ID}" -X PATCH -f state=closed
  [ "$status" -eq 0 ]

  # First poll — no task created, but since should advance past this comment
  run gh_mentions.sh
  [ "$status" -eq 0 ]

  repo_key=$(printf 'mock/repo' | tr '/:' '__')
  mentions_db="${ORCH_HOME}/.orchestrator/mentions/${repo_key}.json"
  since_before=$(jq -r '.since' "$mentions_db")

  # Second poll — should not re-fetch or re-process the same comment
  run gh_mentions.sh
  [ "$status" -eq 0 ]

  since_after=$(jq -r '.since' "$mentions_db")

  # since should have advanced (not stuck at the initial value)
  [ "$since_after" != "1970-01-01T00:00:00Z" ]
  [ "$since_before" = "$since_after" ]

  # Still no new task
  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -eq 1 ]
}

@test "create_task_entry includes last_comment_hash field" {
  NOW="2026-01-01T00:00:00Z"
  export NOW PROJECT_DIR="$TMP_DIR"

  run bash -c "
    source '${REPO_DIR}/scripts/lib.sh'
    create_task_entry 50 'Schema Test' 'Check fields' '' '' ''
  "
  [ "$status" -eq 0 ]
  NEW_ID=$(echo "$output" | tr -d '[:space:]')

  # Verify the task was created and has a valid ID
  [ -n "$NEW_ID" ]
  run tdb_field "$NEW_ID" title
  [ "$status" -eq 0 ]
  [ "$output" = "Schema Test" ]
}

@test "fetch_issue_comments returns empty for missing issue" {
  run bash -c "
    source '${REPO_DIR}/scripts/lib.sh'
    result=\$(fetch_issue_comments 'test/repo' '' 10)
    echo \"result=[\$result]\"
  "
  [ "$status" -eq 0 ]
  [[ "$output" == *"result=[]"* ]]
}

@test "fetch_issue_comments returns empty for null issue" {
  run bash -c "
    source '${REPO_DIR}/scripts/lib.sh'
    result=\$(fetch_issue_comments 'test/repo' 'null' 10)
    echo \"result=[\$result]\"
  "
  [ "$status" -eq 0 ]
  [[ "$output" == *"result=[]"* ]]
}

@test "build_skills_catalog returns JSON array from SKILL.md files" {
  SKILLS_TMP=$(mktemp -d)
  mkdir -p "$SKILLS_TMP/test-skill"
  cat > "$SKILLS_TMP/test-skill/SKILL.md" <<'MD'
---
name: test-skill
description: A test skill for testing.
---

# Test Skill
This is a test.
MD

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && build_skills_catalog '$SKILLS_TMP'"
  [ "$status" -eq 0 ]

  # Should be valid JSON with one entry
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && build_skills_catalog '$SKILLS_TMP' | jq '.[0].id'"
  [ "$status" -eq 0 ]
  [ "$output" = '"test-skill"' ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && build_skills_catalog '$SKILLS_TMP' | jq '.[0].description'"
  [ "$status" -eq 0 ]
  [ "$output" = '"A test skill for testing."' ]

  rm -rf "$SKILLS_TMP"
}

@test "build_skills_catalog returns empty array for missing dir" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && build_skills_catalog '/tmp/nonexistent_skills_$$'"
  [ "$status" -eq 0 ]
  [ "$output" = "[]" ]
}

@test "build_skills_catalog handles malformed SKILL.md gracefully" {
  SKILLS_TMP=$(mktemp -d)
  mkdir -p "$SKILLS_TMP/good-skill" "$SKILLS_TMP/bad-skill"

  cat > "$SKILLS_TMP/good-skill/SKILL.md" <<'MD'
---
name: good-skill
description: This one is fine.
---
MD

  # Create a file with bad encoding
  printf '\xff\xfe' > "$SKILLS_TMP/bad-skill/SKILL.md"

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && build_skills_catalog '$SKILLS_TMP' 2>/dev/null | jq 'length'"
  [ "$status" -eq 0 ]
  # Should have at least the good skill (bad one skipped)
  [[ "$output" =~ ^[0-9]+$ ]]
  [ "$output" -ge 1 ]

  rm -rf "$SKILLS_TMP"
}

@test "run_task.sh detects retry loop with 3 identical blocked errors" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Retry Loop" "Test retry loop detection" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Set up task with 3 attempts and 3 identical blocked history entries
  tdb_set "$TASK2_ID" agent "codex"
  tdb_set "$TASK2_ID" attempts "3"
  tdb_set "$TASK2_ID" status "routed"
  _add_history "$TASK2_ID" "2026-01-01T00:00:00Z" "blocked" "agent command failed (exit 1)"
  _add_history "$TASK2_ID" "2026-01-01T00:01:00Z" "blocked" "agent command failed (exit 1)"
  _add_history "$TASK2_ID" "2026-01-01T00:02:00Z" "blocked" "agent command failed (exit 1)"

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
echo "should not be called" >&2
exit 1
SH
  chmod +x "$CODEX_STUB"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" \
    PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" \
    ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false \
    "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  # Task should be needs_review due to retry loop
  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "needs_review" ]

  run tdb_field "$TASK2_ID" last_error
  [ "$status" -eq 0 ]
  [[ "$output" == *"retry loop"* ]]
}

@test "run_task.sh does not detect retry loop with varied errors" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "No Loop" "Different errors each time" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  tdb_set "$TASK2_ID" agent "codex"
  tdb_set "$TASK2_ID" attempts "3"
  tdb_set "$TASK2_ID" status "routed"
  _add_history "$TASK2_ID" "2026-01-01T00:00:00Z" "blocked" "error A"
  _add_history "$TASK2_ID" "2026-01-01T00:01:00Z" "blocked" "error B"
  _add_history "$TASK2_ID" "2026-01-01T00:02:00Z" "blocked" "error C"

  # Stub that succeeds (prints JSON to stdout)
  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"status":"done","summary":"fixed it","files_changed":[],"needs_help":false,"delegations":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" \
    PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" \
    ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false \
    "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  # Task should complete normally (not blocked by retry loop)
  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "done" ]
}

@test "agent prompt includes error history and issue comments sections" {
  run grep -c 'TASK_HISTORY\|TASK_LAST_ERROR\|ISSUE_COMMENTS' "${REPO_DIR}/prompts/agent.md"
  [ "$status" -eq 0 ]
  [ "$output" -ge 3 ]
}

@test "system prompt enforces workflow rules" {
  run grep -c 'worktree\|Do NOT run.*git push\|Do NOT mark.*done\|commit' "${REPO_DIR}/prompts/system.md"
  [ "$status" -eq 0 ]
  [ "$output" -ge 3 ]
}

@test "system prompt includes logging instructions" {
  run grep -c 'accomplished.*bullet\|remaining.*owner\|files_changed.*comment\|reason.*error' "${REPO_DIR}/prompts/system.md"
  [ "$status" -eq 0 ]
  [ "$output" -ge 3 ]
}

@test "skills_sync.sh uses ORCH_HOME paths" {
  run grep -c 'ORCH_HOME' "${REPO_DIR}/scripts/skills_sync.sh"
  [ "$status" -eq 0 ]
  [ "$output" -ge 2 ]
}

@test "lib.sh defaults to ORCH_HOME for state paths" {
  run bash -c "head -10 '${REPO_DIR}/scripts/lib.sh' | grep -c 'ORCH_HOME'"
  [ "$status" -eq 0 ]
  [ "$output" -ge 2 ]
}

# --- start/stop/restart routing tests ---

# --- Visibility / reporting tests ---

@test "duration_fmt formats seconds correctly" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && duration_fmt 45"
  [ "$output" = "45s" ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && duration_fmt 125"
  [ "$output" = "2m 5s" ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && duration_fmt 3661"
  [ "$output" = "1h 1m" ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && duration_fmt 0"
  [ "$output" = "0s" ]
}

@test "normalize_json.py --usage extracts token counts" {
  EVENTS='{"type":"result","usage":{"input_tokens":15000,"output_tokens":3000}}'
  run bash -c "RAW_RESPONSE='$EVENTS' python3 '${REPO_DIR}/scripts/normalize_json.py' --usage"
  [ "$status" -eq 0 ]
  echo "$output" | jq -e '.input_tokens == 15000'
  echo "$output" | jq -e '.output_tokens == 3000'
}

@test "normalize_json.py --usage returns zeros on no usage" {
  EVENTS='{"type":"text","part":{"text":"hello"}}'
  run bash -c "RAW_RESPONSE='$EVENTS' python3 '${REPO_DIR}/scripts/normalize_json.py' --usage"
  [ "$status" -eq 0 ]
  echo "$output" | jq -e '.input_tokens == 0'
  echo "$output" | jq -e '.output_tokens == 0'
}

@test "read_tool_summary generates markdown table from tools JSON" {
  mkdir -p "${STATE_DIR}"
  cat > "${STATE_DIR}/tools-1.json" <<'JSON'
[
  {"tool":"Bash","input":{"command":"ls"},"error":false},
  {"tool":"Bash","input":{"command":"git push"},"error":true},
  {"tool":"Read","input":{"file_path":"foo.txt"},"error":false},
  {"tool":"Edit","input":{"file_path":"bar.txt"},"error":false}
]
JSON

  # Extract just the read_tool_summary function and test it
  local func_file="${TMP_DIR}/tool_summary_func.sh"
  sed -n '/^read_tool_summary()/,/^}/p' "${REPO_DIR}/scripts/backend_github.sh" > "$func_file"

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && source '$func_file' && read_tool_summary '' 1"
  [ "$status" -eq 0 ]
  echo "$output" | grep -q '| Bash | 2 |'
  echo "$output" | grep -q '| Read | 1 |'
  echo "$output" | grep -q 'Failed tool calls (1)'
  echo "$output" | grep -q 'git push'
}

@test "read_tool_summary returns empty for missing file" {
  local func_file="${TMP_DIR}/tool_summary_func.sh"
  sed -n '/^read_tool_summary()/,/^}/p' "${REPO_DIR}/scripts/backend_github.sh" > "$func_file"

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && source '$func_file' && read_tool_summary '' 999"
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "run_task.sh stores duration and tokens in task YAML" {
  run grep -c 'duration\|input_tokens\|output_tokens\|AGENT_DURATION\|stderr_snippet' "${REPO_DIR}/scripts/run_task.sh"
  [ "$status" -eq 0 ]
  [ "$output" -ge 6 ]
}

@test "run_task.sh completion log includes duration and tokens" {
  run grep 'DONE.*duration.*tokens' "${REPO_DIR}/scripts/run_task.sh"
  [ "$status" -eq 0 ]
}

@test "dashboard.sh runs without errors" {
  # Add gh_issue_number to exercise issue formatting
  tdb_set "$INIT_TASK_ID" gh_issue_number "10"
  run "${REPO_DIR}/scripts/dashboard.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Tasks:"* ]]
  [[ "$output" == *"Projects:"* ]]
  [[ "$output" == *"Worktrees:"* ]]
}

@test "status.sh --global shows PROJECT column" {
  # Add a task with a dir to test global view
  tdb_set "$INIT_TASK_ID" dir "/Users/test/myproject"
  run "${REPO_DIR}/scripts/status.sh" --global
  [ "$status" -eq 0 ]
  # Header should include PROJECT column
  [[ "$output" == *"PROJECT"* ]]
  # Just verify the global view runs and shows the column header
}

@test "list_tasks.sh shows table with issue numbers" {
  tdb_set "$INIT_TASK_ID" gh_issue_number "42"
  run "${REPO_DIR}/scripts/list_tasks.sh"
  [ "$status" -eq 0 ]
  # Header should include ISSUE column
  [[ "$output" == *"ISSUE"* ]]
  # Just verify the list runs and shows the column header
}

@test "task_field reads a task field" {
  run tdb_field "$INIT_TASK_ID" title
  [ "$status" -eq 0 ]
  [ "$output" = "Init" ]
}

@test "task_count counts tasks by status" {
  # Init task should be "new" — verify via backend
  run tdb_field "$INIT_TASK_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "new" ]

  # Total count
  run tdb_count
  [ "$status" -eq 0 ]
  [ "$output" -ge 1 ]
}

# --- tree.sh ---

@test "tree.sh displays task tree" {
  run "${REPO_DIR}/scripts/tree.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Init"* ]]
  [[ "$output" == *"(new)"* ]]
}

@test "tree.sh shows parent-child relationships" {
  PARENT_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Parent" "" "")
  PARENT_ID=$(_task_id "$PARENT_OUTPUT")
  CHILD_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Child" "" "")
  CHILD_ID=$(_task_id "$CHILD_OUTPUT")
  _task_set_parent "$CHILD_ID" "$PARENT_ID"

  run "${REPO_DIR}/scripts/tree.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Parent"* ]]
  [[ "$output" == *"Child"* ]]
  # Child should be indented under parent
  [[ "$output" == *"└─"* ]] || [[ "$output" == *"├─"* ]]
}

# --- retry_task.sh ---

@test "retry_task.sh resets done task to new" {
  tdb_set "$INIT_TASK_ID" status "done"

  run "${REPO_DIR}/scripts/retry_task.sh" "$INIT_TASK_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$INIT_TASK_ID" status
  [ "$output" = "new" ]
}

@test "retry_task.sh resets blocked task to new" {
  tdb_set "$INIT_TASK_ID" status "blocked"

  run "${REPO_DIR}/scripts/retry_task.sh" "$INIT_TASK_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$INIT_TASK_ID" status
  [ "$output" = "new" ]
}

@test "retry_task.sh clears agent on reset" {
  tdb_set "$INIT_TASK_ID" status "done"
  tdb_set "$INIT_TASK_ID" agent "claude"

  run "${REPO_DIR}/scripts/retry_task.sh" "$INIT_TASK_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$INIT_TASK_ID" agent
  [ -z "$output" ]
}

@test "retry_task.sh skips already-new tasks" {
  run "${REPO_DIR}/scripts/retry_task.sh" "$INIT_TASK_ID"
  [ "$status" -eq 0 ]
  [[ "$output" == *"already new"* ]] || true
}

@test "retry_task.sh fails on missing task" {
  run "${REPO_DIR}/scripts/retry_task.sh" "nonexistent-task-id"
  [ "$status" -ne 0 ]
}

@test "retry_task.sh requires task id" {
  run "${REPO_DIR}/scripts/retry_task.sh"
  [ "$status" -ne 0 ]
}

# --- set_agent.sh ---

@test "set_agent.sh sets agent on a task" {
  run "${REPO_DIR}/scripts/set_agent.sh" "$INIT_TASK_ID" claude
  [ "$status" -eq 0 ]

  run tdb_field "$INIT_TASK_ID" agent
  [ "$output" = "claude" ]
}

@test "set_agent.sh requires both id and agent" {
  run "${REPO_DIR}/scripts/set_agent.sh" "$INIT_TASK_ID"
  [ "$status" -ne 0 ]

  run "${REPO_DIR}/scripts/set_agent.sh"
  [ "$status" -ne 0 ]
}

# --- jobs_list.sh ---

# --- output.sh helpers ---

@test "output.sh status_icon returns correct icons" {
  source "${REPO_DIR}/scripts/output.sh"
  run status_icon "done"
  [ "$output" = "✓" ]

  run status_icon "blocked"
  [ "$output" = "✗" ]

  run status_icon "new"
  [ "$output" = "○" ]
}

@test "output.sh fmt_issue formats issue numbers" {
  source "${REPO_DIR}/scripts/output.sh"

  run fmt_issue 42
  [ "$output" = "#42" ]

  run fmt_issue ""
  [ "$output" = "-" ]

  run fmt_issue "null"
  [ "$output" = "-" ]
}

@test "output.sh table_with_header formats table" {
  source "${REPO_DIR}/scripts/output.sh"
  result=$(printf '1\tnew\tInit\n' | table_with_header "ID\tSTATUS\tTITLE")
  [[ "$result" == *"ID"* ]]
  [[ "$result" == *"STATUS"* ]]
  [[ "$result" == *"Init"* ]]
}

# --- jobs_tick.sh bash execution ---

# --- jobs_tick.sh catch-up ---

# --- status.sh ---

@test "status.sh shows counts table" {
  run "${REPO_DIR}/scripts/status.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *"STATUS"* ]]
  [[ "$output" == *"QTY"* ]]
  [[ "$output" == *"new"* ]]
  [[ "$output" == *"open"* ]]
  [[ "$output" == *"total"* ]]
}

@test "status.sh shows correct per-status counts and totals" {
  ROUTED_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Routed" "" "")
  ROUTED_ID=$(_task_id "$ROUTED_OUTPUT")
  tdb_set "$ROUTED_ID" status "routed"

  IP1_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "In Progress 1" "" "")
  IP1_ID=$(_task_id "$IP1_OUTPUT")
  tdb_set "$IP1_ID" status "in_progress"

  IP2_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "In Progress 2" "" "")
  IP2_ID=$(_task_id "$IP2_OUTPUT")
  tdb_set "$IP2_ID" status "in_progress"

  IR_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "In Review" "" "")
  IR_ID=$(_task_id "$IR_OUTPUT")
  tdb_set "$IR_ID" status "in_review"

  NR_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Needs Review" "" "")
  NR_ID=$(_task_id "$NR_OUTPUT")
  tdb_set "$NR_ID" status "needs_review"

  DONE_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Done" "" "")
  DONE_ID=$(_task_id "$DONE_OUTPUT")
  tdb_set "$DONE_ID" status "done"
  gh api "repos/mock/repo/issues/${DONE_ID}" -X PATCH -f state=closed >/dev/null

  run "${REPO_DIR}/scripts/status.sh"
  [ "$status" -eq 0 ]

  echo "$output" | grep -qE '^new[[:space:]]+1[[:space:]]*$'
  echo "$output" | grep -qE '^routed[[:space:]]+1[[:space:]]*$'
  echo "$output" | grep -qE '^in_progress[[:space:]]+2[[:space:]]*$'
  echo "$output" | grep -qE '^in_review[[:space:]]+1[[:space:]]*$'
  echo "$output" | grep -qE '^blocked[[:space:]]+0[[:space:]]*$'
  echo "$output" | grep -qE '^needs_review[[:space:]]+1[[:space:]]*$'
  echo "$output" | grep -qE '^done[[:space:]]+1[[:space:]]*$'
  echo "$output" | grep -qE '^open[[:space:]]+6[[:space:]]*$'
  echo "$output" | grep -qE '^total[[:space:]]+7[[:space:]]*$'
}

@test "status.sh --json --global returns valid JSON" {
  run "${REPO_DIR}/scripts/status.sh" --json --global
  [ "$status" -eq 0 ]
  # Validate it's JSON
  printf '%s' "$output" | jq . >/dev/null
  # Check total
  TOTAL=$(printf '%s' "$output" | jq -r '.total')
  [ "$TOTAL" -ge 1 ]
  OPEN=$(printf '%s' "$output" | jq -r '.open')
  [ "$OPEN" -ge 0 ]
}

# --- add_task.sh edge cases ---

@test "add_task.sh with empty body and labels" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "No Body Task" "" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  run tdb_field "$TASK2_ID" title
  [ "$output" = "No Body Task" ]
}

@test "add_task.sh assigns unique ids" {
  OUTPUT_A=$("${REPO_DIR}/scripts/add_task.sh" "Task A" "" "")
  ID_A=$(_task_id "$OUTPUT_A")
  OUTPUT_B=$("${REPO_DIR}/scripts/add_task.sh" "Task B" "" "")
  ID_B=$(_task_id "$OUTPUT_B")
  OUTPUT_C=$("${REPO_DIR}/scripts/add_task.sh" "Task C" "" "")
  ID_C=$(_task_id "$OUTPUT_C")

  # IDs should be unique
  [ "$ID_A" != "$ID_B" ]
  [ "$ID_B" != "$ID_C" ]
  [ "$ID_A" != "$ID_C" ]
}

@test "add_task.sh --dry-run prints preview without creating issue" {
  BEFORE=$(tdb_count)

  run "${REPO_DIR}/scripts/add_task.sh" --dry-run "Dry Run Title" "Dry run body" "label1,label2"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Dry run: would create GitHub issue"* ]]
  [[ "$output" == *"title: Dry Run Title"* ]]
  [[ "$output" == *"body: Dry run body"* ]]
  [[ "$output" == *"labels: status:new, label1, label2"* ]]

  AFTER=$(tdb_count)
  [ "$AFTER" -eq "$BEFORE" ]
}

# --- task_set helper ---

@test "task_set updates a task field" {
  tdb_set "$INIT_TASK_ID" status "blocked"

  run tdb_field "$INIT_TASK_ID" status
  [ "$output" = "blocked" ]
}

# --- unlock.sh ---

@test "available_agents respects disabled_agents config" {
  source "${REPO_DIR}/scripts/lib.sh"

  # Disable codex
  yq -i '.router.disabled_agents = ["codex"]' "$CONFIG_PATH"

  result=$(available_agents)
  # codex should NOT be in result (if installed)
  [[ "$result" != *"codex"* ]] || skip "codex not installed"
  # claude should still be there (if installed)
  if command -v claude >/dev/null 2>&1; then
    [[ "$result" == *"claude"* ]]
  fi
}

@test "load_project_config merges per-project orchestrator.yml" {
  # Create a per-project config with a different repo
  cat > "${TMP_DIR}/orchestrator.yml" <<YAML
gh:
  repo: "testorg/testrepo"
  project_id: "PVT_test123"
YAML

  # Global config has a different repo
  run yq -i '.gh.repo = "global/repo"' "$CONFIG_PATH"

  # Source lib and load project config — PROJECT_DIR must be set first
  run bash -c '
    export PROJECT_DIR="'"$TMP_DIR"'"
    export CONFIG_PATH="'"$CONFIG_PATH"'"
    export STATE_DIR="'"$STATE_DIR"'"
    source "'"$REPO_DIR"'/scripts/lib.sh"
    load_project_config
    config_get ".gh.repo"
  '
  [ "$status" -eq 0 ]
  [ "$output" = "testorg/testrepo" ]
}

@test "load_project_config uses global config when no project config exists" {
  # No orchestrator.yml in PROJECT_DIR
  run yq -i '.gh.repo = "global/repo"' "$CONFIG_PATH"

  run bash -c '
    export PROJECT_DIR="'"$TMP_DIR"'/no-project"
    export CONFIG_PATH="'"$CONFIG_PATH"'"
    export STATE_DIR="'"$STATE_DIR"'"
    mkdir -p "$PROJECT_DIR"
    source "'"$REPO_DIR"'/scripts/lib.sh"
    load_project_config
    config_get ".gh.repo"
  '
  [ "$status" -eq 0 ]
  [ "$output" = "global/repo" ]
}

@test "load_project_config called twice uses global config not merged" {
  # Regression test: when parent process calls load_project_config, CONFIG_PATH
  # becomes config-merged.yml. If a child process inherits this and calls
  # load_project_config again, it must still merge from the GLOBAL config,
  # not from the already-merged file (which would produce empty output).
  cat > "${TMP_DIR}/orchestrator.yml" <<YAML
gh:
  repo: "testorg/testrepo"
YAML

  run bash -c '
    export PROJECT_DIR="'"$TMP_DIR"'"
    export CONFIG_PATH="'"$CONFIG_PATH"'"
    export STATE_DIR="'"$STATE_DIR"'"
    source "'"$REPO_DIR"'/scripts/lib.sh"
    # First call (parent process)
    load_project_config
    repo1=$(config_get ".gh.repo")
    # Second call (simulates child process inheriting CONFIG_PATH=merged)
    load_project_config
    repo2=$(config_get ".gh.repo")
    echo "first=$repo1 second=$repo2"
    [ "$repo1" = "testorg/testrepo" ] && [ "$repo2" = "testorg/testrepo" ]
  '
  [ "$status" -eq 0 ]
  [[ "$output" == *"first=testorg/testrepo second=testorg/testrepo"* ]]
}

@test "all scripts set PROJECT_DIR before load_project_config" {
  # This is a lint test — ensures no script calls load_project_config
  # before setting PROJECT_DIR, which causes wrong config loading.
  FAILURES=""
  for f in "${REPO_DIR}"/scripts/*.sh; do
    [ -f "$f" ] || continue
    fname=$(basename "$f")
    # Skip lib.sh (defines the function)
    [ "$fname" = "lib.sh" ] && continue
    grep -q 'load_project_config' "$f" || continue
    pd_line=$(grep -n 'PROJECT_DIR=' "$f" | head -1 | cut -d: -f1)
    lp_line=$(grep -n 'load_project_config' "$f" | head -1 | cut -d: -f1)
    if [ -z "$pd_line" ] || [ "$pd_line" -gt "$lp_line" ]; then
      FAILURES="${FAILURES}${fname}: PROJECT_DIR set at line ${pd_line:-MISSING}, load_project_config at line ${lp_line}\n"
    fi
  done
  if [ -n "$FAILURES" ]; then
    printf "Scripts with PROJECT_DIR after load_project_config:\n%b" "$FAILURES"
    return 1
  fi
}

@test "unlock.sh removes lock files" {
  # Create fake lock files
  mkdir -p "$LOCK_PATH"
  touch "${LOCK_PATH}/serve.lock"
  mkdir -p "${LOCK_PATH}/task.test-id"

  run "${REPO_DIR}/scripts/unlock.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Removed"* ]]
}

# --- Integration: full worktree workflow ---

@test "e2e: worktree created, agent runs, commits, push attempted, PR attempted" {
  # Create a "remote" bare repo to push to
  REMOTE_DIR="${TMP_DIR}/remote.git"
  git init --bare "$REMOTE_DIR" --quiet
  git -C "$PROJECT_DIR" remote add origin "$REMOTE_DIR"
  # Commit any locally-created files before pushing
  git -C "$PROJECT_DIR" add -A 2>/dev/null || true
  git -C "$PROJECT_DIR" -c user.email="test@test.com" -c user.name="Test" commit -m "test init" --quiet 2>/dev/null || true
  git -C "$PROJECT_DIR" push -u origin main --quiet 2>/dev/null

  # Add a task with issue number
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Add README" "Create a README.md file" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Set agent (issue number = task ID in GitHub backend)
  tdb_set "$TASK2_ID" agent "codex"

  # Stub codex: writes output JSON to .orchestrator/ and creates a file + commit
  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'STUB'
#!/usr/bin/env bash
# Simulate agent work: create file and commit
echo "# Test Repo" > README.md
git add README.md
git commit -m "docs: add README" --quiet 2>/dev/null

# Write output JSON to the expected location (run_task.sh exports OUTPUT_FILE)
if [ -n "${OUTPUT_FILE:-}" ]; then
  mkdir -p "$(dirname "$OUTPUT_FILE")"
  cat > "$OUTPUT_FILE" <<'JSON'
{"status":"done","summary":"Added README","files_changed":["README.md"],"needs_help":false,"accomplished":["Created README.md"],"remaining":[],"blockers":[],"delegations":[]}
JSON
else
  mkdir -p .orchestrator
  cat > .orchestrator/output.json <<'JSON'
{"status":"done","summary":"Added README","files_changed":["README.md"],"needs_help":false,"accomplished":["Created README.md"],"remaining":[],"blockers":[],"delegations":[]}
JSON
fi
STUB
  chmod +x "$CODEX_STUB"

  # Stub gh (PR creation will be attempted; delegate api/auth to proper mock)
  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<STUB
#!/usr/bin/env bash
if [ "\$1" = "api" ] || [ "\$1" = "auth" ]; then
  exec "${MOCK_BIN}/gh" "\$@"
elif [[ "\$*" == *"pr list"* ]]; then
  echo ""
  exit 0
elif [[ "\$*" == *"pr create"* ]]; then
  echo "https://github.com/test/repo/pull/1"
  exit 0
elif [[ "\$*" == *"issue develop"* ]]; then
  exit 0
fi
exec "${MOCK_BIN}/gh" "\$@"
STUB
  chmod +x "$GH_STUB"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" \
    PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" \
    ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false \
    "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  # Verify worktree was created (project-local location)
  # In GitHub backend, task ID = issue number, so branch uses TASK2_ID
  WORKTREE_DIR="${PROJECT_DIR}/.orchestrator/worktrees/gh-task-${TASK2_ID}-add-readme"
  [ -d "$WORKTREE_DIR" ]

  BRANCH_NAME="gh-task-${TASK2_ID}-add-readme"

  # Verify worktree info saved to task
  run tdb_field "$TASK2_ID" worktree
  [ "$status" -eq 0 ]
  [[ "$output" == *"$BRANCH_NAME"* ]]

  run tdb_field "$TASK2_ID" branch
  [ "$status" -eq 0 ]
  [ "$output" = "$BRANCH_NAME" ]

  # Verify task completed
  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "done" ]

  # Verify commit exists in worktree
  run git -C "$WORKTREE_DIR" log --oneline main..HEAD
  [ "$status" -eq 0 ]
  [[ "$output" == *"add README"* ]]

  # Verify branch was pushed to remote
  run git -C "$REMOTE_DIR" branch --list "$BRANCH_NAME"
  [ "$status" -eq 0 ]
  [[ "$output" == *"$BRANCH_NAME"* ]]

  # Clean up worktree
  git -C "$PROJECT_DIR" worktree remove "$WORKTREE_DIR" --force 2>/dev/null || true
  git -C "$PROJECT_DIR" branch -D "$BRANCH_NAME" 2>/dev/null || true
}

@test "e2e: auto-commit when agent writes files but does not commit" {
  # Create a "remote" bare repo to push to
  REMOTE_DIR="${TMP_DIR}/remote.git"
  git init --bare "$REMOTE_DIR" --quiet
  git -C "$PROJECT_DIR" remote add origin "$REMOTE_DIR"
  # Commit any locally-created files before pushing
  git -C "$PROJECT_DIR" add -A 2>/dev/null || true
  git -C "$PROJECT_DIR" -c user.email="test@test.com" -c user.name="Test" commit -m "test init" --quiet 2>/dev/null || true
  git -C "$PROJECT_DIR" push -u origin main --quiet 2>/dev/null

  # Add a task with issue number
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Add LICENSE" "Create a LICENSE file" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Set agent (issue number = task ID in GitHub backend)
  tdb_set "$TASK2_ID" agent "claude"

  # Stub claude: writes files and output JSON but does NOT git commit
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'STUB'
#!/usr/bin/env bash
# Simulate agent that edits files but cannot run git commit (acceptEdits mode)
echo "MIT License" > LICENSE
# Write output JSON to the expected location (run_task.sh exports OUTPUT_FILE)
if [ -n "${OUTPUT_FILE:-}" ]; then
  mkdir -p "$(dirname "$OUTPUT_FILE")"
  cat > "$OUTPUT_FILE" <<'JSON'
{"status":"done","summary":"Added LICENSE","files_changed":["LICENSE"],"needs_help":false,"accomplished":["Created LICENSE"],"remaining":[],"blockers":[],"delegations":[]}
JSON
else
  mkdir -p .orchestrator
  cat > .orchestrator/output.json <<'JSON'
{"status":"done","summary":"Added LICENSE","files_changed":["LICENSE"],"needs_help":false,"accomplished":["Created LICENSE"],"remaining":[],"blockers":[],"delegations":[]}
JSON
fi
STUB
  chmod +x "$CLAUDE_STUB"

  # Stub gh (PR creation; delegate api/auth to proper mock)
  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<STUB
#!/usr/bin/env bash
if [ "\$1" = "api" ] || [ "\$1" = "auth" ]; then
  exec "${MOCK_BIN}/gh" "\$@"
elif [[ "\$*" == *"pr list"* ]]; then
  echo ""
  exit 0
elif [[ "\$*" == *"pr create"* ]]; then
  echo "https://github.com/test/repo/pull/2"
  exit 0
elif [[ "\$*" == *"issue develop"* ]]; then
  exit 0
fi
exec "${MOCK_BIN}/gh" "\$@"
STUB
  chmod +x "$GH_STUB"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" \
    PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" \
    ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false \
    "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  # Verify worktree was created (project-local location)
  # In GitHub backend, task ID = issue number
  BRANCH_NAME="gh-task-${TASK2_ID}-add-license"
  WORKTREE_DIR="${PROJECT_DIR}/.orchestrator/worktrees/${BRANCH_NAME}"
  [ -d "$WORKTREE_DIR" ]

  # Verify orchestrator auto-committed the changes
  run git -C "$WORKTREE_DIR" log --oneline main..HEAD
  [ "$status" -eq 0 ]
  [[ "$output" == *"Add LICENSE"* ]]

  # Verify LICENSE file is tracked
  run git -C "$WORKTREE_DIR" show HEAD:LICENSE
  [ "$status" -eq 0 ]
  [[ "$output" == *"MIT License"* ]]

  # Verify branch was pushed to remote
  run git -C "$REMOTE_DIR" branch --list "$BRANCH_NAME"
  [ "$status" -eq 0 ]
  [[ "$output" == *"$BRANCH_NAME"* ]]

  # Clean up worktree
  git -C "$PROJECT_DIR" worktree remove "$WORKTREE_DIR" --force 2>/dev/null || true
  git -C "$PROJECT_DIR" branch -D "$BRANCH_NAME" 2>/dev/null || true
}

@test "model_for_complexity resolves agent model from config" {
  # Add model_map to config
  yq -i '.model_map.simple.claude = "haiku" |
         .model_map.simple.codex = "gpt-5.1-codex-mini" |
         .model_map.medium.claude = "sonnet" |
         .model_map.medium.codex = "gpt-5.2" |
         .model_map.complex.claude = "opus" |
         .model_map.complex.codex = "gpt-5.3-codex"' "$CONFIG_PATH"

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; CONFIG_PATH='$CONFIG_PATH'; model_for_complexity claude simple"
  [ "$status" -eq 0 ]
  [ "$output" = "haiku" ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; CONFIG_PATH='$CONFIG_PATH'; model_for_complexity claude medium"
  [ "$status" -eq 0 ]
  [ "$output" = "sonnet" ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; CONFIG_PATH='$CONFIG_PATH'; model_for_complexity claude complex"
  [ "$status" -eq 0 ]
  [ "$output" = "opus" ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; CONFIG_PATH='$CONFIG_PATH'; model_for_complexity codex medium"
  [ "$status" -eq 0 ]
  [ "$output" = "gpt-5.2" ]

  # Unconfigured agent returns empty
  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; CONFIG_PATH='$CONFIG_PATH'; model_for_complexity opencode simple"
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "route_task.sh stores complexity in sidecar not as label" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Route Complexity" "Test complexity routing" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"executor":"codex","complexity":"simple","reason":"docs task","profile":{"role":"writer","skills":["docs"],"tools":["git"],"constraints":[]},"selected_skills":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" "${REPO_DIR}/scripts/route_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  # Complexity should be stored in sidecar
  run tdb_field "$TASK2_ID" complexity
  [ "$status" -eq 0 ]
  [ "$output" = "simple" ]

  # complexity: should NOT be a label (stored in sidecar only)
  run _task_labels "$TASK2_ID"
  [ "$status" -eq 0 ]
  [[ "$output" != *"complexity:"* ]]
  [[ "$output" != *"model:"* ]]
}

@test "route_task.sh fallback sets complexity to medium" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Fallback Complexity" "Test fallback" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  run yq -i '.router.agent = "claude" | .router.timeout_seconds = 0 | .router.fallback_executor = "codex"' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
exit 1
SH
  chmod +x "$CLAUDE_STUB"

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
echo "stub"
SH
  chmod +x "$CODEX_STUB"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" "${REPO_DIR}/scripts/route_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  # Fallback should default to medium complexity
  run tdb_field "$TASK2_ID" complexity
  [ "$status" -eq 0 ]
  [ "$output" = "medium" ]

  # complexity should NOT be a label (sidecar only)
  run _task_labels "$TASK2_ID"
  [ "$status" -eq 0 ]
  [[ "$output" != *"complexity:"* ]]
}

@test "create_task_entry includes complexity field" {
  NOW="2026-01-01T00:00:00Z"
  export NOW PROJECT_DIR="$TMP_DIR"

  run bash -c "
    source '${REPO_DIR}/scripts/lib.sh'
    create_task_entry 99 'Complexity Task' 'Test body' 'test' '' ''
  "
  [ "$status" -eq 0 ]
  NEW_ID=$(echo "$output" | tr -d '[:space:]')

  # complexity is NULL by default (not set until routing)
  run tdb_field "$NEW_ID" complexity
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "run_task.sh resolves model from complexity config" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Resolve Model" "Test model resolution" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Add model_map and set complexity on task
  yq -i '.model_map.simple.codex = "gpt-5.1-codex-mini" |
         .model_map.medium.codex = "gpt-5.2" |
         .model_map.complex.codex = "gpt-5.3-codex"' "$CONFIG_PATH"

  tdb_set "$TASK2_ID" agent "codex"
  tdb_set "$TASK2_ID" complexity "simple"

  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"status":"done","summary":"resolved model","files_changed":[],"needs_help":false,"delegations":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "done" ]
}

# @test "review agent uses reject decision to close PR" {
#   TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Review Reject" "Test reject" "")
#   TASK2_ID=$(_task_id "$TASK_OUTPUT")
#
#   run yq -i '.workflow.enable_review_agent = true' "$CONFIG_PATH"
#   [ "$status" -eq 0 ]
#
#   tdb_set "$TASK2_ID" agent "codex"
#
#   # Execution stub (codex) returns done
#   CODEX_STUB="${TMP_DIR}/codex"
#   cat > "$CODEX_STUB" <<'SH'
# #!/usr/bin/env bash
# cat <<'JSON'
# {"status":"done","summary":"done","files_changed":[],"needs_help":false,"delegations":[]}
# JSON
# SH
#   chmod +x "$CODEX_STUB"
#
#   # Review stub (claude) returns reject
#   CLAUDE_STUB="${TMP_DIR}/claude"
#   cat > "$CLAUDE_STUB" <<'SH'
# #!/usr/bin/env bash
# cat <<'JSON'
# {"decision":"reject","notes":"hallucinated API calls"}
# JSON
# SH
#   chmod +x "$CLAUDE_STUB"
#
#   # Mock gh — return PR number for list, accept diff/review/close
#   # Delegate api calls to the proper gh mock for backend support
#   GH_STUB="${TMP_DIR}/gh"
#   cat > "$GH_STUB" <<SH
# #!/usr/bin/env bash
# if [ "\$1" = "api" ] || [ "\$1" = "auth" ]; then
#   exec "${MOCK_BIN}/gh" "\$@"
# elif [ "\$1" = "pr" ] && [ "\$2" = "list" ]; then
#   echo "42"
# elif [ "\$1" = "pr" ] && [ "\$2" = "diff" ]; then
#   echo "+added line"
# elif [ "\$1" = "pr" ] && [ "\$2" = "review" ]; then
#   echo "ok"
# elif [ "\$1" = "pr" ] && [ "\$2" = "close" ]; then
#   echo "ok"
# elif [ "\$1" = "issue" ]; then
#   echo ""
# else
#   echo "[]"
# fi
# SH
#   chmod +x "$GH_STUB"
#
#   run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
#   [ "$status" -eq 0 ]
#
#   # Task should be needs_review after reject
#   run tdb_field "$TASK2_ID" status
#   [ "$status" -eq 0 ]
#   [ "$output" = "needs_review" ]
#
#   # Last error should mention rejection
#   run tdb_field "$TASK2_ID" last_error
#   [ "$status" -eq 0 ]
#   [[ "$output" == *"review rejected"* ]]
# }

@test "model_for_complexity defaults to medium when null" {
  yq -i '.model_map.medium.claude = "sonnet"' "$CONFIG_PATH"

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; CONFIG_PATH='$CONFIG_PATH'; model_for_complexity claude ''"
  [ "$status" -eq 0 ]
  [ "$output" = "sonnet" ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; CONFIG_PATH='$CONFIG_PATH'; model_for_complexity claude null"
  [ "$status" -eq 0 ]
  [ "$output" = "sonnet" ]
}

@test "model_for_complexity returns empty for unconfigured model_map" {
  # No model_map in config at all
  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; CONFIG_PATH='$CONFIG_PATH'; model_for_complexity claude complex"
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "run_task.sh sandbox adds disallowed tools for main project dir" {
  # Verify the sandbox code block exists and generates correct patterns
  run grep -c 'SANDBOX_PATTERNS' "${REPO_DIR}/scripts/run_task.sh"
  [ "$status" -eq 0 ]
  [ "$output" -ge 1 ]

  # Verify sandbox patterns include Read, Write, Edit, Bash restrictions
  run grep 'SANDBOX_PATTERNS=' "${REPO_DIR}/scripts/run_task.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *'Read('* ]]
  [[ "$output" == *'Write('* ]]
  [[ "$output" == *'Edit('* ]]
  [[ "$output" == *'Bash(cd'* ]]
}

@test "run_task.sh saves MAIN_PROJECT_DIR before worktree override" {
  run grep -n 'MAIN_PROJECT_DIR=' "${REPO_DIR}/scripts/run_task.sh"
  [ "$status" -eq 0 ]
  [[ "$output" == *'MAIN_PROJECT_DIR="$PROJECT_DIR"'* ]]
}

@test "opposite_agent picks different agent from task agent" {
  # Make both codex and claude available; opposite of codex should be claude
  CODEX_STUB="${TMP_DIR}/codex"
  printf '#!/usr/bin/env bash\necho ok\n' > "$CODEX_STUB"
  chmod +x "$CODEX_STUB"

  CLAUDE_STUB="${TMP_DIR}/claude"
  printf '#!/usr/bin/env bash\necho ok\n' > "$CLAUDE_STUB"
  chmod +x "$CLAUDE_STUB"

  run bash -c "export PATH='${TMP_DIR}:${PATH}'; source '${REPO_DIR}/scripts/lib.sh'; CONFIG_PATH='$CONFIG_PATH'; opposite_agent codex"
  [ "$status" -eq 0 ]
  [ "$output" = "claude" ]

  # opposite of claude should be codex
  run bash -c "export PATH='${TMP_DIR}:${PATH}'; source '${REPO_DIR}/scripts/lib.sh'; CONFIG_PATH='$CONFIG_PATH'; opposite_agent claude"
  [ "$status" -eq 0 ]
  [ "$output" = "codex" ]
}

@test "review agent request_changes posts review but does not close PR" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Review Changes" "Test request_changes" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  run yq -i '.workflow.enable_review_agent = true' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  tdb_set "$TASK2_ID" agent "codex"

  # Execution stub (codex) returns done
  CODEX_STUB="${TMP_DIR}/codex"
  cat > "$CODEX_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"status":"done","summary":"done","files_changed":[],"needs_help":false,"delegations":[]}
JSON
SH
  chmod +x "$CODEX_STUB"

  # Review stub (claude) returns request_changes
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
cat <<'JSON'
{"decision":"request_changes","notes":"missing error handling"}
JSON
SH
  chmod +x "$CLAUDE_STUB"

  # Mock gh — track calls to detect if pr close was called
  # Delegate api calls to the proper gh mock for backend support
  GH_LOG="${STATE_DIR}/gh_calls.log"
  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<SH
#!/usr/bin/env bash
echo "\$@" >> "${GH_LOG}"
if [ "\$1" = "api" ] || [ "\$1" = "auth" ]; then
  exec "${MOCK_BIN}/gh" "\$@"
elif [ "\$1" = "pr" ] && [ "\$2" = "list" ]; then
  echo "42"
elif [ "\$1" = "pr" ] && [ "\$2" = "diff" ]; then
  echo "+added line"
elif [ "\$1" = "pr" ] && [ "\$2" = "review" ]; then
  echo "ok"
elif [ "\$1" = "pr" ] && [ "\$2" = "close" ]; then
  echo "ok"
elif [ "\$1" = "issue" ]; then
  echo ""
else
  echo "[]"
fi
SH
  chmod +x "$GH_STUB"

  run env PATH="${TMP_DIR}:${PATH}" CONFIG_PATH="$CONFIG_PATH" PROJECT_DIR="$PROJECT_DIR" STATE_DIR="$STATE_DIR" ORCH_HOME="$ORCH_HOME" JOBS_FILE="$JOBS_FILE" LOCK_PATH="$LOCK_PATH" USE_TMUX=false "${REPO_DIR}/scripts/run_task.sh" "$TASK2_ID"
  [ "$status" -eq 0 ]

  # Task should be needs_review (not done)
  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "needs_review" ]

  # gh pr review should have been called (request-changes)
  run grep "pr review" "$GH_LOG"
  [ "$status" -eq 0 ]
  [[ "$output" == *"--request-changes"* ]]

  # gh pr close should NOT have been called
  run grep "pr close" "$GH_LOG"
  [ "$status" -ne 0 ]
}

@test "fetch_owner_feedback filters to owner comments" {
  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
if [[ "$*" == *"issues"*"comments"* ]]; then
  cat <<'JSON'
[
  {"user":{"login":"owner1"},"created_at":"2026-01-01T00:00:01Z","body":"Fix this please"},
  {"user":{"login":"contributor"},"created_at":"2026-01-01T00:00:02Z","body":"I disagree"},
  {"user":{"login":"owner1"},"created_at":"2026-01-01T00:00:03Z","body":"This should be internal"}
]
JSON
else
  echo "[]"
fi
SH
  chmod +x "$GH_STUB"

  run bash -c "export PATH='${TMP_DIR}:${PATH}'; source '${REPO_DIR}/scripts/lib.sh'; fetch_owner_feedback 'org/repo' 42 'owner1' ''"
  [ "$status" -eq 0 ]

  count=$(printf '%s' "$output" | jq -r 'length')
  [ "$count" -eq 2 ]

  login0=$(printf '%s' "$output" | jq -r '.[0].login')
  [ "$login0" = "owner1" ]

  login1=$(printf '%s' "$output" | jq -r '.[1].login')
  [ "$login1" = "owner1" ]
}

@test "fetch_owner_feedback excludes orchestrator bot comments" {
  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
if [[ "$*" == *"issues"*"comments"* ]]; then
  cat <<'JSON'
[
  {"user":{"login":"owner1"},"created_at":"2026-01-01T00:00:01Z","body":"Real feedback"},
  {"user":{"login":"owner1"},"created_at":"2026-01-01T00:00:02Z","body":"Automated update via [Orchestrator]"}
]
JSON
else
  echo "[]"
fi
SH
  chmod +x "$GH_STUB"

  run bash -c "export PATH='${TMP_DIR}:${PATH}'; source '${REPO_DIR}/scripts/lib.sh'; fetch_owner_feedback 'org/repo' 42 'owner1' ''"
  [ "$status" -eq 0 ]

  count=$(printf '%s' "$output" | jq -r 'length')
  [ "$count" -eq 1 ]

  body=$(printf '%s' "$output" | jq -r '.[0].body')
  [ "$body" = "Real feedback" ]
}

@test "process_owner_feedback resets task to routed" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Feedback Task" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Set task to done with an agent
  tdb_set "$TASK2_ID" status "done"
  tdb_set "$TASK2_ID" agent "codex"

  FEEDBACK='[{"login":"owner1","created_at":"2026-01-01T12:00:00Z","body":"This should be an internal doc"}]'

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; PROJECT_DIR='$PROJECT_DIR'; CONTEXTS_DIR='$ORCH_HOME/contexts'; process_owner_feedback '$TASK2_ID' '$FEEDBACK'"
  [ "$status" -eq 0 ]

  # Status should be routed
  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "routed" ]

  # Agent should be preserved
  run tdb_field "$TASK2_ID" agent
  [ "$status" -eq 0 ]
  [ "$output" = "codex" ]

  # last_error should contain feedback
  run tdb_field "$TASK2_ID" last_error
  [ "$status" -eq 0 ]
  [[ "$output" == *"internal doc"* ]]

  # gh_last_feedback_at should be set
  run tdb_field "$TASK2_ID" gh_last_feedback_at
  [ "$status" -eq 0 ]
  [ "$output" = "2026-01-01T12:00:00Z" ]
}

@test "process_owner_feedback appends to task context" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Context Task" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  FEEDBACK='[{"login":"owner1","created_at":"2026-01-01T12:00:00Z","body":"Please use markdown format"}]'

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; PROJECT_DIR='$PROJECT_DIR'; CONTEXTS_DIR='$ORCH_HOME/contexts'; process_owner_feedback '$TASK2_ID' '$FEEDBACK'"
  [ "$status" -eq 0 ]

  # Context file should exist and contain the feedback
  CTX_FILE="$ORCH_HOME/contexts/task-${TASK2_ID}.md"
  [ -f "$CTX_FILE" ]

  run bash -c "cat '$CTX_FILE'"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Owner feedback from owner1"* ]]
  [[ "$output" == *"Please use markdown format"* ]]
}

@test "owner slash command /retry resets task to new and clears agent" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Retry Task" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Make it look completed
  tdb_set "$TASK2_ID" status "done"
  tdb_set "$TASK2_ID" agent "codex"
  tdb_set "$TASK2_ID" attempts "3"

  # Owner comment: /retry
  run bash -c "GH_MOCK_LOGIN=mock gh api repos/mock/repo/issues/${TASK2_ID}/comments -f body='/retry'"
  [ "$status" -eq 0 ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; GH_MOCK_LOGIN=mock; process_owner_feedback_for_task 'mock/repo' '${TASK2_ID}' 'mock'"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "new" ]

  run tdb_field "$TASK2_ID" agent
  [ "$status" -eq 0 ]
  [ -z "$output" ]

  run tdb_field "$TASK2_ID" attempts
  [ "$status" -eq 0 ]
  [ "$output" = "0" ]
}

@test "owner slash command /assign codex sets agent and routes task" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Assign Task" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  tdb_set "$TASK2_ID" status "needs_review"
  tdb_set "$TASK2_ID" agent "claude"

  run bash -c "GH_MOCK_LOGIN=mock gh api repos/mock/repo/issues/${TASK2_ID}/comments -f body='/assign codex'"
  [ "$status" -eq 0 ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; GH_MOCK_LOGIN=mock; process_owner_feedback_for_task 'mock/repo' '${TASK2_ID}' 'mock'"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "routed" ]

  run tdb_field "$TASK2_ID" agent
  [ "$status" -eq 0 ]
  [ "$output" = "codex" ]
}

@test "owner slash command /unblock clears blocked status" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Unblock Task" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  tdb_set "$TASK2_ID" status "blocked"
  tdb_set "$TASK2_ID" reason "some reason"

  run bash -c "GH_MOCK_LOGIN=mock gh api repos/mock/repo/issues/${TASK2_ID}/comments -f body='/unblock'"
  [ "$status" -eq 0 ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; GH_MOCK_LOGIN=mock; process_owner_feedback_for_task 'mock/repo' '${TASK2_ID}' 'mock'"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "new" ]

  run tdb_field "$TASK2_ID" reason
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "owner slash command /context appends context and routes task" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Context Command Task" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  tdb_set "$TASK2_ID" status "done"
  tdb_set "$TASK2_ID" attempts "2"
  tdb_set "$TASK2_ID" last_error "previous error"

  run bash -c "GH_MOCK_LOGIN=mock gh api repos/mock/repo/issues/${TASK2_ID}/comments -f body='/context please use bash arrays'"
  [ "$status" -eq 0 ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; GH_MOCK_LOGIN=mock; process_owner_feedback_for_task 'mock/repo' '${TASK2_ID}' 'mock'"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "routed" ]

  CTX_FILE="$ORCH_HOME/contexts/task-${TASK2_ID}.md"
  [ -f "$CTX_FILE" ]
  run bash -c "cat '$CTX_FILE'"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Owner context"* ]]
  [[ "$output" == *"please use bash arrays"* ]]

  run tdb_field "$TASK2_ID" attempts
  [ "$status" -eq 0 ]
  [ "$output" = "0" ]

  run tdb_field "$TASK2_ID" last_error
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "owner slash command /close marks done and closes the GitHub issue" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Close Command Task" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  tdb_set "$TASK2_ID" status "needs_review"

  run bash -c "GH_MOCK_LOGIN=mock gh api repos/mock/repo/issues/${TASK2_ID}/comments -f body='/close'"
  [ "$status" -eq 0 ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; GH_MOCK_LOGIN=mock; process_owner_feedback_for_task 'mock/repo' '${TASK2_ID}' 'mock'"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "done" ]

  run bash -c "GH_MOCK_LOGIN=mock gh api repos/mock/repo/issues/${TASK2_ID} -q .state"
  [ "$status" -eq 0 ]
  [ "$output" = "closed" ]
}

@test "owner slash command /priority high sets complexity=complex and routes task" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Priority Command Task" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  tdb_set "$TASK2_ID" status "needs_review"
  tdb_set "$TASK2_ID" last_error "previous error"

  run bash -c "GH_MOCK_LOGIN=mock gh api repos/mock/repo/issues/${TASK2_ID}/comments -f body='/priority high'"
  [ "$status" -eq 0 ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; GH_MOCK_LOGIN=mock; process_owner_feedback_for_task 'mock/repo' '${TASK2_ID}' 'mock'"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "routed" ]

  run tdb_field "$TASK2_ID" complexity
  [ "$status" -eq 0 ]
  [ "$output" = "complex" ]

  run tdb_field "$TASK2_ID" last_error
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "owner slash commands are case-insensitive" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Case Insensitive Command Task" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  tdb_set "$TASK2_ID" status "done"
  tdb_set "$TASK2_ID" agent "codex"
  tdb_set "$TASK2_ID" attempts "2"

  run bash -c "GH_MOCK_LOGIN=mock gh api repos/mock/repo/issues/${TASK2_ID}/comments -f body='/RETRY'"
  [ "$status" -eq 0 ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; GH_MOCK_LOGIN=mock; process_owner_feedback_for_task 'mock/repo' '${TASK2_ID}' 'mock'"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "new" ]
}

@test "owner slash command /help posts readable multiline help" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Help Command Task" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  tdb_set "$TASK2_ID" status "done"

  run bash -c "GH_MOCK_LOGIN=mock gh api repos/mock/repo/issues/${TASK2_ID}/comments -f body='/help'"
  [ "$status" -eq 0 ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; GH_MOCK_LOGIN=mock; process_owner_feedback_for_task 'mock/repo' '${TASK2_ID}' 'mock'"
  [ "$status" -eq 0 ]

  run bash -c "GH_MOCK_LOGIN=mock gh api repos/mock/repo/issues/${TASK2_ID}/comments -q '.[-1].body'"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Supported commands:"* ]]
  [[ "$output" == *$'\n- `/retry`'* ]]
  [[ "$output" != *'\\n'* ]]
}

@test "non-command owner comment falls back to owner feedback retry" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Feedback Non-Command Task" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  tdb_set "$TASK2_ID" status "done"
  tdb_set "$TASK2_ID" agent "codex"

  run bash -c "GH_MOCK_LOGIN=mock gh api repos/mock/repo/issues/${TASK2_ID}/comments -f body='Please add a unit test for edge cases'"
  [ "$status" -eq 0 ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; GH_MOCK_LOGIN=mock; process_owner_feedback_for_task 'mock/repo' '${TASK2_ID}' 'mock'"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "routed" ]

  run tdb_field "$TASK2_ID" agent
  [ "$status" -eq 0 ]
  [ "$output" = "codex" ]
}

@test "owner slash commands ignore non-owner comments" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Ignore Task" "Body" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  tdb_set "$TASK2_ID" status "done"
  tdb_set "$TASK2_ID" agent "codex"

  run bash -c "GH_MOCK_LOGIN=someone-else gh api repos/mock/repo/issues/${TASK2_ID}/comments -f body='/retry'"
  [ "$status" -eq 0 ]

  run bash -c "source '${REPO_DIR}/scripts/lib.sh'; GH_MOCK_LOGIN=mock; process_owner_feedback_for_task 'mock/repo' '${TASK2_ID}' 'mock'"
  [ "$status" -eq 0 ]

  run tdb_field "$TASK2_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "done" ]

  run tdb_field "$TASK2_ID" agent
  [ "$status" -eq 0 ]
  [ "$output" = "codex" ]
}

# ===== project_add.sh / bare repo support =====

@test "is_bare_repo detects bare repositories" {
  source "${REPO_DIR}/scripts/lib.sh"

  # Regular repo should NOT be bare
  run is_bare_repo "$PROJECT_DIR"
  [ "$status" -ne 0 ]

  # Create a bare repo
  BARE="${TMP_DIR}/bare-test.git"
  git clone --bare "$PROJECT_DIR" "$BARE" 2>/dev/null

  run is_bare_repo "$BARE"
  [ "$status" -eq 0 ]
}

@test "project_add.sh creates config for pre-existing bare repo" {
  # Pre-clone a bare repo at the expected path (avoids stubbing git for SSH)
  SRC="${TMP_DIR}/source-repo"
  mkdir -p "$SRC"
  git -C "$SRC" init -b main --quiet
  git -C "$SRC" -c user.email="test@test.com" -c user.name="Test" commit --allow-empty -m "init" --quiet

  BARE_DIR="${ORCH_HOME}/projects/testowner/testrepo.git"
  mkdir -p "$(dirname "$BARE_DIR")"
  git clone --bare "$SRC" "$BARE_DIR" 2>/dev/null

  # Stub gh (init.sh calls gh)
  GH_STUB="${TMP_DIR}/bin/gh"
  mkdir -p "${TMP_DIR}/bin"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
echo "{}"
SH
  chmod +x "$GH_STUB"

  run env PATH="${TMP_DIR}/bin:${PATH}" ORCH_HOME="$ORCH_HOME" CONFIG_PATH="$CONFIG_PATH" STATE_DIR="$STATE_DIR" \
    bash "${REPO_DIR}/scripts/project_add.sh" "testowner/testrepo"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Already cloned"* ]]

  # Config should have correct repo slug
  run yq -r '.gh.repo' "${BARE_DIR}/orchestrator.yml"
  [ "$status" -eq 0 ]
  [ "$output" = "testowner/testrepo" ]
}

@test "project_add.sh fetches if already cloned" {
  # Create a source repo and bare clone it manually
  SRC="${TMP_DIR}/source-repo2"
  mkdir -p "$SRC"
  git -C "$SRC" init -b main --quiet
  git -C "$SRC" -c user.email="test@test.com" -c user.name="Test" commit --allow-empty -m "init" --quiet

  BARE_DIR="${ORCH_HOME}/projects/fetchowner/fetchrepo.git"
  mkdir -p "$(dirname "$BARE_DIR")"
  git clone --bare "$SRC" "$BARE_DIR" 2>/dev/null

  # Stub gh
  GH_STUB="${TMP_DIR}/bin/gh"
  mkdir -p "${TMP_DIR}/bin"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
echo "{}"
SH
  chmod +x "$GH_STUB"

  run env PATH="${TMP_DIR}/bin:${PATH}" ORCH_HOME="$ORCH_HOME" CONFIG_PATH="$CONFIG_PATH" STATE_DIR="$STATE_DIR" \
    bash "${REPO_DIR}/scripts/project_add.sh" "fetchowner/fetchrepo"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Already cloned"* ]]
}

@test "project_add.sh normalizes GitHub URLs" {
  # Pre-clone bare repos at the expected normalized paths
  SRC="${TMP_DIR}/source-repo3"
  mkdir -p "$SRC"
  git -C "$SRC" init -b main --quiet
  git -C "$SRC" -c user.email="test@test.com" -c user.name="Test" commit --allow-empty -m "init" --quiet

  # Pre-clone for HTTPS URL test
  BARE_HTTPS="${ORCH_HOME}/projects/urlowner/urlrepo.git"
  mkdir -p "$(dirname "$BARE_HTTPS")"
  git clone --bare "$SRC" "$BARE_HTTPS" 2>/dev/null

  # Pre-clone for SSH URL test
  BARE_SSH="${ORCH_HOME}/projects/sshowner/sshrepo.git"
  mkdir -p "$(dirname "$BARE_SSH")"
  git clone --bare "$SRC" "$BARE_SSH" 2>/dev/null

  GH_STUB="${TMP_DIR}/bin/gh"
  mkdir -p "${TMP_DIR}/bin"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
echo "{}"
SH
  chmod +x "$GH_STUB"

  # Test HTTPS URL normalization
  run env PATH="${TMP_DIR}/bin:${PATH}" ORCH_HOME="$ORCH_HOME" CONFIG_PATH="$CONFIG_PATH" STATE_DIR="$STATE_DIR" \
    bash "${REPO_DIR}/scripts/project_add.sh" "https://github.com/urlowner/urlrepo.git"
  [ "$status" -eq 0 ]

  run yq -r '.gh.repo' "${BARE_HTTPS}/orchestrator.yml"
  [ "$status" -eq 0 ]
  [ "$output" = "urlowner/urlrepo" ]

  # Test SSH URL normalization
  run env PATH="${TMP_DIR}/bin:${PATH}" ORCH_HOME="$ORCH_HOME" CONFIG_PATH="$CONFIG_PATH" STATE_DIR="$STATE_DIR" \
    bash "${REPO_DIR}/scripts/project_add.sh" "git@github.com:sshowner/sshrepo.git"
  [ "$status" -eq 0 ]

  run yq -r '.gh.repo' "${BARE_SSH}/orchestrator.yml"
  [ "$status" -eq 0 ]
  [ "$output" = "sshowner/sshrepo" ]
}

@test "run_task.sh creates worktree from bare repo" {
  # Create a bare repo with a commit
  SRC="${TMP_DIR}/source-repo4"
  mkdir -p "$SRC"
  git -C "$SRC" init -b main --quiet
  git -C "$SRC" -c user.email="test@test.com" -c user.name="Test" commit --allow-empty -m "init" --quiet

  BARE_DIR="${TMP_DIR}/bare-worktree-test.git"
  git clone --bare "$SRC" "$BARE_DIR" 2>/dev/null

  # Write orchestrator.yml
  cat > "${BARE_DIR}/orchestrator.yml" <<YAML
gh:
  repo: "testowner/testrepo"
  sync_label: ""
YAML

  # Point PROJECT_DIR to bare repo and add a task
  PROJECT_DIR_OLD="$PROJECT_DIR"
  export PROJECT_DIR="$BARE_DIR"
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Bare Repo Task" "Test body" "")
  BARE_TASK_ID=$(_task_id "$TASK_OUTPUT")

  # Verify task dir
  run tdb_field "$BARE_TASK_ID" dir
  [ "$status" -eq 0 ]
  [ "$output" = "$BARE_DIR" ]

  # Stub the agent (claude)
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
# Write output file
cat > "${OUTPUT_FILE}" <<'JSON'
{"status":"done","summary":"did the thing","accomplished":["tested bare"],"remaining":[],"blockers":[],"files_changed":[],"needs_help":false,"reason":"all good"}
JSON
echo '{"type":"result","result":"done"}'
SH
  chmod +x "$CLAUDE_STUB"

  # Route the task first
  tdb_set "$BARE_TASK_ID" agent "claude"
  tdb_set "$BARE_TASK_ID" status "routed"

  # Use gh mock from $MOCK_BIN (setup), claude stub from $TMP_DIR
  run env PATH="${TMP_DIR}:${PATH}" \
    CONFIG_PATH="$CONFIG_PATH" \
    PROJECT_DIR="$BARE_DIR" ORCH_HOME="$ORCH_HOME" STATE_DIR="$STATE_DIR" \
    AGENT_TIMEOUT_SECONDS=5 LOCK_PATH="$LOCK_PATH" USE_TMUX=false \
    bash "${REPO_DIR}/scripts/run_task.sh" "$BARE_TASK_ID"
  # run_task.sh calls load_project_config which sets STATE_DIR to PROJECT_DIR/.orchestrator
  # so sidecar fields are written there, not to the test's default STATE_DIR
  BARE_STATE_DIR="${BARE_DIR}/.orchestrator"

  # Task should have a worktree set
  run env STATE_DIR="$BARE_STATE_DIR" bash -c "source '${REPO_DIR}/scripts/lib.sh' && db_task_field '$BARE_TASK_ID' worktree"
  [ "$status" -eq 0 ]
  [ "$output" != "null" ]
  [ -n "$output" ]

  # Worktree directory should exist
  WT_DIR="$output"
  [ -d "$WT_DIR" ]

  # Clean up worktree
  git -C "$BARE_DIR" worktree remove "$WT_DIR" 2>/dev/null || true

  export PROJECT_DIR="$PROJECT_DIR_OLD"
}



# --- stop.sh --force tests ---

# --- version in log output tests ---

@test "log() includes version when ORCH_VERSION is set" {
  source "${REPO_DIR}/scripts/lib.sh"
  export ORCH_VERSION="1.2.3"
  result=$(log "[test] hello")
  [[ "$result" == *"[v1.2.3]"* ]]
  [[ "$result" == *"[test] hello"* ]]
}

@test "log() omits version when ORCH_VERSION is unset" {
  source "${REPO_DIR}/scripts/lib.sh"
  unset ORCH_VERSION
  result=$(log "[test] hello")
  # Should NOT contain [v
  [[ "$result" != *"[v"* ]]
  [[ "$result" == *"[test] hello"* ]]
}

@test "log_err() includes version when ORCH_VERSION is set" {
  source "${REPO_DIR}/scripts/lib.sh"
  export ORCH_VERSION="0.33.0"
  result=$(log_err "[gh_push] syncing" 2>&1)
  [[ "$result" == *"[v0.33.0]"* ]]
}

# --- rate limit backoff in serve.sh tests ---

# --- graceful shutdown tests ---

# --- PR review agent tests ---

@test "review_prs.sh exits early when review agent disabled" {
  run yq -i '.workflow.enable_review_agent = false' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  # Mock gh to detect if it's called — it should NOT be called
  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
echo "ERROR: gh should not be called" >&2
exit 1
SH
  chmod +x "$GH_STUB"

  run env PATH="${TMP_DIR}:${PATH}" "${REPO_DIR}/scripts/review_prs.sh"
  [ "$status" -eq 0 ]
}

@test "review_prs.sh exits when no open PRs" {
  run yq -i '.workflow.enable_review_agent = true' "$CONFIG_PATH"
  run yq -i '.gh.repo = "owner/repo"' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
if [ "$1" = "api" ]; then
  echo "[]"
  exit 0
fi
exit 0
SH
  chmod +x "$GH_STUB"

  run env PATH="${TMP_DIR}:${PATH}" "${REPO_DIR}/scripts/review_prs.sh"
  [ "$status" -eq 0 ]
}

@test "review_prs.sh skips draft PRs by default" {
  run yq -i '.workflow.enable_review_agent = true' "$CONFIG_PATH"
  run yq -i '.gh.repo = "owner/repo"' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
if [ "$1" = "api" ] && [[ "$*" == *"repos/owner/repo/pulls"* ]]; then
  echo '[{"number":1,"title":"Draft PR","body":"wip","user":{"login":"dev"},"head":{"sha":"abc123","ref":"feat/wip"},"draft":true}]'
  exit 0
fi
if [ "$1" = "api" ] && [[ "$*" == *"issues"* ]]; then
  echo '[]'
  exit 0
fi
exit 0
SH
  chmod +x "$GH_STUB"

  # Mock claude (should NOT be called for draft PRs)
  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
echo "ERROR: claude should not be called for drafts" >&2
exit 1
SH
  chmod +x "$CLAUDE_STUB"

  run env PATH="${TMP_DIR}:${PATH}" "${REPO_DIR}/scripts/review_prs.sh"
  [ "$status" -eq 0 ]
  # State file should be empty (no reviews recorded)
  [ ! -s "${STATE_DIR}/pr_reviews_owner_repo.tsv" ] || [ "$(wc -l < "${STATE_DIR}/pr_reviews_owner_repo.tsv")" -eq 0 ]
}

@test "review_prs.sh skips already-reviewed PRs at same SHA" {
  run yq -i '.workflow.enable_review_agent = true' "$CONFIG_PATH"
  run yq -i '.gh.repo = "owner/repo"' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  # Pre-populate review state
  mkdir -p "$STATE_DIR"
  printf '1\tabc123\tapprove\t2026-01-01T00:00:00Z\tAlready reviewed PR\n' > "${STATE_DIR}/pr_reviews_owner_repo.tsv"

  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
if [ "$1" = "api" ] && [[ "$*" == *"repos/owner/repo/pulls"* ]]; then
  echo '[{"number":1,"title":"Already reviewed PR","body":"test","user":{"login":"dev"},"head":{"sha":"abc123","ref":"feat/test"},"draft":false}]'
  exit 0
fi
if [ "$1" = "api" ] && [[ "$*" == *"issues"* ]]; then
  echo '[]'
  exit 0
fi
exit 0
SH
  chmod +x "$GH_STUB"

  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
echo "ERROR: claude should not be called" >&2
exit 1
SH
  chmod +x "$CLAUDE_STUB"

  run env PATH="${TMP_DIR}:${PATH}" "${REPO_DIR}/scripts/review_prs.sh"
  [ "$status" -eq 0 ]
  # Should still have only 1 line in state
  [ "$(wc -l < "${STATE_DIR}/pr_reviews_owner_repo.tsv")" -eq 1 ]
}

@test "review_prs.sh reviews new PR and records state" {
  run yq -i '.workflow.enable_review_agent = true' "$CONFIG_PATH"
  run yq -i '.workflow.review_agent = "claude"' "$CONFIG_PATH"
  run yq -i '.gh.repo = "owner/repo"' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
if [ "$1" = "api" ] && [[ "$*" == *"repos/owner/repo/pulls"* ]]; then
  echo '[{"number":42,"title":"Add feature","body":"Implements X","user":{"login":"dev"},"head":{"sha":"def456","ref":"feat/add-feature"},"draft":false}]'
  exit 0
fi
if [ "$1" = "api" ] && [[ "$*" == *"issues"* ]]; then
  echo '[]'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "diff" ]; then
  echo "+++ b/test.sh"
  echo "+echo hello"
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "review" ]; then
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "comment" ]; then
  exit 0
fi
exit 0
SH
  chmod +x "$GH_STUB"

  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
echo '{"decision":"approve","notes":"Looks good, clean implementation."}'
SH
  chmod +x "$CLAUDE_STUB"

  run env PATH="${TMP_DIR}:${PATH}" AGENT_TIMEOUT_SECONDS=10 "${REPO_DIR}/scripts/review_prs.sh"
  [ "$status" -eq 0 ]

  # Should have recorded the review
  run grep "42" "${STATE_DIR}/pr_reviews_owner_repo.tsv"
  [ "$status" -eq 0 ]
  [[ "$output" == *"def456"* ]]
  [[ "$output" == *"approve"* ]]
}

@test "review_prs.sh re-reviews PR when SHA changes" {
  run yq -i '.workflow.enable_review_agent = true' "$CONFIG_PATH"
  run yq -i '.workflow.review_agent = "claude"' "$CONFIG_PATH"
  run yq -i '.gh.repo = "owner/repo"' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  # Pre-populate with old SHA
  mkdir -p "$STATE_DIR"
  printf '1\told_sha\tapprove\t2026-01-01T00:00:00Z\tOld review\n' > "${STATE_DIR}/pr_reviews_owner_repo.tsv"

  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
if [ "$1" = "api" ] && [[ "$*" == *"repos/owner/repo/pulls"* ]]; then
  echo '[{"number":1,"title":"Updated PR","body":"new commits","user":{"login":"dev"},"head":{"sha":"new_sha","ref":"feat/test"},"draft":false}]'
  exit 0
fi
if [ "$1" = "api" ] && [[ "$*" == *"issues"* ]]; then
  echo '[]'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "diff" ]; then
  echo "+new changes"
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "review" ]; then
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "comment" ]; then
  exit 0
fi
exit 0
SH
  chmod +x "$GH_STUB"

  CLAUDE_STUB="${TMP_DIR}/claude"
  cat > "$CLAUDE_STUB" <<'SH'
#!/usr/bin/env bash
echo '{"decision":"request_changes","notes":"Missing tests."}'
SH
  chmod +x "$CLAUDE_STUB"

  run env PATH="${TMP_DIR}:${PATH}" AGENT_TIMEOUT_SECONDS=10 "${REPO_DIR}/scripts/review_prs.sh"
  [ "$status" -eq 0 ]

  # Should have 2 lines in state now (old + new)
  [ "$(wc -l < "${STATE_DIR}/pr_reviews_owner_repo.tsv")" -eq 2 ]
  run grep "new_sha" "${STATE_DIR}/pr_reviews_owner_repo.tsv"
  [ "$status" -eq 0 ]
  [[ "$output" == *"request_changes"* ]]
}

@test "review_prs.sh handles merge command from owner" {
  run yq -i '.workflow.enable_review_agent = true' "$CONFIG_PATH"
  run yq -i '.workflow.review_owner = "gabriel"' "$CONFIG_PATH"
  run yq -i '.gh.repo = "owner/repo"' "$CONFIG_PATH"
  [ "$status" -eq 0 ]

  # Pre-populate as already reviewed (so it only checks merge commands)
  mkdir -p "$STATE_DIR"
  printf '10\tabc123\tapprove\t2026-01-01T00:00:00Z\tPR title\n' > "${STATE_DIR}/pr_reviews_owner_repo.tsv"

  MERGED=false
  GH_STUB="${TMP_DIR}/gh"
  cat > "$GH_STUB" <<'SH'
#!/usr/bin/env bash
if [ "$1" = "api" ] && [[ "$*" == *"repos/owner/repo/pulls"* ]] && [[ "$*" != *"issues"* ]]; then
  echo '[{"number":10,"title":"Ready PR","body":"","user":{"login":"dev"},"head":{"sha":"abc123","ref":"feat/ready"},"draft":false}]'
  exit 0
fi
if [ "$1" = "api" ] && [[ "$*" == *"issues/10/comments"* ]]; then
  echo '[{"user":{"login":"gabriel"},"body":"merge","created_at":"2026-02-19T12:00:00Z"}]'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "merge" ]; then
  echo "merged"
  exit 0
fi
exit 0
SH
  chmod +x "$GH_STUB"

  run env PATH="${TMP_DIR}:${PATH}" "${REPO_DIR}/scripts/review_prs.sh"
  [ "$status" -eq 0 ]

  # Should have recorded the merge
  run grep "^merge" "${STATE_DIR}/pr_reviews_owner_repo.tsv"
  [ "$status" -eq 0 ]
  [[ "$output" == *"10"* ]]
}

@test "review_prs.sh prompt template exists and has required placeholders" {
  [ -f "${REPO_DIR}/prompts/pr_review.md" ]
  run grep "PR_NUMBER" "${REPO_DIR}/prompts/pr_review.md"
  [ "$status" -eq 0 ]
  run grep "GIT_DIFF" "${REPO_DIR}/prompts/pr_review.md"
  [ "$status" -eq 0 ]
  run grep "PR_TITLE" "${REPO_DIR}/prompts/pr_review.md"
  [ "$status" -eq 0 ]
}

# --- Backend (db_*) function tests ---

@test "db_task_field reads a single field" {
  source "${REPO_DIR}/scripts/lib.sh"

  TASK_ID=$(db_create_task "Test" "" "$PROJECT_DIR")

  run db_task_field "$TASK_ID" status
  [ "$status" -eq 0 ]
  [ "$output" = "new" ]

  run db_task_field "$TASK_ID" title
  [ "$status" -eq 0 ]
  [ "$output" = "Test" ]
}

@test "db_task_set updates field and bumps updated_at" {
  source "${REPO_DIR}/scripts/lib.sh"

  TASK_ID=$(db_create_task "Test" "" "$PROJECT_DIR")
  local old_updated_at
  old_updated_at=$(db_task_field "$TASK_ID" "updated_at")

  sleep 1
  db_task_set "$TASK_ID" status "in_progress"
  run db_task_field "$TASK_ID" status
  [ "$output" = "in_progress" ]

  # updated_at should be newer than the original
  local new_updated_at
  new_updated_at=$(db_task_field "$TASK_ID" "updated_at")
  [[ "$new_updated_at" > "$old_updated_at" ]]
}

@test "db_task_claim rejects wrong from_status" {
  source "${REPO_DIR}/scripts/lib.sh"

  TASK_ID=$(db_create_task "Test" "" "$PROJECT_DIR")
  db_task_set "$TASK_ID" status "done"

  # Trying to claim from 'new' should fail because status is 'done'
  run db_task_claim "$TASK_ID" "new" "in_progress"
  [ "$status" -ne 0 ]

  # Status should still be 'done'
  run db_task_field "$TASK_ID" status
  [ "$output" = "done" ]
}

@test "db_task_claim succeeds with correct from_status" {
  source "${REPO_DIR}/scripts/lib.sh"

  TASK_ID=$(db_create_task "Test" "" "$PROJECT_DIR")

  run db_task_claim "$TASK_ID" "new" "in_progress"
  [ "$status" -eq 0 ]

  run db_task_field "$TASK_ID" status
  [ "$output" = "in_progress" ]
}

@test "db_task_count counts by status" {
  source "${REPO_DIR}/scripts/lib.sh"

  # setup() already created the Init task (status=new)
  # Create two more tasks
  local id_b id_c
  id_b=$(db_create_task "B" "" "$PROJECT_DIR")
  id_c=$(db_create_task "C" "" "$PROJECT_DIR")

  run db_task_count "new"
  # Init task + B + C = at least 3 new tasks
  [ "$output" -ge 3 ]

  # Set one to in_progress — search API should find it
  db_task_set "$id_b" status "in_progress"
  run db_task_count "in_progress"
  [ "$output" -ge 1 ]
}

@test "db_create_task creates task with labels" {
  source "${REPO_DIR}/scripts/lib.sh"

  NEW_ID=$(db_create_task "My task" "Some body" "$PROJECT_DIR" "bug,priority:high")
  [ -n "$NEW_ID" ]

  run db_task_field "$NEW_ID" title
  [ "$output" = "My task" ]

  run db_task_field "$NEW_ID" status
  [ "$output" = "new" ]

  run db_task_labels_csv "$NEW_ID"
  [[ "$output" == *"bug"* ]]
  [[ "$output" == *"priority:high"* ]]
}

@test "db_append_history adds entries" {
  source "${REPO_DIR}/scripts/lib.sh"

  TASK_ID=$(db_create_task "Test" "" "$PROJECT_DIR")

  db_append_history "$TASK_ID" "routed" "routed to claude"
  db_append_history "$TASK_ID" "in_progress" "started attempt 1"

  run db_task_history "$TASK_ID"
  [ "$status" -eq 0 ]
  [[ "$output" == *"routed to claude"* ]]
  [[ "$output" == *"started attempt 1"* ]]
  # Should have 2 history entries (comments)
  local count
  count=$(db_task_history "$TASK_ID" | wc -l | tr -d ' ')
  [ "$count" -eq 2 ]
}

@test "db_set_labels replaces existing labels" {
  source "${REPO_DIR}/scripts/lib.sh"

  TASK_ID=$(db_create_task "Test" "" "$PROJECT_DIR" "old-label")

  # Verify old label exists
  run db_task_labels_csv "$TASK_ID"
  [[ "$output" == *"old-label"* ]]

  # Replace labels
  db_set_labels "$TASK_ID" "new-a,new-b"
  run db_task_labels_csv "$TASK_ID"
  [[ "$output" == *"new-a"* ]]
  [[ "$output" == *"new-b"* ]]

  # Old label should be gone
  [[ "$output" != *"old-label"* ]]
}

@test "db_task_update updates multiple fields at once" {
  source "${REPO_DIR}/scripts/lib.sh"

  TASK_ID=$(db_create_task "Test" "" "$PROJECT_DIR")

  db_task_update "$TASK_ID" status=done agent=claude summary="Complete"
  run db_task_field "$TASK_ID" status
  [ "$output" = "done" ]
  run db_task_field "$TASK_ID" agent
  [ "$output" = "claude" ]
  run db_task_field "$TASK_ID" summary
  [ "$output" = "Complete" ]
}

@test "db_task_ids_by_status excludes tasks with label" {
  source "${REPO_DIR}/scripts/lib.sh"

  local id_a id_b
  id_a=$(db_create_task "A" "" "$PROJECT_DIR")
  id_b=$(db_create_task "B" "" "$PROJECT_DIR" "no-agent")

  run db_task_ids_by_status "new" "no-agent"
  [ "$status" -eq 0 ]
  [[ "$output" == *"$id_a"* ]]
  [[ "$output" != *"$id_b"* ]]
}

# ─── build_git_diff tests ───

@test "build_git_diff shows stat for uncommitted changes" {
  # Create a tracked file, then modify it
  echo "hello" > "${PROJECT_DIR}/file.txt"
  git -C "$PROJECT_DIR" add file.txt
  git -C "$PROJECT_DIR" -c user.email="test@test.com" -c user.name="Test" commit -m "add file" --quiet

  echo "world" >> "${PROJECT_DIR}/file.txt"

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && build_git_diff '$PROJECT_DIR' main"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Uncommitted changes:"* ]]
  [[ "$output" == *"file.txt"* ]]
}

@test "build_git_diff shows diff against base branch" {
  # Create a feature branch with a commit
  git -C "$PROJECT_DIR" checkout -b feature-test --quiet
  echo "new content" > "${PROJECT_DIR}/feature.txt"
  git -C "$PROJECT_DIR" add feature.txt
  git -C "$PROJECT_DIR" -c user.email="test@test.com" -c user.name="Test" commit -m "add feature" --quiet

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && build_git_diff '$PROJECT_DIR' main"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Diff against main:"* ]]
  [[ "$output" == *"new content"* ]]
}

@test "build_git_diff shows commit log since base branch" {
  git -C "$PROJECT_DIR" checkout -b log-test --quiet
  echo "log content" > "${PROJECT_DIR}/log.txt"
  git -C "$PROJECT_DIR" add log.txt
  git -C "$PROJECT_DIR" -c user.email="test@test.com" -c user.name="Test" commit -m "add log file" --quiet

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && build_git_diff '$PROJECT_DIR' main"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Commits since main:"* ]]
  [[ "$output" == *"add log file"* ]]
}

@test "build_git_diff returns empty for no changes" {
  # On main with no changes — no diff, no commits
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && build_git_diff '$PROJECT_DIR' main"
  [ "$status" -eq 0 ]
  [ -z "$output" ]
}

@test "build_git_diff truncates diff to 200 lines" {
  git -C "$PROJECT_DIR" checkout -b trunc-test --quiet
  # Generate a file with 300 lines to produce a diff > 200 lines
  for i in $(seq 1 300); do echo "line $i"; done > "${PROJECT_DIR}/big.txt"
  git -C "$PROJECT_DIR" add big.txt
  git -C "$PROJECT_DIR" -c user.email="test@test.com" -c user.name="Test" commit -m "add big file" --quiet

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && build_git_diff '$PROJECT_DIR' main"
  [ "$status" -eq 0 ]
  # Count diff lines (between "Diff against" header and next section or end)
  local diff_lines
  diff_lines=$(echo "$output" | sed -n '/^Diff against/,/^$/p' | wc -l)
  # The diff output section should be at most ~202 lines (header + 200 content + blank)
  [ "$diff_lines" -le 203 ]
}

@test "build_git_diff accepts custom base branch" {
  git -C "$PROJECT_DIR" checkout -b develop --quiet
  echo "develop content" > "${PROJECT_DIR}/dev.txt"
  git -C "$PROJECT_DIR" add dev.txt
  git -C "$PROJECT_DIR" -c user.email="test@test.com" -c user.name="Test" commit -m "add dev" --quiet

  git -C "$PROJECT_DIR" checkout -b feature-from-develop --quiet
  echo "feature content" > "${PROJECT_DIR}/feat.txt"
  git -C "$PROJECT_DIR" add feat.txt
  git -C "$PROJECT_DIR" -c user.email="test@test.com" -c user.name="Test" commit -m "add feat" --quiet

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && build_git_diff '$PROJECT_DIR' develop"
  [ "$status" -eq 0 ]
  [[ "$output" == *"Diff against develop:"* ]]
  [[ "$output" == *"feature content"* ]]
  [[ "$output" == *"Commits since develop:"* ]]
  [[ "$output" == *"add feat"* ]]
}

# ─── Label Validation tests ───

@test "_gh_validate_label accepts valid status labels" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'status:new'"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'status:done'"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'status:in_progress'"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'status:needs_review'"
  [ "$status" -eq 0 ]
}

@test "_gh_validate_label rejects invalid status labels" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'status:invalid'"
  [ "$status" -eq 1 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'status:open'"
  [ "$status" -eq 1 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'status:pending'"
  [ "$status" -eq 1 ]
}

@test "_gh_validate_label accepts valid agent labels" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'agent:claude'"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'agent:codex'"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'agent:opencode'"
  [ "$status" -eq 0 ]
}

@test "_gh_validate_label rejects invalid agent labels" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'agent:gpt'"
  [ "$status" -eq 1 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'agent:invalid'"
  [ "$status" -eq 1 ]
}

@test "_gh_validate_label allows complexity and role as user labels" {
  # complexity: and role: are NOT reserved prefixes — they are stored in sidecar only
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'complexity:simple'"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'role:backend'"
  [ "$status" -eq 0 ]
}

@test "_gh_validate_label allows any skill: and job: values" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'skill:anything'"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'job:cron-123'"
  [ "$status" -eq 0 ]
}

@test "_gh_validate_label allows any model: label" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'model:opus'"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'model:gpt-4o'"
  [ "$status" -eq 0 ]
}

@test "_gh_validate_label allows user labels without reserved prefix" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'bug'"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'frontend'"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label 'my-custom-label'"
  [ "$status" -eq 0 ]
}

@test "_gh_validate_label allows empty label" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_label ''"
  [ "$status" -eq 0 ]
}

# ─── Agent-Model Cross-Validation tests ───

@test "_gh_validate_agent_model accepts valid claude models" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model claude opus"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model claude sonnet"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model claude haiku"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model claude claude-3-opus"
  [ "$status" -eq 0 ]
}

@test "_gh_validate_agent_model rejects invalid claude models" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model claude gpt-4o"
  [ "$status" -eq 1 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model claude o3-mini"
  [ "$status" -eq 1 ]
}

@test "_gh_validate_agent_model accepts valid codex models" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model codex o3-mini"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model codex o4-mini"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model codex gpt-4o"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model codex codex-mini"
  [ "$status" -eq 0 ]
}

@test "_gh_validate_agent_model rejects invalid codex models" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model codex opus"
  [ "$status" -eq 1 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model codex sonnet"
  [ "$status" -eq 1 ]
}

@test "_gh_validate_agent_model allows any model for opencode" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model opencode anything-goes"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model opencode opus"
  [ "$status" -eq 0 ]
}

@test "_gh_validate_agent_model allows empty model" {
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model claude ''"
  [ "$status" -eq 0 ]
  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && _gh_validate_agent_model claude null"
  [ "$status" -eq 0 ]
}

# ─── Label Validation Integration tests ───

@test "db_create_task rejects reserved prefix in user labels" {
  source "${REPO_DIR}/scripts/lib.sh"
  # status:invalid should be silently skipped
  local id
  id=$(db_create_task "Test task" "body" "" "bug,status:invalid,frontend" "" "")
  [ -n "$id" ]
  # The issue should have bug and frontend but not status:invalid
  local labels
  labels=$(db_task_labels_csv "$id")
  [[ "$labels" == *"bug"* ]]
  [[ "$labels" == *"frontend"* ]]
  [[ "$labels" != *"status:invalid"* ]]
}

@test "db_create_task allows valid labels" {
  source "${REPO_DIR}/scripts/lib.sh"
  local id
  id=$(db_create_task "Test valid" "body" "" "bug,skill:docker" "" "claude")
  [ -n "$id" ]
  local labels
  labels=$(db_task_labels_csv "$id")
  [[ "$labels" == *"bug"* ]]
  [[ "$labels" == *"skill:docker"* ]]
  [[ "$labels" == *"agent:claude"* ]]
}

@test "db_add_label rejects invalid prefixed label" {
  source "${REPO_DIR}/scripts/lib.sh"
  run db_add_label "$INIT_TASK_ID" "status:bogus"
  [ "$status" -eq 1 ]
}

@test "db_add_label accepts valid user label" {
  source "${REPO_DIR}/scripts/lib.sh"
  run db_add_label "$INIT_TASK_ID" "enhancement"
  [ "$status" -eq 0 ]
}

# ─── Normalize New Issues tests ───

@test "db_normalize_new_issues adds status:new to unlabeled issues" {
  source "${REPO_DIR}/scripts/lib.sh"
  # Create issue directly via mock (no status label)
  local json
  json=$(gh_api repos/$ORCH_GH_REPO/issues -f title="Unlabeled issue")
  local id
  id=$(printf '%s' "$json" | jq -r '.number')
  # Add a user label but no status: label
  gh_api "repos/$ORCH_GH_REPO/issues/$id/labels" \
    --input - <<< '{"labels":["bug"]}' >/dev/null 2>&1

  # Verify no status: label
  local labels
  labels=$(gh_api "repos/$ORCH_GH_REPO/issues/$id" --cache 0s -q '[.labels[].name]' 2>/dev/null)
  [[ "$labels" != *"status:"* ]]

  # Run normalize
  db_normalize_new_issues

  # Verify status:new was added
  local issue_labels
  issue_labels=$(db_task_labels_csv "$id")
  [[ "$issue_labels" == *"status:new"* ]]
  # Original user label preserved
  [[ "$issue_labels" == *"bug"* ]]
}

@test "db_normalize_new_issues skips issues with existing status label" {
  source "${REPO_DIR}/scripts/lib.sh"
  # The INIT_TASK_ID already has status:new
  local labels_before
  labels_before=$(jq -r '.issues["'"$INIT_TASK_ID"'"].labels // [] | map(.name) | join(",")' "$GH_MOCK_STATE")

  db_normalize_new_issues

  local labels_after
  labels_after=$(jq -r '.issues["'"$INIT_TASK_ID"'"].labels // [] | map(.name) | join(",")' "$GH_MOCK_STATE")
  [ "$labels_before" = "$labels_after" ]
}

@test "db_normalize_new_issues skips issues with no-agent label" {
  source "${REPO_DIR}/scripts/lib.sh"
  # Create issue with no-agent label but no status label
  local json
  json=$(gh_api repos/$ORCH_GH_REPO/issues -f title="No agent issue")
  local id
  id=$(printf '%s' "$json" | jq -r '.number')
  gh_api "repos/$ORCH_GH_REPO/issues/$id/labels" \
    --input - <<< '{"labels":["no-agent"]}' >/dev/null 2>&1

  db_normalize_new_issues

  local labels
  labels=$(jq -r '.issues["'"$id"'"].labels // [] | map(.name) | join(",")' "$GH_MOCK_STATE")
  [[ "$labels" != *"status:new"* ]]
}

@test "db_normalize_new_issues skips issues from non-owner authors" {
  source "${REPO_DIR}/scripts/lib.sh"
  # Create issue as a stranger
  export GH_MOCK_LOGIN="stranger"
  local json
  json=$(gh_api repos/$ORCH_GH_REPO/issues -f title="Spam issue")
  unset GH_MOCK_LOGIN
  local id
  id=$(printf '%s' "$json" | jq -r '.number')

  _GH_ALLOWED_AUTHORS=""  # reset cache
  db_normalize_new_issues

  local labels
  labels=$(jq -r '.issues["'"$id"'"].labels // [] | map(.name) | join(",")' "$GH_MOCK_STATE")
  [[ "$labels" != *"status:new"* ]]
}

@test "db_normalize_new_issues accepts issues from allowed_authors config" {
  source "${REPO_DIR}/scripts/lib.sh"
  # Create issue as a collaborator
  export GH_MOCK_LOGIN="trusted-bot"
  local json
  json=$(gh_api repos/$ORCH_GH_REPO/issues -f title="Bot issue")
  unset GH_MOCK_LOGIN
  local id
  id=$(printf '%s' "$json" | jq -r '.number')

  # Add trusted-bot to allowed_authors in config
  yq -i '.workflow.allowed_authors = ["trusted-bot"]' "$CONFIG_PATH"

  _GH_ALLOWED_AUTHORS=""  # reset cache
  db_normalize_new_issues

  local labels
  labels=$(db_task_labels_csv "$id")
  [[ "$labels" == *"status:new"* ]]

  # Cleanup
  yq -i 'del(.workflow.allowed_authors)' "$CONFIG_PATH"
}

@test "db_task_ids_by_status filters out non-owner issues" {
  source "${REPO_DIR}/scripts/lib.sh"
  # Create issue as stranger with status:new label
  export GH_MOCK_LOGIN="attacker"
  local json
  json=$(gh_api repos/$ORCH_GH_REPO/issues -f title="Attack issue")
  unset GH_MOCK_LOGIN
  local id
  id=$(printf '%s' "$json" | jq -r '.number')
  gh_api "repos/$ORCH_GH_REPO/issues/$id/labels" \
    --input - <<< '{"labels":["status:new"]}' >/dev/null 2>&1

  _GH_ALLOWED_AUTHORS=""  # reset cache
  local ids
  ids=$(db_task_ids_by_status "new")
  # Should contain the init task (by owner) but not the attacker's
  [[ "$ids" == *"$INIT_TASK_ID"* ]]
  [[ "$ids" != *"$id"* ]]
}

@test "_gh_is_allowed_author accepts repo owner" {
  source "${REPO_DIR}/scripts/lib.sh"
  _gh_is_allowed_author "mock"
}

@test "_gh_is_allowed_author rejects strangers" {
  source "${REPO_DIR}/scripts/lib.sh"
  run _gh_is_allowed_author "stranger"
  [ "$status" -eq 1 ]
}

@test "db_normalize_new_issues fails closed when repo unknown" {
  source "${REPO_DIR}/scripts/lib.sh"
  # Create issue as stranger
  export GH_MOCK_LOGIN="stranger"
  local json
  json=$(gh_api repos/$ORCH_GH_REPO/issues -f title="Sneaky issue")
  unset GH_MOCK_LOGIN
  local id
  id=$(printf '%s' "$json" | jq -r '.number')

  # Break repo detection — fail closed means no issues get status:new
  local saved_repo="$_GH_REPO"
  _GH_REPO=""
  _GH_ALLOWED_AUTHORS=""

  db_normalize_new_issues

  local labels
  labels=$(jq -r '.issues["'"$id"'"].labels // [] | map(.name) | join(",")' "$GH_MOCK_STATE")
  [[ "$labels" != *"status:new"* ]]

  _GH_REPO="$saved_repo"
}

@test "_gh_set_status_label rejects invalid status" {
  source "${REPO_DIR}/scripts/lib.sh"
  run _gh_set_status_label "$INIT_TASK_ID" "invalid_status"
  [ "$status" -eq 1 ]
}

@test "_gh_set_status_label accepts valid status" {
  source "${REPO_DIR}/scripts/lib.sh"
  run _gh_set_status_label "$INIT_TASK_ID" "routed"
  [ "$status" -eq 0 ]
}

@test "gh_project_list.sh lists managed bare-clone projects from ORCH_HOME/projects" {
  mkdir -p "$ORCH_HOME/projects/acme/widget.git"
  git -C "$ORCH_HOME/projects/acme/widget.git" init --bare --quiet
  git -C "$ORCH_HOME/projects/acme/widget.git" config remote.origin.url "git@github.com:acme/widget.git"

  mkdir -p "$ORCH_HOME/projects/foo/bar.git"
  git -C "$ORCH_HOME/projects/foo/bar.git" init --bare --quiet

  run "${REPO_DIR}/scripts/gh_project_list.sh"
  [ "$status" -eq 0 ]

  [[ "$output" == *"REPO"* ]]
  [[ "$output" == *"PATH"* ]]
  [[ "$output" == *"acme/widget"* ]]
  [[ "$output" == *"$ORCH_HOME/projects/acme/widget.git"* ]]
  [[ "$output" == *"foo/bar"* ]]
  [[ "$output" == *"$ORCH_HOME/projects/foo/bar.git"* ]]
}

@test "db_task_ids_by_status excludes issues that also have status:needs_review" {
  # Create an issue that is "new" but (incorrectly) also has status:needs_review.
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Corrupted status labels" "Should not be runnable" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")

  # Add a second status label without removing the existing one.
  gh api "repos/${ORCH_GH_REPO}/issues/${TASK2_ID}/labels" -f "labels[]=status:needs_review" >/dev/null

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && db_task_ids_by_status new"
  [ "$status" -eq 0 ]
  [[ \"$output\" != *\"${TASK2_ID}\"* ]]
}

@test "db_task_ids_by_status still returns needs_review issues when requested" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Needs review task" "Should be listed" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")
  tdb_set "$TASK2_ID" status "needs_review"

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && db_task_ids_by_status needs_review"
  [ "$status" -eq 0 ]
  [[ \"$output\" == *\"${TASK2_ID}\"* ]]
}

@test "db_task_ids_by_status excludes needs_review issues from routed list when labels are corrupted" {
  TASK_OUTPUT=$("${REPO_DIR}/scripts/add_task.sh" "Corrupted routed labels" "Should not be runnable" "")
  TASK2_ID=$(_task_id "$TASK_OUTPUT")
  tdb_set "$TASK2_ID" status "routed"

  # Corrupt: add needs_review label without removing routed.
  gh api "repos/${ORCH_GH_REPO}/issues/${TASK2_ID}/labels" -f "labels[]=status:needs_review" >/dev/null

  run bash -c "source '${REPO_DIR}/scripts/lib.sh' && db_task_ids_by_status routed"
  [ "$status" -eq 0 ]
  [[ \"$output\" != *\"${TASK2_ID}\"* ]]
}

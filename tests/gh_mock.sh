#!/usr/bin/env bash
# gh CLI mock for bats tests.
# Simulates GitHub REST & GraphQL API calls backed by a local JSON state file.
#
# Usage in bats tests:
#   1. Copy/symlink this file as "gh" into a directory early in PATH:
#        MOCK_BIN="${TMP_DIR}/mock_bin"; mkdir -p "$MOCK_BIN"
#        cp "${BATS_TEST_DIRNAME}/gh_mock.sh" "$MOCK_BIN/gh"
#        chmod +x "$MOCK_BIN/gh"
#        export PATH="${MOCK_BIN}:${PATH}"
#
#   2. Set required env vars:
#        export GH_MOCK_STATE="${STATE_DIR}/gh_mock_state.json"
#        export ORCH_GH_REPO="mock/repo"
#        export ORCH_BACKEND="github"
#
#   3. The mock auto-initialises on first call.  State persists in
#      $GH_MOCK_STATE between calls within the same test.
#
# State lives at $GH_MOCK_STATE (default: $STATE_DIR/gh_mock_state.json).
#
# Supported patterns:
#   gh api repos/O/R/issues              POST  — create issue
#   gh api repos/O/R/issues              GET   — list issues (with -f state=, -f labels=)
#   gh api repos/O/R/issues/N            GET   — get issue
#   gh api repos/O/R/issues/N            PATCH — update issue
#   gh api repos/O/R/issues/N/labels     POST  — add labels
#   gh api repos/O/R/issues/N/labels/L   DELETE — remove label
#   gh api repos/O/R/issues/N/comments   POST  — add comment
#   gh api repos/O/R/issues/N/comments   GET   — list comments
#   gh api repos/O/R/labels/NAME         GET   — check label exists
#   gh api repos/O/R/labels              POST  — create label
#   gh api search/issues                 GET   — search issues (with -f q=)
#   gh api graphql ... addSubIssue       — link sub-issue
#   gh api graphql ... subIssues         — list sub-issues
#   gh api graphql ...                   — other → {}
#   gh issue list                        — empty
#   gh pr view/list/create               — mock defaults
#   gh repo view                         — {"nameWithOwner":"mock/repo"}
#   gh auth setup-git                    — no-op
set -euo pipefail

# ---------------------------------------------------------------------------
# State file
# ---------------------------------------------------------------------------

GH_MOCK_STATE="${GH_MOCK_STATE:-${STATE_DIR:-.}/gh_mock_state.json}"

_state_read() {
  if [ -f "$GH_MOCK_STATE" ]; then
    cat "$GH_MOCK_STATE"
  else
    echo '{}'
  fi
}

_state_write() {
  local json="$1"
  printf '%s' "$json" > "$GH_MOCK_STATE"
}

_ensure_state() {
  if [ ! -f "$GH_MOCK_STATE" ]; then
    local dir
    dir=$(dirname "$GH_MOCK_STATE")
    mkdir -p "$dir"
    _state_write '{
  "issues": {},
  "next_issue": 1,
  "next_comment_id": 1,
  "comments": {},
  "labels": {},
  "sub_issues": {}
}'
  fi
}

# ---------------------------------------------------------------------------
# Timestamp helper
# ---------------------------------------------------------------------------

_now_iso() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

# ---------------------------------------------------------------------------
# Argument parser
# Parse -f key=value, -f key[]=value, -X METHOD, -q JQ, --input -, --cache T,
# -H header, positional endpoint, etc.
# ---------------------------------------------------------------------------

_parse_args() {
  # Outputs are set as globals (ugly but works in bash 3.2+)
  _PA_METHOD="GET"
  _PA_ENDPOINT=""
  _PA_JQ_QUERY=""
  _PA_STDIN_INPUT=""
  _PA_FIELDS=()       # "key=value" pairs
  _PA_ARRAY_FIELDS=() # "key[]=value" pairs
  _PA_HEADERS=()
  _PA_POSITIONAL=()

  while [ $# -gt 0 ]; do
    case "$1" in
      -X)
        _PA_METHOD="$2"; shift 2 ;;
      -f)
        local fval="$2"; shift 2
        if [[ "$fval" == *"[]="* ]]; then
          _PA_ARRAY_FIELDS+=("$fval")
        else
          _PA_FIELDS+=("$fval")
        fi
        ;;
      -q)
        _PA_JQ_QUERY="$2"; shift 2 ;;
      --input)
        if [ "$2" = "-" ]; then
          _PA_STDIN_INPUT=$(cat)
        fi
        shift 2
        ;;
      --cache)
        # Silently ignored
        shift 2 ;;
      -H)
        _PA_HEADERS+=("$2"); shift 2 ;;
      --json)
        # gh repo view --json field — just consume the arg
        shift 2 ;;
      --repo)
        shift 2 ;;
      --state)
        _PA_FIELDS+=("state=$2"); shift 2 ;;
      --search)
        _PA_FIELDS+=("search=$2"); shift 2 ;;
      --jq)
        _PA_JQ_QUERY="$2"; shift 2 ;;
      -*)
        # Unknown flag — skip with value if it looks like it takes one
        shift ;;
      *)
        _PA_POSITIONAL+=("$1"); shift ;;
    esac
  done

  # First positional is usually the endpoint (for `gh api`)
  if [ ${#_PA_POSITIONAL[@]} -gt 0 ]; then
    _PA_ENDPOINT="${_PA_POSITIONAL[0]}"
  fi
}

# Retrieve a field value from _PA_FIELDS by key.
_field_val() {
  local key="$1"
  for f in "${_PA_FIELDS[@]+"${_PA_FIELDS[@]}"}"; do
    local k="${f%%=*}"
    local v="${f#*=}"
    if [ "$k" = "$key" ]; then
      echo "$v"
      return
    fi
  done
}

# Collect all values for an array field key (e.g. "labels[]").
_array_field_vals() {
  local key="$1"
  for f in "${_PA_ARRAY_FIELDS[@]+"${_PA_ARRAY_FIELDS[@]}"}"; do
    local raw_key="${f%%=*}"
    local v="${f#*=}"
    if [ "$raw_key" = "${key}[]" ]; then
      echo "$v"
    fi
  done
}

# ---------------------------------------------------------------------------
# Apply jq query if present, otherwise pass through
# ---------------------------------------------------------------------------

_maybe_jq() {
  local json="$1"
  if [ -n "$_PA_JQ_QUERY" ]; then
    printf '%s' "$json" | jq -r "$_PA_JQ_QUERY" 2>/dev/null || printf '%s' "$json"
  else
    printf '%s' "$json"
  fi
}

# ---------------------------------------------------------------------------
# REST API handlers
# ---------------------------------------------------------------------------

# POST repos/OWNER/REPO/issues — create issue
_handle_create_issue() {
  _ensure_state
  local state
  state=$(_state_read)

  local title body
  title=$(_field_val "title")
  body=$(_field_val "body")

  # Collect labels from array fields
  local labels_json='[]'
  local label_line
  while IFS= read -r label_line; do
    [ -z "$label_line" ] && continue
    labels_json=$(printf '%s' "$labels_json" | jq -c --arg l "$label_line" '. + [{"name": $l, "color": "c5def5"}]')
  done <<< "$(_array_field_vals "labels")"

  local num
  num=$(printf '%s' "$state" | jq -r '.next_issue')
  local now
  now=$(_now_iso)
  local node_id="MOCK_NODE_${num}"
  local html_url="https://github.com/mock/repo/issues/${num}"

  local login="${GH_MOCK_LOGIN:-mock}"

  local issue
  issue=$(jq -nc \
    --argjson num "$num" \
    --arg title "${title:-}" \
    --arg body "${body:-}" \
    --argjson labels "$labels_json" \
    --arg now "$now" \
    --arg node_id "$node_id" \
    --arg html_url "$html_url" \
    --arg login "$login" \
    '{
      number: $num,
      title: $title,
      body: $body,
      state: "open",
      labels: $labels,
      created_at: $now,
      updated_at: $now,
      html_url: $html_url,
      node_id: $node_id,
      user: {login: $login},
      pull_request: null
    }')

  # Update state
  state=$(printf '%s' "$state" | jq -c \
    --argjson num "$num" \
    --argjson issue "$issue" \
    '.issues[($num | tostring)] = $issue | .next_issue = ($num + 1)')

  # Ensure empty comments array for this issue
  state=$(printf '%s' "$state" | jq -c \
    --argjson num "$num" \
    '.comments[($num | tostring)] //= []')

  _state_write "$state"
  _maybe_jq "$issue"
}

# GET repos/OWNER/REPO/issues — list issues
_handle_list_issues() {
  _ensure_state
  local state
  state=$(_state_read)

  local filter_state
  filter_state=$(_field_val "state")
  filter_state="${filter_state:-open}"

  local filter_labels
  filter_labels=$(_field_val "labels")

  local sort_field
  sort_field=$(_field_val "sort")
  sort_field="${sort_field:-created}"

  local direction
  direction=$(_field_val "direction")
  direction="${direction:-desc}"

  # Build the jq filter
  local jq_filter='[.issues | to_entries[].value'

  # State filter
  if [ "$filter_state" = "all" ]; then
    jq_filter+=' | select(true)'
  elif [ "$filter_state" = "closed" ]; then
    jq_filter+=' | select(.state == "closed")'
  else
    jq_filter+=' | select(.state == "open")'
  fi

  # Label filter
  if [ -n "$filter_labels" ]; then
    # May be comma-separated
    jq_filter+=" | select((.labels // []) | map(.name) | any(. == \"$filter_labels\"))"
  fi

  jq_filter+=']'

  # Sort
  if [ "$sort_field" = "updated" ]; then
    jq_filter+=' | sort_by(.updated_at)'
  else
    jq_filter+=' | sort_by(.number)'
  fi

  if [ "$direction" = "desc" ]; then
    jq_filter+=' | reverse'
  fi

  local result
  result=$(printf '%s' "$state" | jq -c "$jq_filter" 2>/dev/null || echo '[]')
  _maybe_jq "$result"
}

# GET repos/OWNER/REPO/issues/N — get single issue
_handle_get_issue() {
  local num="$1"
  _ensure_state
  local state
  state=$(_state_read)

  local issue
  issue=$(printf '%s' "$state" | jq -c --arg n "$num" '.issues[$n] // empty')
  if [ -z "$issue" ] || [ "$issue" = "null" ]; then
    echo '{"message":"Not Found"}' >&2
    return 1
  fi
  _maybe_jq "$issue"
}

# PATCH repos/OWNER/REPO/issues/N — update issue
_handle_update_issue() {
  local num="$1"
  _ensure_state
  local state
  state=$(_state_read)

  local title body patch_state labels_input
  title=$(_field_val "title")
  body=$(_field_val "body")
  patch_state=$(_field_val "state")

  local now
  now=$(_now_iso)

  # Build patch JSON
  local patch='{}'
  [ -n "$title" ] && patch=$(printf '%s' "$patch" | jq -c --arg t "$title" '.title = $t')
  [ -n "$body" ] && patch=$(printf '%s' "$patch" | jq -c --arg b "$body" '.body = $b')
  [ -n "$patch_state" ] && patch=$(printf '%s' "$patch" | jq -c --arg s "$patch_state" '.state = $s')
  patch=$(printf '%s' "$patch" | jq -c --arg now "$now" '.updated_at = $now')

  # Handle --input - for full JSON patch (used for labels replacement)
  if [ -n "$_PA_STDIN_INPUT" ]; then
    local stdin_labels
    stdin_labels=$(printf '%s' "$_PA_STDIN_INPUT" | jq -c '.labels // empty' 2>/dev/null || true)
    if [ -n "$stdin_labels" ] && [ "$stdin_labels" != "null" ]; then
      # Convert string labels to objects
      local label_objects
      label_objects=$(printf '%s' "$stdin_labels" | jq -c '[.[] | if type == "string" then {"name": ., "color": "c5def5"} else . end]')
      patch=$(printf '%s' "$patch" | jq -c --argjson l "$label_objects" '.labels = $l')
    fi
  fi

  # Merge the patch into existing issue
  state=$(printf '%s' "$state" | jq -c \
    --arg n "$num" \
    --argjson patch "$patch" \
    'if .issues[$n] then .issues[$n] = (.issues[$n] * $patch) else . end')

  _state_write "$state"

  local issue
  issue=$(printf '%s' "$state" | jq -c --arg n "$num" '.issues[$n]')
  _maybe_jq "$issue"
}

# POST repos/OWNER/REPO/issues/N/labels — add labels
_handle_add_labels() {
  local num="$1"
  _ensure_state
  local state
  state=$(_state_read)

  local now
  now=$(_now_iso)

  # Labels can come from --input - or -f fields
  local new_labels='[]'
  if [ -n "$_PA_STDIN_INPUT" ]; then
    local stdin_labels
    stdin_labels=$(printf '%s' "$_PA_STDIN_INPUT" | jq -c '.labels // []' 2>/dev/null || echo '[]')
    new_labels="$stdin_labels"
  fi

  # Also collect from -f labels[]=... or -f "labels[]=..."
  local label_line
  while IFS= read -r label_line; do
    [ -z "$label_line" ] && continue
    new_labels=$(printf '%s' "$new_labels" | jq -c --arg l "$label_line" '. + [$l]')
  done <<< "$(_array_field_vals "labels")"

  # Convert to objects and merge into existing labels
  state=$(printf '%s' "$state" | jq -c \
    --arg n "$num" \
    --argjson nl "$new_labels" \
    --arg now "$now" \
    'if .issues[$n] then
      .issues[$n].labels = (
        (.issues[$n].labels // []) +
        [$nl[] | if type == "string" then {"name": ., "color": "c5def5"} else . end]
        | unique_by(.name)
      )
      | .issues[$n].updated_at = $now
    else . end')

  _state_write "$state"

  local labels
  labels=$(printf '%s' "$state" | jq -c --arg n "$num" '.issues[$n].labels // []')
  _maybe_jq "$labels"
}

# DELETE repos/OWNER/REPO/issues/N/labels/LABEL — remove label
_handle_remove_label() {
  local num="$1" label="$2"
  _ensure_state
  local state
  state=$(_state_read)

  local now
  now=$(_now_iso)

  # URL-decode the label name (handle %3A etc.)
  local decoded_label
  decoded_label=$(printf '%s' "$label" | python3 -c "import sys, urllib.parse; print(urllib.parse.unquote(sys.stdin.read().strip()))" 2>/dev/null || echo "$label")

  state=$(printf '%s' "$state" | jq -c \
    --arg n "$num" \
    --arg lbl "$decoded_label" \
    --arg now "$now" \
    'if .issues[$n] then
      .issues[$n].labels = [.issues[$n].labels[] | select(.name != $lbl)]
      | .issues[$n].updated_at = $now
    else . end')

  _state_write "$state"
}

# POST repos/OWNER/REPO/issues/N/comments — add comment
_handle_add_comment() {
  local owner="$1" repo="$2" num="$3"
  _ensure_state
  local state
  state=$(_state_read)

  local body
  body=$(_field_val "body")
  local now
  now=$(_now_iso)
  local login
  login="${GH_MOCK_LOGIN:-mock-user}"

  local comment_id
  comment_id=$(printf '%s' "$state" | jq -r '.next_comment_id')

  local comment
  comment=$(jq -nc \
    --argjson id "$comment_id" \
    --arg body "${body:-}" \
    --arg now "$now" \
    --arg login "$login" \
    --arg owner "$owner" --arg repo "$repo" --arg num "$num" \
    '{id: $id, body: $body, created_at: $now,
      user: {login: $login},
      issue_url: ("https://api.github.com/repos/" + $owner + "/" + $repo + "/issues/" + $num),
      html_url: ("https://github.com/" + $owner + "/" + $repo + "/issues/" + $num + "#issuecomment-" + ($id|tostring))
    }')

  state=$(printf '%s' "$state" | jq -c \
    --arg n "$num" \
    --argjson c "$comment" \
    --argjson cid "$comment_id" \
    '.comments[$n] = ((.comments[$n] // []) + [$c]) | .next_comment_id = ($cid + 1)')

  # Also bump issue updated_at
  state=$(printf '%s' "$state" | jq -c \
    --arg n "$num" \
    --arg now "$now" \
    'if .issues[$n] then .issues[$n].updated_at = $now else . end')

  _state_write "$state"
  _maybe_jq "$comment"
}

# GET repos/OWNER/REPO/issues/N/comments — list comments
_handle_list_comments() {
  local owner="$1" repo="$2" num="$3"
  _ensure_state
  local state
  state=$(_state_read)

  local comments
  comments=$(printf '%s' "$state" | jq -c --arg n "$num" --arg owner "$owner" --arg repo "$repo" '
    (.comments[$n] // [])
    | map(
        . + {
          user: (.user // {login: "mock-user"}),
          issue_url: (.issue_url // ("https://api.github.com/repos/" + $owner + "/" + $repo + "/issues/" + $n)),
          html_url: (.html_url // ("https://github.com/" + $owner + "/" + $repo + "/issues/" + $n + "#issuecomment-" + (.id|tostring)))
        }
      )')
  _maybe_jq "$comments"
}

# GET repos/OWNER/REPO/issues/comments — list all issue/PR comments
_handle_list_repo_issue_comments() {
  local owner="$1" repo="$2"
  _ensure_state
  local state
  state=$(_state_read)

  local since
  since=$(_field_val "since")
  local since_filter='.'
  if [ -n "$since" ]; then
    since_filter="map(select((.created_at // \"\") > \$since))"
  fi

  local comments
  comments=$(printf '%s' "$state" | jq -c --arg owner "$owner" --arg repo "$repo" --arg since "${since:-}" "
    [
      (.comments // {} | to_entries[]? | . as \$e
        | (\$e.value // [])[]
        | . + {
            user: (.user // {login: \"mock-user\"}),
            issue_url: (.issue_url // (\"https://api.github.com/repos/\" + \$owner + \"/\" + \$repo + \"/issues/\" + \$e.key)),
            html_url: (.html_url // (\"https://github.com/\" + \$owner + \"/\" + \$repo + \"/issues/\" + \$e.key + \"#issuecomment-\" + (.id|tostring)))
          }
      )
    ]
    | ${since_filter}
  " 2>/dev/null || echo "[]")
  _maybe_jq "$comments"
}

# GET repos/OWNER/REPO/labels/NAME — check label exists
_handle_get_label() {
  local name="$1"
  _ensure_state
  local state
  state=$(_state_read)

  # URL-decode the label name
  local decoded_name
  decoded_name=$(printf '%s' "$name" | python3 -c "import sys, urllib.parse; print(urllib.parse.unquote(sys.stdin.read().strip()))" 2>/dev/null || echo "$name")

  local label
  label=$(printf '%s' "$state" | jq -c --arg n "$decoded_name" '.labels[$n] // empty')
  if [ -z "$label" ] || [ "$label" = "null" ]; then
    return 1
  fi
  _maybe_jq "$label"
}

# POST repos/OWNER/REPO/labels — create label
_handle_create_label() {
  _ensure_state
  local state
  state=$(_state_read)

  local name color description
  name=$(_field_val "name")
  color=$(_field_val "color")
  description=$(_field_val "description")

  local label
  label=$(jq -nc \
    --arg name "${name:-}" \
    --arg color "${color:-ededed}" \
    --arg desc "${description:-}" \
    '{name: $name, color: $color, description: $desc}')

  state=$(printf '%s' "$state" | jq -c \
    --arg name "${name:-}" \
    --argjson label "$label" \
    '.labels[$name] = $label')

  _state_write "$state"
  _maybe_jq "$label"
}

# GET search/issues — search issues
_handle_search_issues() {
  _ensure_state
  local state
  state=$(_state_read)

  local query
  query=$(_field_val "q")

  # Parse search query for: label:X, is:open/closed, repo:X, is:issue
  local is_open="" is_closed="" label_filter=""

  if printf '%s' "$query" | grep -q 'is:open'; then
    is_open=true
  fi
  if printf '%s' "$query" | grep -q 'is:closed'; then
    is_closed=true
  fi

  # Extract label:X (may have multiple)
  local label_filters=()
  while IFS= read -r lf; do
    [ -n "$lf" ] && label_filters+=("$lf")
  done < <(printf '%s' "$query" | grep -oE 'label:[^ ]+' | sed 's/^label://')

  # Start with all issues
  local jq_filter='[.issues | to_entries[].value'

  # State filter
  if [ "$is_open" = "true" ] && [ "$is_closed" != "true" ]; then
    jq_filter+=' | select(.state == "open")'
  elif [ "$is_closed" = "true" ] && [ "$is_open" != "true" ]; then
    jq_filter+=' | select(.state == "closed")'
  fi
  # If both or neither, include all

  # Pull request filter (is:issue → exclude PRs)
  jq_filter+=' | select(.pull_request == null)'

  # Label filters
  for lf in "${label_filters[@]+"${label_filters[@]}"}"; do
    jq_filter+=" | select((.labels // []) | map(.name) | any(. == \"$lf\"))"
  done

  jq_filter+=']'

  local items
  items=$(printf '%s' "$state" | jq -c "$jq_filter" 2>/dev/null || echo '[]')
  local count
  count=$(printf '%s' "$items" | jq 'length')

  local result
  result=$(jq -nc --argjson count "$count" --argjson items "$items" \
    '{total_count: $count, incomplete_results: false, items: $items}')

  _maybe_jq "$result"
}

# ---------------------------------------------------------------------------
# GraphQL handler
# ---------------------------------------------------------------------------

_handle_graphql() {
  _ensure_state
  local state
  state=$(_state_read)

  local query_str=""
  local parent_issue_id="" child_issue_id="" parent_id=""

  for f in "${_PA_FIELDS[@]+"${_PA_FIELDS[@]}"}"; do
    local k="${f%%=*}"
    local v="${f#*=}"
    case "$k" in
      query)          query_str="$v" ;;
      parentIssueId)  parent_issue_id="$v" ;;
      childIssueId)   child_issue_id="$v" ;;
      parentId)       parent_id="$v" ;;
    esac
  done

  # addSubIssue mutation
  if printf '%s' "$query_str" | grep -q 'addSubIssue'; then
    if [ -n "$parent_issue_id" ] && [ -n "$child_issue_id" ]; then
      # Extract issue numbers from node IDs (MOCK_NODE_N)
      local parent_num child_num
      parent_num=$(printf '%s' "$parent_issue_id" | sed 's/MOCK_NODE_//')
      child_num=$(printf '%s' "$child_issue_id" | sed 's/MOCK_NODE_//')

      state=$(printf '%s' "$state" | jq -c \
        --arg p "$parent_num" \
        --argjson c "$child_num" \
        '.sub_issues[$p] = ((.sub_issues[$p] // []) + [$c] | unique)')
      _state_write "$state"

      printf '{"data":{"addSubIssue":{"issue":{"number":%s},"subIssue":{"number":%s}}}}' \
        "$parent_num" "$child_num"
    else
      echo '{}'
    fi
    return
  fi

  # subIssues query
  if printf '%s' "$query_str" | grep -q 'subIssues'; then
    if [ -n "$parent_id" ]; then
      local parent_num
      parent_num=$(printf '%s' "$parent_id" | sed 's/MOCK_NODE_//')

      local children
      children=$(printf '%s' "$state" | jq -c --arg p "$parent_num" \
        '[(.sub_issues[$p] // [])[] | {number: .}]')

      printf '{"data":{"node":{"subIssues":{"nodes":%s}}}}' "$children"
    else
      echo '{"data":{"node":{"subIssues":{"nodes":[]}}}}'
    fi
    return
  fi

  # Default: return empty for project V2 and other graphql queries
  echo '{}'
}

# ---------------------------------------------------------------------------
# gh issue list handler
# ---------------------------------------------------------------------------

_handle_issue_list() {
  # gh issue list --repo REPO --state STATE --search "..." ...
  # Return empty by default
  echo ""
}

# ---------------------------------------------------------------------------
# gh pr handlers
# ---------------------------------------------------------------------------

_handle_pr_view() {
  _ensure_state
  local state
  state=$(_state_read)
  local prs
  prs=$(printf '%s' "$state" | jq -c '.prs // {}')
  if [ "$prs" != "{}" ] && [ -n "$prs" ]; then
    # Return first PR from state
    local pr
    pr=$(printf '%s' "$prs" | jq -c 'to_entries[0].value // empty' 2>/dev/null || true)
    if [ -n "$pr" ]; then
      _maybe_jq "$pr"
      return
    fi
  fi
  echo '{"state":"OPEN","number":42,"url":"https://github.com/mock/repo/pull/42"}'
}

_handle_pr_list() {
  _ensure_state
  local state
  state=$(_state_read)
  local prs
  prs=$(printf '%s' "$state" | jq -c '[.prs // {} | to_entries[].value]' 2>/dev/null || echo '[]')
  _maybe_jq "$prs"
}

_handle_pr_diff() {
  _ensure_state
  local state
  state=$(_state_read)
  local diff
  diff=$(printf '%s' "$state" | jq -r '.pr_diff // ""' 2>/dev/null || true)
  if [ -n "$diff" ]; then
    echo "$diff"
  else
    echo ""
  fi
}

_handle_pr_review() {
  echo "Approved"
}

_handle_pr_create() {
  echo 'https://github.com/mock/repo/pull/99'
}

# ---------------------------------------------------------------------------
# gh repo view handler
# ---------------------------------------------------------------------------

_handle_repo_view() {
  echo '{"nameWithOwner":"mock/repo"}'
}

# ---------------------------------------------------------------------------
# Main dispatch — invoked as `gh <subcommand> ...`
# ---------------------------------------------------------------------------

main() {
  local subcmd="${1:-}"
  shift || true

  case "$subcmd" in
    auth)
      # gh auth setup-git — no-op
      ;;

    api)
      _parse_args "$@"

      # GraphQL?
      if [ "$_PA_ENDPOINT" = "graphql" ]; then
        _handle_graphql
        return
      fi

      # Normalise endpoint: strip leading /
      _PA_ENDPOINT="${_PA_ENDPOINT#/}"

      # Determine effective method.
      # Real `gh api` infers POST when -f/--input fields are present and no
      # explicit -X is given.  We replicate that: if _PA_METHOD was never
      # explicitly set (still the default "GET") and there are -f or
      # --input fields, treat it as POST — UNLESS the caller explicitly
      # passed -X GET (which we detect by checking our own flag).
      local method="$_PA_METHOD"
      local _explicit_get=false
      for _a in "$@"; do
        if [ "$_a" = "-X" ]; then _explicit_get=true; fi
        # reset if the *value* after -X is not GET
        if [ "$_explicit_get" = "true" ] && [ "$_a" != "-X" ]; then
          if [ "$_a" = "GET" ]; then _explicit_get=true; else _explicit_get=false; fi
          break
        fi
      done

      if [ "$method" = "GET" ] && [ "$_explicit_get" != "true" ]; then
        if [ ${#_PA_FIELDS[@]} -gt 0 ] || [ ${#_PA_ARRAY_FIELDS[@]} -gt 0 ] || [ -n "$_PA_STDIN_INPUT" ]; then
          method="POST"
        fi
      fi

      # Route by endpoint pattern
      # repos/OWNER/REPO/issues/N/labels/LABEL
      if [[ "$_PA_ENDPOINT" =~ ^repos/[^/]+/[^/]+/issues/([0-9]+)/labels/(.+)$ ]]; then
        local issue_num="${BASH_REMATCH[1]}"
        local label_name="${BASH_REMATCH[2]}"
        if [ "$method" = "DELETE" ]; then
          _handle_remove_label "$issue_num" "$label_name"
        fi
        return
      fi

      # repos/OWNER/REPO/issues/N/labels
      if [[ "$_PA_ENDPOINT" =~ ^repos/[^/]+/[^/]+/issues/([0-9]+)/labels$ ]]; then
        local issue_num="${BASH_REMATCH[1]}"
        _handle_add_labels "$issue_num"
        return
      fi

      # repos/OWNER/REPO/issues/comments — list repo-wide issue/PR comments
      if [[ "$_PA_ENDPOINT" =~ ^repos/([^/]+)/([^/]+)/issues/comments$ ]]; then
        local owner="${BASH_REMATCH[1]}"
        local repo="${BASH_REMATCH[2]}"
        _handle_list_repo_issue_comments "$owner" "$repo"
        return
      fi

      # repos/OWNER/REPO/issues/N/comments
      if [[ "$_PA_ENDPOINT" =~ ^repos/([^/]+)/([^/]+)/issues/([0-9]+)/comments$ ]]; then
        local owner="${BASH_REMATCH[1]}"
        local repo="${BASH_REMATCH[2]}"
        local issue_num="${BASH_REMATCH[3]}"
        if [ "$method" = "GET" ]; then
          _handle_list_comments "$owner" "$repo" "$issue_num"
        else
          _handle_add_comment "$owner" "$repo" "$issue_num"
        fi
        return
      fi

      # repos/OWNER/REPO/issues/N (single issue)
      if [[ "$_PA_ENDPOINT" =~ ^repos/[^/]+/[^/]+/issues/([0-9]+)$ ]]; then
        local issue_num="${BASH_REMATCH[1]}"
        if [ "$method" = "PATCH" ]; then
          _handle_update_issue "$issue_num"
        else
          _handle_get_issue "$issue_num"
        fi
        return
      fi

      # repos/OWNER/REPO/issues (list or create)
      if [[ "$_PA_ENDPOINT" =~ ^repos/[^/]+/[^/]+/issues$ ]]; then
        if [ "$method" = "POST" ]; then
          _handle_create_issue
        else
          _handle_list_issues
        fi
        return
      fi

      # repos/OWNER/REPO/labels/NAME (get label)
      if [[ "$_PA_ENDPOINT" =~ ^repos/[^/]+/[^/]+/labels/(.+)$ ]]; then
        local label_name="${BASH_REMATCH[1]}"
        _handle_get_label "$label_name"
        return
      fi

      # repos/OWNER/REPO/labels (create label)
      if [[ "$_PA_ENDPOINT" =~ ^repos/[^/]+/[^/]+/labels$ ]]; then
        if [ "$method" = "POST" ]; then
          _handle_create_label
        fi
        return
      fi

      # search/issues
      if [[ "$_PA_ENDPOINT" =~ ^search/issues$ ]]; then
        _handle_search_issues
        return
      fi

      # Unmatched endpoint — return empty JSON
      echo '{}'
      ;;

    issue)
      # gh issue list ...
      local issue_subcmd="${1:-}"
      shift || true
      case "$issue_subcmd" in
        list) _handle_issue_list ;;
        *)    echo "" ;;
      esac
      ;;

    pr)
      local pr_subcmd="${1:-}"
      shift || true
      # Parse remaining args for --jq, --json, etc.
      _parse_args "$@"
      case "$pr_subcmd" in
        view)   _handle_pr_view ;;
        list)   _handle_pr_list ;;
        diff)   _handle_pr_diff ;;
        review) _handle_pr_review ;;
        create) _handle_pr_create ;;
        *)      echo "" ;;
      esac
      ;;

    repo)
      local repo_subcmd="${1:-}"
      shift || true
      case "$repo_subcmd" in
        view) _handle_repo_view ;;
        *)    echo "" ;;
      esac
      ;;

    *)
      # Unknown subcommand — no-op
      echo ""
      ;;
  esac
}

main "$@"

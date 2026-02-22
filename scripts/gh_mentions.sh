#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=scripts/lib.sh
source "$(dirname "$0")/lib.sh"

# Ensure PROJECT_DIR is set before loading project config (lint + correct state paths)
PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"

load_project_config

MENTIONS_MARKER="<!-- orch:mention -->"

mentions_db_init() {
  local path="${MENTIONS_DB_PATH:-}"
  [ -n "$path" ] || return 1
  mkdir -p "$(dirname "$path")"
  if [ ! -f "$path" ]; then
    printf '%s\n' '{"since":"1970-01-01T00:00:00Z","processed":{}}' >"$path"
  fi
}

mentions_db_since() {
  local path="${MENTIONS_DB_PATH:-}"
  [ -n "$path" ] || { echo "1970-01-01T00:00:00Z"; return 0; }
  jq -r '.since // "1970-01-01T00:00:00Z"' "$path" 2>/dev/null || echo "1970-01-01T00:00:00Z"
}

mentions_db_lookup_task() {
  local comment_id="$1"
  local path="${MENTIONS_DB_PATH:-}"
  [ -n "$path" ] || return 0
  jq -r --arg cid "$comment_id" '.processed[$cid].task_id // empty' "$path" 2>/dev/null || true
}

mentions_db_store() {
  local comment_id="$1" task_id="$2" issue_number="$3" created_at="$4" comment_url="$5"
  local path tmp
  path="${MENTIONS_DB_PATH:-}"
  [ -n "$path" ] || return 1
  tmp=$(mktemp)
  jq -c \
    --arg cid "$comment_id" \
    --argjson tid "$task_id" \
    --argjson inum "$issue_number" \
    --arg ca "$created_at" \
    --arg url "$comment_url" \
    '.processed[$cid] = {task_id: $tid, issue_number: $inum, created_at: $ca, comment_url: $url}' \
    "$path" >"$tmp" 2>/dev/null || { rm -f "$tmp"; return 1; }
  mv "$tmp" "$path"
}

mentions_db_set_since() {
  local since="$1"
  local path tmp
  path="${MENTIONS_DB_PATH:-}"
  [ -n "$path" ] || return 1
  tmp=$(mktemp)
  jq -c --arg s "$since" '.since = $s' "$path" >"$tmp" 2>/dev/null || { rm -f "$tmp"; return 1; }
  mv "$tmp" "$path"
}

acquire_mentions_lock() {
  local lock_dir="${MENTIONS_LOCK_DIR:-}"
  [ -n "$lock_dir" ] || return 1
  mkdir -p "$(dirname "$lock_dir")"
  if mkdir "$lock_dir" 2>/dev/null; then
    trap "rmdir \"$lock_dir\" 2>/dev/null || true" EXIT
    return 0
  fi
  return 1
}

mention_actionable() {
  local body="${1:-}"
  [ -n "$body" ] || return 1

  # Never act on orchestrator-generated comments
  if printf '%s' "$body" | rg -qi "via \\[Orchestrator\\]|<!--\\s*orch:"; then
    return 1
  fi

  # Skip agent result comments that reference @orchestrator as a noun, not a command.
  # These are comments like "Results from task #237 (@orchestrator mention handler)"
  # or "Starting on task #250" that describe orchestrator work rather than requesting it.
  if printf '%s' "$body" | rg -qi '(results|summary|follow.up|starting\s+on)\s+.*task\s*#[0-9]+'; then
    return 1
  fi

  # Skip comments that only mention @orchestrator inside parenthetical references
  # e.g. "(@orchestrator mention handler)" or "the @orchestrator mention"
  local stripped
  stripped=$(printf '%s' "$body" | sed -E 's/\([^)]*@orchestrator[^)]*\)//gi; s/@orchestrator\s+mention[^[:space:]]*/orchestrator-mention/gi')
  if ! printf '%s' "$stripped" | rg -qi '@orchestrator'; then
    return 1
  fi

  # Skip fenced code blocks and markdown blockquotes; detect @orchestrator in remaining text.
  # - Fences: ``` or ~~~
  # - Quotes: lines starting with optional whitespace then >
  printf '%s\n' "$body" | awk '
    BEGIN { in_fence = 0; hit = 0; }
    /^[[:space:]]*(```|~~~)/ { in_fence = !in_fence; next; }
    in_fence == 1 { next; }
    /^[[:space:]]*>/ { next; }
    {
      line = $0

      # Remove inline code and quoted substrings to reduce false positives from
      # status updates that merely reference an @orchestrator mention.
      # - Inline code: `...`
      # - Double quotes: "..."
      # - Single quotes: '...'
      while (match(line, /`[^`]*`/)) {
        line = substr(line, 1, RSTART - 1) substr(line, RSTART + RLENGTH)
      }
      while (match(line, /"[^"]*"/)) {
        line = substr(line, 1, RSTART - 1) substr(line, RSTART + RLENGTH)
      }
      while (match(line, /\047[^\047]*\047/)) {
        line = substr(line, 1, RSTART - 1) substr(line, RSTART + RLENGTH)
      }
      if (tolower(line) ~ /(^|[^a-z0-9_])@orchestrator([^a-z0-9_-]|$)/) { hit = 1; exit; }
    }
    END { exit(hit ? 0 : 1); }
  ' >/dev/null 2>&1
}

build_task_body() {
  local repo="$1" issue_number="$2" author="$3" created_at="$4" comment_url="$5" comment_body="$6"
  cat <<EOF
This task was created from an @orchestrator mention.

- **Repo:** \`$repo\`
- **Target:** #$issue_number
- **Author:** @$author
- **Comment:** $comment_url
- **Created:** $created_at

### Mention Body

\`\`\`markdown
$comment_body
\`\`\`

### Instructions

Respond back on #$issue_number with your results and next steps.

**IMPORTANT:** Do NOT use @orchestrator in your response comments — it will trigger the mention handler again and create an infinite loop. Write "orchestrator" without the @ prefix.
EOF
}

main() {
  if ! command -v gh >/dev/null 2>&1; then
    return 0
  fi

  local repo
  repo=$(config_get '.gh.repo // ""' 2>/dev/null || true)
  repo="${repo:-${ORCH_GH_REPO:-}}"
  [ -n "$repo" ] && [ "$repo" != "null" ] || return 0

  local repo_key mentions_dir
  repo_key=$(printf '%s' "$repo" | tr '/:' '__')
  mentions_dir="${ORCH_HOME}/.orchestrator/mentions"
  MENTIONS_DB_PATH="${mentions_dir}/${repo_key}.json"
  MENTIONS_LOCK_DIR="${mentions_dir}/${repo_key}.lock"

  acquire_mentions_lock || return 0

  mentions_db_init
  local since
  since=$(mentions_db_since)

  local comments_json
  comments_json=$(gh_api -X GET "repos/${repo}/issues/comments" -f since="$since" -f per_page=100 2>/dev/null || echo "[]")
  local count
  count=$(printf '%s' "$comments_json" | jq -r 'length' 2>/dev/null || echo 0)
  [ "$count" -gt 0 ] || return 0

  local max_seen="$since"

  for i in $(seq 0 $((count - 1))); do
    local comment_id issue_url issue_number comment_body created_at updated_at author comment_url
    comment_id=$(printf '%s' "$comments_json" | jq -r ".[$i].id // empty" 2>/dev/null || true)
    comment_body=$(printf '%s' "$comments_json" | jq -r ".[$i].body // \"\"" 2>/dev/null || true)
    created_at=$(printf '%s' "$comments_json" | jq -r ".[$i].created_at // \"\"" 2>/dev/null || true)
    updated_at=$(printf '%s' "$comments_json" | jq -r ".[$i].updated_at // \"\"" 2>/dev/null || true)
    issue_url=$(printf '%s' "$comments_json" | jq -r ".[$i].issue_url // \"\"" 2>/dev/null || true)
    author=$(printf '%s' "$comments_json" | jq -r ".[$i].user.login // \"\"" 2>/dev/null || true)
    comment_url=$(printf '%s' "$comments_json" | jq -r ".[$i].html_url // \"\"" 2>/dev/null || true)

    [ -n "$comment_id" ] || continue
    [ -n "$issue_url" ] || continue

    issue_number=$(printf '%s' "$issue_url" | sed -E 's#.*/issues/([0-9]+).*#\1#')
    [ -n "$issue_number" ] || continue

    local seen_at
    seen_at="${updated_at:-$created_at}"
    if [ -n "$seen_at" ] && [ "$seen_at" \> "$max_seen" ]; then
      max_seen="$seen_at"
    fi

    # Skip comments on closed issues — no point creating tasks for them
    local _issue_state
    _issue_state=$(gh_api "repos/${repo}/issues/${issue_number}" --cache 120s -q '.state' 2>/dev/null || true)
    if [ "$_issue_state" = "closed" ]; then
      continue
    fi

    if ! printf '%s' "$comment_body" | rg -qi "@orchestrator"; then
      continue
    fi

    if ! mention_actionable "$comment_body"; then
      continue
    fi

    local existing
    existing=$(mentions_db_lookup_task "$comment_id")
    if [ -n "$existing" ]; then
      continue
    fi

    # Per-issue dedup: skip if there's already an active (non-done) mention task for this issue.
    # This prevents infinite loops where agent responses trigger new mention tasks.
    local _has_active=false
    local _active_tasks
    _active_tasks=$(jq -r --argjson inum "$issue_number" \
      '[.processed | to_entries[] | select(.value.issue_number == $inum) | .value.task_id] | .[]' \
      "$MENTIONS_DB_PATH" 2>/dev/null || true)

    # If prior tasks exist for this issue, re-verify issue state without cache.
    # The initial check uses --cache 120s; a stale "open" response could let a
    # comment through on an already-closed issue. Rechecking here ensures that
    # done tasks on closed issues don't re-trigger new task creation.
    if [ -n "$_active_tasks" ]; then
      local _fresh_state
      _fresh_state=$(gh_api "repos/${repo}/issues/${issue_number}" -q '.state' 2>/dev/null || true)
      if [ "$_fresh_state" = "closed" ]; then
        log_err "[mentions] skipping issue #$issue_number — issue is closed, prior mention tasks exist"
        continue
      fi
    fi

    for _tid in $_active_tasks; do
      local _st
      _st=$(db_task_field "$_tid" "status" 2>/dev/null || true)
      if [ -n "$_st" ] && [ "$_st" != "done" ] && [ "$_st" != "null" ]; then
        _has_active=true
        break
      fi
    done
    if [ "$_has_active" = true ]; then
      log_err "[mentions] skipping issue #$issue_number — active mention task #$_tid exists"
      continue
    fi

    local title task_body task_id
    title="Respond to @orchestrator mention in #${issue_number}"
    task_body=$(build_task_body "$repo" "$issue_number" "${author:-unknown}" "${created_at:-}" "${comment_url:-$issue_url}" "$comment_body")
    task_id=$(db_create_task "$title" "$task_body" "${PROJECT_DIR:-}" "mention" "" "" 2>/dev/null || true)
    [ -n "$task_id" ] || continue

    _sidecar_write "$task_id" "mention_target_repo" "$repo"
    _sidecar_write "$task_id" "mention_target_issue" "$issue_number"
    _sidecar_write "$task_id" "mention_source_comment_id" "$comment_id"
    [ -n "$comment_url" ] && _sidecar_write "$task_id" "mention_source_comment_url" "$comment_url"
    [ -n "$author" ] && _sidecar_write "$task_id" "mention_source_author" "$author"

    local ack_body
    ack_body="${MENTIONS_MARKER}

Acknowledged. Created task #${task_id} from this @orchestrator mention.

---
*via [Orchestrator](https://github.com/gabrielkoerich/orchestrator)*"
    gh_api "repos/${repo}/issues/${issue_number}/comments" -f body="$ack_body" >/dev/null 2>&1 || true

    mentions_db_store "$comment_id" "$task_id" "$issue_number" "${created_at:-}" "${comment_url:-}" || true
  done

  [ -n "$max_seen" ] && mentions_db_set_since "$max_seen" || true
}

main "$@"

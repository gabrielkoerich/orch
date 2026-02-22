#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"
require_jq
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"
export PROJECT_DIR
PROJECT_NAME=$(basename "$PROJECT_DIR" .git)
init_config_file
load_project_config

# Check if review agent is enabled
ENABLE_REVIEW=${ENABLE_REVIEW_AGENT:-$(config_get '.workflow.enable_review_agent // false')}
if [ "$ENABLE_REVIEW" != "true" ]; then
  exit 0
fi

# Resolve repo
REPO=${GITHUB_REPO:-$(config_get '.gh.repo // ""')}
if [ -z "$REPO" ] || [ "$REPO" = "null" ]; then
  if [ -n "$PROJECT_DIR" ] && [ -d "$PROJECT_DIR/.git" ]; then
    REPO=$(cd "$PROJECT_DIR" && gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || true)
  elif [ -n "$PROJECT_DIR" ] && is_bare_repo "$PROJECT_DIR"; then
    REPO=$(git -C "$PROJECT_DIR" config remote.origin.url 2>/dev/null \
      | sed -E 's#^https?://github\.com/##; s#^git@github\.com:##; s#\.git$##' || true)
  fi
fi
if [ -z "$REPO" ] || [ "$REPO" = "null" ]; then
  exit 0
fi

# Config
REVIEW_AGENT=${REVIEW_AGENT:-$(config_get '.workflow.review_agent // "claude"')}
DIFF_LIMIT=${REVIEW_DIFF_LIMIT:-$(config_get '.workflow.review_diff_limit // 5000')}
REVIEW_DRAFTS=${REVIEW_DRAFTS:-$(config_get '.workflow.review_drafts // false')}
MERGE_COMMANDS=${MERGE_COMMANDS:-$(config_get '.workflow.merge_commands // "merge,lgtm,ship it"')}
MERGE_STRATEGY=${MERGE_STRATEGY:-$(config_get '.workflow.merge_strategy // "squash"')}

# Review owner — person whose comments trigger merge
REVIEW_OWNER=$(config_get '.workflow.review_owner // ""' | sed 's/^@//')
if [ -z "$REVIEW_OWNER" ] || [ "$REVIEW_OWNER" = "null" ]; then
  REVIEW_OWNER=$(repo_owner "$REPO")
fi

# State tracking — keyed by repo to prevent duplicate reviews across project dirs
ensure_state_dir
REVIEW_STATE_FILE="pr_reviews_$(printf '%s' "$REPO" | tr '/' '_').tsv"
REVIEW_STATE="${STATE_DIR}/${REVIEW_STATE_FILE}"
touch "$REVIEW_STATE"

# Agent badge helper (inline for minimal dependencies)
_review_badge() {
  case "${1:-}" in
    claude)   echo "🟣" ;;
    codex)    echo "🟢" ;;
    opencode) echo "🔵" ;;
    *)        echo "🔍" ;;
  esac
}

# Extract JSON from agent response (may be wrapped in markdown code blocks)
_extract_json() {
  local raw="$1"
  local json=""
  # Try ```json ... ``` blocks first
  json=$(printf '%s' "$raw" | sed -n '/^```json/,/^```$/p' | sed '1d;$d')
  if [ -z "$json" ]; then
    # Try any ``` ... ``` block
    json=$(printf '%s' "$raw" | sed -n '/^```/,/^```$/p' | sed '1d;$d')
  fi
  if [ -z "$json" ]; then
    # Try raw JSON (find first { to last })
    json=$(printf '%s' "$raw" | sed -n '/{/,/}/p')
  fi
  if [ -z "$json" ]; then
    json="$raw"
  fi
  printf '%s' "$json"
}

# --- Check for merge commands in PR comments ---
_check_merge_commands() {
  local pr_number="$1"
  local pr_sha="$2"

  [ -z "$REVIEW_OWNER" ] || [ "$REVIEW_OWNER" = "null" ] && return 0

  # Get recent comments on the PR
  local comments_json
  comments_json=$(gh_api -X GET "repos/$REPO/issues/$pr_number/comments" \
    -f per_page=20 -f sort=created -f direction=desc 2>/dev/null || echo "[]")

  local comment_count
  comment_count=$(printf '%s' "$comments_json" | jq -r 'length' 2>/dev/null || echo "0")
  [ "$comment_count" -eq 0 ] && return 0

  # Check if owner posted a merge command
  for ci in $(seq 0 $((comment_count - 1))); do
    local author body created_at
    author=$(printf '%s' "$comments_json" | jq -r ".[$ci].user.login" 2>/dev/null)
    body=$(printf '%s' "$comments_json" | jq -r ".[$ci].body // \"\"" 2>/dev/null)
    created_at=$(printf '%s' "$comments_json" | jq -r ".[$ci].created_at // \"\"" 2>/dev/null)

    # Only process owner comments
    [ "$author" != "$REVIEW_OWNER" ] && continue

    # Check if this comment was already processed
    if grep -q "^merge	${pr_number}	${created_at}" "$REVIEW_STATE" 2>/dev/null; then
      continue
    fi

    # Normalize body for matching
    local body_lower
    body_lower=$(printf '%s' "$body" | tr '[:upper:]' '[:lower:]' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')

    # Check against merge commands
    local should_merge=false
    IFS=',' read -ra cmds <<< "$MERGE_COMMANDS"
    for cmd in "${cmds[@]}"; do
      cmd=$(printf '%s' "$cmd" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
      if [ "$body_lower" = "$cmd" ]; then
        should_merge=true
        break
      fi
    done

    if [ "$should_merge" = true ]; then
      log "[review_prs] [$PROJECT_NAME] PR #$pr_number: merge command from @$REVIEW_OWNER"

      local merge_flag="--squash"
      case "$MERGE_STRATEGY" in
        rebase) merge_flag="--rebase" ;;
        merge)  merge_flag="--merge" ;;
        *)      merge_flag="--squash" ;;
      esac

      if gh pr merge "$pr_number" --repo "$REPO" "$merge_flag" --delete-branch 2>/dev/null; then
        log "[review_prs] [$PROJECT_NAME] PR #$pr_number merged ($MERGE_STRATEGY)"
        printf 'merge\t%s\t%s\t%s\n' "$pr_number" "$created_at" "$(now_iso)" >> "$REVIEW_STATE"

        # Update linked task if exists
        _update_linked_task "$pr_number" "done" "merged via command by @$REVIEW_OWNER"
      else
        log_err "[review_prs] [$PROJECT_NAME] PR #$pr_number: merge failed"
      fi
      return 0
    fi
  done
}

# --- Update linked task ---
_update_linked_task() {
  local pr_number="$1" new_status="$2" note="$3"
  local pr_branch
  pr_branch=$(printf '%s' "$PRS_JSON" | jq -r ".[] | select(.number == $pr_number) | .head.ref" 2>/dev/null || true)
  [ -z "$pr_branch" ] && return 0

  local task_id
  task_id=$(db_task_id_by_branch "$pr_branch" "$PROJECT_DIR")
  [ -z "$task_id" ] || [ "$task_id" = "null" ] && return 0

  db_task_update "$task_id" "status=$new_status"
  db_append_history "$task_id" "$new_status" "$note"
}

# --- Review a single PR ---
_review_pr() {
  local pr_number="$1" pr_title="$2" pr_body="$3" pr_author="$4" pr_sha="$5" pr_branch="$6"

  log "[review_prs] [$PROJECT_NAME] reviewing PR #$pr_number: $pr_title (by $pr_author)"

  # Get diff
  local GIT_DIFF
  GIT_DIFF=$(gh pr diff "$pr_number" --repo "$REPO" 2>/dev/null | head -"$DIFF_LIMIT" || true)

  if [ -z "$GIT_DIFF" ]; then
    log_err "[review_prs] [$PROJECT_NAME] PR #$pr_number: empty diff, skipping"
    return 0
  fi

  # Build prompt
  export PR_NUMBER="$pr_number" PR_TITLE="$pr_title" PR_BODY="$pr_body"
  export PR_AUTHOR="$pr_author" GIT_DIFF DIFF_LIMIT REPO
  local REVIEW_PROMPT
  REVIEW_PROMPT=$(render_template "$SCRIPT_DIR/../prompts/pr_review.md")

  # Run review agent
  local REVIEW_MODEL REVIEW_RESPONSE="" REVIEW_RC=0
  REVIEW_MODEL=$(model_for_complexity "$REVIEW_AGENT" "review")

  case "$REVIEW_AGENT" in
    codex)
      REVIEW_RESPONSE=$(run_with_timeout codex ${REVIEW_MODEL:+--model "$REVIEW_MODEL"} --print "$REVIEW_PROMPT") || REVIEW_RC=$?
      ;;
    claude)
      REVIEW_RESPONSE=$(run_with_timeout claude ${REVIEW_MODEL:+--model "$REVIEW_MODEL"} --print "$REVIEW_PROMPT") || REVIEW_RC=$?
      ;;
    opencode)
      REVIEW_RESPONSE=$(run_with_timeout opencode ${REVIEW_MODEL:+--model "$REVIEW_MODEL"} --print "$REVIEW_PROMPT") || REVIEW_RC=$?
      ;;
    *)
      log_err "[review_prs] unknown review agent: $REVIEW_AGENT"
      return 0
      ;;
  esac

  if [ "$REVIEW_RC" -ne 0 ] || [ -z "$REVIEW_RESPONSE" ]; then
    log_err "[review_prs] [$PROJECT_NAME] PR #$pr_number: agent failed (rc=$REVIEW_RC)"
    return 0
  fi

  # Parse response
  local REVIEW_JSON DECISION NOTES
  REVIEW_JSON=$(_extract_json "$REVIEW_RESPONSE")
  DECISION=$(printf '%s' "$REVIEW_JSON" | jq -r '.decision // ""' 2>/dev/null || true)
  NOTES=$(printf '%s' "$REVIEW_JSON" | jq -r '.notes // ""' 2>/dev/null || true)

  if [ -z "$DECISION" ]; then
    log_err "[review_prs] [$PROJECT_NAME] PR #$pr_number: could not parse decision"
    return 0
  fi

  # Build comment body
  local BADGE ICON COMMENT_BODY
  BADGE=$(_review_badge "$REVIEW_AGENT")
  if [ "$DECISION" = "approve" ]; then
    ICON="approve"
  else
    ICON="request_changes"
  fi

  COMMENT_BODY=$(cat <<EOF
## ${BADGE} Automated Review — $([ "$ICON" = "approve" ] && echo "Approve" || echo "Changes Requested")

${NOTES}

---
*By ${REVIEW_AGENT}[bot]${REVIEW_MODEL:+ using model \`${REVIEW_MODEL}\`} via [Orchestrator](https://github.com/gabrielkoerich/orchestrator)*
EOF
  )

  # Post review: try formal PR review first, fall back to comment
  if [ "$DECISION" = "approve" ]; then
    gh pr review "$pr_number" --repo "$REPO" --approve --body "$COMMENT_BODY" 2>/dev/null \
      || gh pr comment "$pr_number" --repo "$REPO" --body "$COMMENT_BODY" 2>/dev/null \
      || log_err "[review_prs] failed to post review on PR #$pr_number"
  else
    gh pr review "$pr_number" --repo "$REPO" --request-changes --body "$COMMENT_BODY" 2>/dev/null \
      || gh pr comment "$pr_number" --repo "$REPO" --body "$COMMENT_BODY" 2>/dev/null \
      || log_err "[review_prs] failed to post review on PR #$pr_number"
  fi

  # Record in state
  printf '%s\t%s\t%s\t%s\t%s\n' "$pr_number" "$pr_sha" "$DECISION" "$(now_iso)" "$pr_title" >> "$REVIEW_STATE"
  log "[review_prs] [$PROJECT_NAME] PR #$pr_number: $DECISION"

  # Auto-merge on approve — GitHub enforces CI checks, merge fails if CI hasn't passed
  if [ "$DECISION" = "approve" ]; then
    local merge_flag="--squash"
    case "$MERGE_STRATEGY" in
      rebase) merge_flag="--rebase" ;;
      merge)  merge_flag="--merge" ;;
      *)      merge_flag="--squash" ;;
    esac
    # Use --auto so GitHub merges only after CI passes (doesn't bypass branch protection)
    if gh pr merge "$pr_number" --repo "$REPO" "$merge_flag" --auto --delete-branch 2>/dev/null; then
      log "[review_prs] [$PROJECT_NAME] PR #$pr_number auto-merge enabled ($MERGE_STRATEGY) — will merge when CI passes"
    else
      log "[review_prs] [$PROJECT_NAME] PR #$pr_number: auto-merge could not be enabled"
    fi
  fi

  # Update linked task
  local task_id
  task_id=$(db_task_id_by_branch "$pr_branch" "$PROJECT_DIR")
  if [ -n "$task_id" ] && [ "$task_id" != "null" ]; then
    local task_status
    if [ "$DECISION" = "approve" ]; then
      task_status="done"
    else
      task_status="needs_review"
    fi
    db_task_update "$task_id" \
      "review_decision=$DECISION" \
      "review_notes=$NOTES" \
      "status=$task_status"
    db_append_history "$task_id" "$task_status" "pr review: $DECISION by $REVIEW_AGENT"
  fi
}

# ============================================================
# Main loop: list open PRs and review unreviewed ones
# ============================================================

# Fetch open PRs
PRS_JSON=$(gh_api -X GET "repos/$REPO/pulls" -f state=open -f per_page=100 2>/dev/null || echo "[]")
# Handle paginated responses
PRS_JSON=$(printf '%s' "$PRS_JSON" | jq -s 'if type == "array" and (.[0] | type) == "array" then [.[][]] else . end' 2>/dev/null || echo "$PRS_JSON")

PR_COUNT=$(printf '%s' "$PRS_JSON" | jq -r 'length' 2>/dev/null || echo "0")
if [ "$PR_COUNT" -eq 0 ]; then
  exit 0
fi

log "[review_prs] [$PROJECT_NAME] found $PR_COUNT open PRs"

for i in $(seq 0 $((PR_COUNT - 1))); do
  PR_NUMBER=$(printf '%s' "$PRS_JSON" | jq -r ".[$i].number")
  PR_TITLE=$(printf '%s' "$PRS_JSON" | jq -r ".[$i].title")
  PR_BODY=$(printf '%s' "$PRS_JSON" | jq -r ".[$i].body // \"\"")
  PR_AUTHOR=$(printf '%s' "$PRS_JSON" | jq -r ".[$i].user.login")
  PR_SHA=$(printf '%s' "$PRS_JSON" | jq -r ".[$i].head.sha")
  PR_DRAFT=$(printf '%s' "$PRS_JSON" | jq -r ".[$i].draft")
  PR_BRANCH=$(printf '%s' "$PRS_JSON" | jq -r ".[$i].head.ref")

  # Skip drafts unless configured
  if [ "$PR_DRAFT" = "true" ] && [ "$REVIEW_DRAFTS" != "true" ]; then
    continue
  fi

  # Check for merge commands from owner (always, even if already reviewed)
  _check_merge_commands "$PR_NUMBER" "$PR_SHA"

  # Skip if already reviewed at this SHA
  if grep -q "^${PR_NUMBER}	${PR_SHA}	" "$REVIEW_STATE" 2>/dev/null; then
    continue
  fi

  # Review the PR
  _review_pr "$PR_NUMBER" "$PR_TITLE" "$PR_BODY" "$PR_AUTHOR" "$PR_SHA" "$PR_BRANCH"
done

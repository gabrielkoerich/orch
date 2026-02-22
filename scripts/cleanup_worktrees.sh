#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
source "$SCRIPT_DIR/lib.sh"

PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"
export PROJECT_DIR
PROJECT_NAME=$(basename "$PROJECT_DIR" .git)

init_config_file
load_project_config
normalize_field() {
  local value="${1:-}"
  if [ -z "$value" ] || [ "$value" = "null" ]; then
    echo ""
  else
    echo "$value"
  fi
}

resolve_repo() {
  local project_dir="$1"
  local repo

  repo=${GITHUB_REPO:-$(config_get '.gh.repo // ""')}
  repo=$(normalize_field "$repo")
  if [ -n "$repo" ]; then
    echo "$repo"
    return 0
  fi

  if [ -n "$project_dir" ] && [ -d "$project_dir/.git" ] && command -v gh >/dev/null 2>&1; then
    repo=$(cd "$project_dir" && gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || true)
  elif [ -n "$project_dir" ] && is_bare_repo "$project_dir"; then
    repo=$(git -C "$project_dir" config remote.origin.url 2>/dev/null \
      | sed -E 's#^https?://github\.com/##; s#^git@github\.com:##; s#\.git$##' || true)
  fi

  normalize_field "$repo"
}

issue_has_merged_pr() {
  local repo="$1"
  local issue_number="$2"
  local pr_number=""

  if ! command -v gh >/dev/null 2>&1; then
    return 1
  fi

  pr_number=$(gh pr list \
    --repo "$repo" \
    --state merged \
    --search "closes #$issue_number" \
    --json number \
    --jq '.[0].number // ""' 2>/dev/null || true)
  [ -n "$pr_number" ]
}

log "[cleanup_worktrees] [$PROJECT_NAME] scan start"

# Query done tasks with worktrees that haven't been cleaned yet
DONE_IDS=$(db_task_ids_by_status "done")
while IFS= read -r id; do
  [ -n "$id" ] || continue

  worktree=$(db_task_field "$id" "worktree")
  branch=$(db_task_field "$id" "branch")
  project_dir=$(db_task_field "$id" "dir")
  gh_issue="$id"  # In GitHub backend, task ID = issue number
  wt_cleaned=$(db_task_field "$id" "worktree_cleaned")

  worktree=$(normalize_field "$worktree")
  branch=$(normalize_field "$branch")
  project_dir=$(normalize_field "$project_dir")

  [ -n "$worktree" ] || continue
  [ -n "$project_dir" ] || continue
  # Skip already-cleaned tasks
  [ "$wt_cleaned" = "1" ] || [ "$wt_cleaned" = "true" ] && continue

  repo=$(resolve_repo "$project_dir")
  if [ -z "$repo" ]; then
    log "[cleanup_worktrees] [$PROJECT_NAME] task=$id missing repo; skipping PR merge check"
    continue
  fi
  if ! issue_has_merged_pr "$repo" "$gh_issue"; then
    continue
  fi

  cleanup_ok=true

  if [ -d "$worktree" ]; then
    log "[cleanup_worktrees] [$PROJECT_NAME] removing worktree=$worktree"
    if ! git -C "$project_dir" worktree remove "$worktree" --force >/dev/null 2>&1; then
      log_err "[cleanup_worktrees] [$PROJECT_NAME] failed to remove worktree=$worktree"
      cleanup_ok=false
    fi
  fi

  if [ -n "$branch" ] && git -C "$project_dir" show-ref --verify --quiet "refs/heads/$branch"; then
    log "[cleanup_worktrees] [$PROJECT_NAME] deleting branch=$branch"
    # Use -D (force) because squash-merged PRs are not considered merged by git
    if ! git -C "$project_dir" branch -D "$branch" >/dev/null 2>&1; then
      log_err "[cleanup_worktrees] [$PROJECT_NAME] failed to delete branch=$branch"
      cleanup_ok=false
    fi
  fi

  if [ "$cleanup_ok" = true ]; then
    db_task_set "$id" "worktree_cleaned" "1"
    log "[cleanup_worktrees] [$PROJECT_NAME] task=$id marked worktree_cleaned=true"
  fi
done <<< "$DONE_IDS"

log "[cleanup_worktrees] [$PROJECT_NAME] scan done"

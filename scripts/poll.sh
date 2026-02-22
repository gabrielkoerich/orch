#!/usr/bin/env bash
# shellcheck source=scripts/lib.sh
set -euo pipefail
source "$(dirname "$0")/lib.sh"

JOBS=${POLL_JOBS:-4}
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

# Recover tasks stuck in_progress — either no agent assigned, or stale (no lock, updated too long ago)
STUCK_TIMEOUT=${STUCK_TIMEOUT:-$(config_get '.workflow.stuck_timeout // 1800')}
NOW_EPOCH=$(date +%s)
NOW=$(now_iso)
export NOW

# Stuck detection: in_progress without agent → needs_review
IN_PROGRESS_IDS=$(db_task_ids_by_status "in_progress")
if [ -n "$IN_PROGRESS_IDS" ]; then
  while IFS= read -r sid; do
    [ -n "$sid" ] || continue
    TASK_LOCK="${LOCK_PATH}.task.${sid}"
    TASK_AGENT=$(db_task_field "$sid" "agent")

    if [ -z "$TASK_AGENT" ] || [ "$TASK_AGENT" = "null" ]; then
      log "[poll] task=$sid stuck in_progress without agent"
      db_task_update "$sid" status=needs_review "last_error=task stuck in_progress without agent"
      db_append_history "$sid" "needs_review" "stuck in_progress without agent"
      continue
    fi

    # Agent assigned but no lock held — agent process died
    if [ ! -d "$TASK_LOCK" ]; then
      UPDATED_AT=$(db_task_field "$sid" "updated_at")
      if [ -n "$UPDATED_AT" ] && [ "$UPDATED_AT" != "null" ]; then
        UPDATED_EPOCH=$(date -u -jf "%Y-%m-%dT%H:%M:%SZ" "$UPDATED_AT" +%s 2>/dev/null \
          || date -d "$UPDATED_AT" +%s 2>/dev/null \
          || echo 0)
        ELAPSED=$((NOW_EPOCH - UPDATED_EPOCH))
        if [ "$ELAPSED" -ge "$STUCK_TIMEOUT" ]; then
          log "[poll] task=$sid stuck in_progress for ${ELAPSED}s (no lock held), recovering"
          db_task_update "$sid" status=new "last_error=recovered: stuck in_progress for ${ELAPSED}s"
          db_append_history "$sid" "new" "recovered from stuck in_progress (${ELAPSED}s, no lock)"
        fi
      fi
    fi
  done <<< "$IN_PROGRESS_IDS"
fi

# Check in_review tasks for merged PRs → mark done
IN_REVIEW_IDS=$(db_task_ids_by_status "in_review")
if [ -n "$IN_REVIEW_IDS" ] && command -v gh >/dev/null 2>&1; then
  while IFS= read -r rid; do
    [ -n "$rid" ] || continue
    BRANCH=$(db_task_field "$rid" "branch")
    TASK_DIR=$(db_task_field "$rid" "dir")
    WORKTREE=$(db_task_field "$rid" "worktree")
    CHECK_DIR="${WORKTREE:-${TASK_DIR:-.}}"

    if [ -n "$BRANCH" ] && [ "$BRANCH" != "null" ] && [ -d "$CHECK_DIR" ]; then
      PR_STATE=$(cd "$CHECK_DIR" && gh pr view "$BRANCH" --json state -q '.state' 2>/dev/null || true)
      if [ "$PR_STATE" = "MERGED" ]; then
        log "[poll] task=$rid PR merged (branch=$BRANCH), marking done"
        db_task_set "$rid" "status" "done"
        db_append_history "$rid" "done" "PR merged"
      fi
    fi
  done <<< "$IN_REVIEW_IDS"
fi

# Worktree janitor: clean up done tasks with worktrees (from sidecar data)
DONE_IDS_WT=$(db_task_ids_by_status "done")
if [ -n "$DONE_IDS_WT" ]; then
  while IFS= read -r tid; do
    [ -n "$tid" ] || continue
    wt=$(db_task_field "$tid" "worktree")
    [ -n "$wt" ] || continue
    [ -d "$wt" ] || continue
    branch=$(db_task_field "$tid" "branch")
    task_dir=$(db_task_field "$tid" "dir")

    MAIN_DIR="${task_dir:-.}"
    if [ ! -d "$MAIN_DIR/.git" ] && [ ! -f "$MAIN_DIR/.git" ]; then
      MAIN_DIR=$(cd "$wt" && git rev-parse --path-format=absolute --git-common-dir 2>/dev/null | sed 's|/.git$||' || true)
    fi
    [ -d "$MAIN_DIR" ] || continue

    log "[poll] task=$tid cleaning up worktree $wt (branch=$branch)"
    (cd "$MAIN_DIR" && git worktree remove --force "$wt" 2>/dev/null) || true
    if [ -n "$branch" ] && [ "$branch" != "null" ] && [ "$branch" != "main" ] && [ "$branch" != "master" ]; then
      (cd "$MAIN_DIR" && git branch -D "$branch" 2>/dev/null) || true
    fi

    db_task_update "$tid" worktree=NULL branch=NULL
    db_append_history "$tid" "done" "cleaned up worktree and branch"
  done <<< "$DONE_IDS_WT"
fi

# Normalize: add status:new to open issues missing a status label
db_normalize_new_issues 2>/dev/null || true

# Owner feedback + slash commands: scan for new owner comments and apply them.
# This lets the owner re-activate completed tasks or issue commands like /retry.
BACKEND=${ORCH_BACKEND:-$(config_get '.backend // ""')}
if [ "$BACKEND" = "github" ] && command -v gh >/dev/null 2>&1; then
  REPO=$(config_get '.gh.repo // ""' 2>/dev/null || true)
  if [ -z "$REPO" ] || [ "$REPO" = "null" ]; then
    REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || true)
  fi
  OWNER_LOGIN=$(repo_owner "$REPO")
  if [ -n "$REPO" ] && [ "$REPO" != "null" ] && [ -n "$OWNER_LOGIN" ]; then
    CANDIDATES=$(
      {
        db_task_ids_by_status "done" || true
        db_task_ids_by_status "in_review" || true
        db_task_ids_by_status "needs_review" || true
        db_task_ids_by_status "blocked" || true
      } | sort -u
    )
    if [ -n "$CANDIDATES" ]; then
      while IFS= read -r cid; do
        [ -n "$cid" ] || continue
        process_owner_feedback_for_task "$REPO" "$cid" "$OWNER_LOGIN" 2>/dev/null || true
      done <<< "$CANDIDATES"
    fi
  fi
fi

# Run all new/routed tasks in parallel (skip tasks with no-agent label)
NEW_IDS=$(db_task_ids_by_status "new" "no-agent")
ROUTED_IDS=$(db_task_ids_by_status "routed" "no-agent")
ALL_IDS=$(printf '%s\n%s' "$NEW_IDS" "$ROUTED_IDS" | grep -v '^$' || true)
if [ -n "$ALL_IDS" ]; then
  printf '%s\n' "$ALL_IDS" | xargs -n1 -P "$JOBS" -I{} "$SCRIPT_DIR/run_task.sh" "{}"
fi

# Collect blocked parents ready to rejoin (all children done)
# Query blocked tasks that have children, check if all children are done
BLOCKED_IDS=$(db_task_ids_by_status "blocked")
if [ -n "$BLOCKED_IDS" ]; then
  READY_IDS=""
  while IFS= read -r bid; do
    [ -n "$bid" ] || continue
    # Get children of this task
    CHILDREN=$(db_task_children "$bid" 2>/dev/null || true)
    [ -z "$CHILDREN" ] && continue
    # Check if ALL children are done
    ALL_DONE=true
    while IFS= read -r cid; do
      [ -n "$cid" ] || continue
      CSTATUS=$(db_task_field "$cid" "status")
      if [ "$CSTATUS" != "done" ]; then
        ALL_DONE=false
        break
      fi
    done <<< "$CHILDREN"
    if $ALL_DONE; then
      READY_IDS="${READY_IDS:+$READY_IDS$'\n'}$bid"
    fi
  done <<< "$BLOCKED_IDS"

  if [ -n "$READY_IDS" ]; then
    printf '%s\n' "$READY_IDS" | xargs -n1 -P "$JOBS" -I{} "$SCRIPT_DIR/run_task.sh" "{}"
  fi
fi

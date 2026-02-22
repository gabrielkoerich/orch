#!/usr/bin/env bash
# shellcheck source=scripts/lib.sh
set -euo pipefail
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/output.sh"

PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"
export PROJECT_DIR

TOTAL=$(db_total_filtered_count)

# Show status summary
NEW=$(db_status_count "new")
ROUTED=$(db_status_count "routed")
INPROG=$(db_status_count "in_progress")
BLOCKED=$(db_status_count "blocked")
IN_REVIEW=$(db_status_count "in_review")
DONE=$(db_status_count "done")
NEEDS_REVIEW=$(db_status_count "needs_review")

if [ "$TOTAL" -eq 0 ]; then
  echo "No tasks."
else
  printf 'Tasks: %s total — %s new, %s routed, %s in_progress, %s in_review, %s blocked, %s done, %s needs_review\n' \
    "$TOTAL" "$NEW" "$ROUTED" "$INPROG" "$IN_REVIEW" "$BLOCKED" "$DONE" "$NEEDS_REVIEW"

  # Show active tasks (non-done)
  ACTIVE_TSV=$(db_task_display_tsv '.status != "done"' "id")
  if [ -n "$ACTIVE_TSV" ]; then
    section "Active tasks:"
    printf '%s\n' "$ACTIVE_TSV" | table_with_header "$TASK_HEADER"
  fi
fi

# Token usage and cost summary
USAGE_TSV=$(db_task_usage_tsv)
if [ -n "$USAGE_TSV" ]; then
  section "Usage:"
  # Pricing per 1M tokens (USD) — approximate
  # Claude: haiku=$0.25/$1.25, sonnet=$3/$15, opus=$15/$75
  # Codex: gpt-5.1-mini=$0.30/$1.20, gpt-5.2=$2.50/$10, gpt-5.3=$15/$60
  USAGE_SUMMARY=$(printf '%s\n' "$USAGE_TSV" | awk -F'\t' '
    BEGIN { ti=0; to=0; dur=0; cost=0 }
    {
      ti += $1; to += $2; dur += ($3 > 0 ? $3 : 0)
      m = $4
      if (m == "haiku" || m == "gpt-5.1-codex-mini")
        cost += ($1 * 0.25 + $2 * 1.25) / 1000000
      else if (m == "opus" || m == "gpt-5.3-codex")
        cost += ($1 * 15 + $2 * 75) / 1000000
      else
        cost += ($1 * 3 + $2 * 15) / 1000000
    }
    END { printf "%d\t%d\t%d\t%.2f", ti, to, dur, cost }
  ')
  TOTAL_INPUT=$(printf '%s' "$USAGE_SUMMARY" | cut -f1)
  TOTAL_OUTPUT=$(printf '%s' "$USAGE_SUMMARY" | cut -f2)
  TOTAL_DURATION=$(printf '%s' "$USAGE_SUMMARY" | cut -f3)
  TOTAL_COST=$(printf '%s' "$USAGE_SUMMARY" | cut -f4)
  printf '  Tokens: %s input / %s output\n' "$TOTAL_INPUT" "$TOTAL_OUTPUT"
  printf '  Duration: %s\n' "$(duration_fmt "$TOTAL_DURATION")"
  printf '  Estimated cost: $%s\n' "$TOTAL_COST"
fi

# Show active projects
section "Projects:"
PROJECTS=$(db_task_projects)
if [ -z "$PROJECTS" ]; then
  echo "  (none)"
else
  while IFS= read -r dir; do
    [ -n "$dir" ] || continue
    count=$(db_task_active_count_for_dir "$dir")
    printf '  %s (%s active)\n' "$dir" "$count"
  done <<< "$PROJECTS"
fi

# Show active worktrees (project-local + legacy global)
section "Worktrees:"
_WT_FOUND=false
# Check project-local worktrees for each managed project
for _wt_dir in "${PROJECT_DIR:-.}/.orchestrator/worktrees" "${ORCH_WORKTREES}"; do
  [ -d "$_wt_dir" ] || continue
  _WTS=$(fd --min-depth 1 --max-depth 2 --type d . "$_wt_dir" 2>/dev/null || true)
  while IFS= read -r wt; do
    [ -n "$wt" ] || continue
    # Only show directories that are actual git worktrees
    [ -f "$wt/.git" ] || continue
    branch=$(cd "$wt" && git branch --show-current 2>/dev/null || echo "?")
    printf '  %s (%s)\n' "$wt" "$branch"
    _WT_FOUND=true
  done <<< "$_WTS"
done
if [ "$_WT_FOUND" = false ]; then
  echo "  (none)"
fi

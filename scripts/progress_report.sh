#!/usr/bin/env bash
# progress_report.sh — Generate a progress report across all projects.
# Used by the monitoring job to give visibility into what's happening.
set -euo pipefail
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/output.sh"

# Report period (default: last 60 minutes)
SINCE_MINUTES=${REPORT_SINCE_MINUTES:-60}
SINCE_ISO=$(date -u -v-${SINCE_MINUTES}M +"%Y-%m-%dT%H:%M:%SZ" 2>/dev/null \
  || date -u -d "$SINCE_MINUTES minutes ago" +"%Y-%m-%dT%H:%M:%SZ")

echo "=== Progress Report (last ${SINCE_MINUTES}m) ==="
echo "Generated: $(date -u +"%Y-%m-%dT%H:%M:%SZ")"
echo ""

# --- Task Status Summary (global) ---
section "Task Status (all projects)"
TOTAL=$(db_total_task_count)
{
  printf 'STATUS\tQTY\n'
  for s in new routed in_progress blocked done needs_review; do
    printf '%s\t%s\n' "$s" "$(db_status_count "$s")"
  done
  printf '────────\t───\n'
  printf 'total\t%s\n' "$TOTAL"
} | column -t -s $'\t'

_list_issues_by_status() {
  local status="$1"
  _gh_ensure_repo || return 0
  local state="open"
  [ "$status" = "done" ] && state="closed"
  gh_api -X GET "repos/$_GH_REPO/issues" \
    -f state="$state" -f labels="${_GH_STATUS_PREFIX}${status}" -f per_page=20 \
    -f sort=updated -f direction=desc 2>/dev/null || echo '[]'
}

# --- Recently completed tasks ---
section "Completed since $(echo "$SINCE_ISO" | sed 's/T/ /;s/Z//')"
DONE_JSON=$(_list_issues_by_status "done")
DONE_RECENT=$(printf '%s' "$DONE_JSON" | jq -r --arg since "$SINCE_ISO" \
  '.[] | select(.pull_request == null and .updated_at >= $since) | [(.number|tostring), "-", "", .title] | @tsv' 2>/dev/null || true)
# Enrich with sidecar data
if [ -n "$DONE_RECENT" ]; then
  {
    printf 'ID\tAGENT\tMODEL\tTITLE\n'
    while IFS=$'\t' read -r did _ _ dtitle; do
      dagent=$(_sidecar_read "$did" "agent" 2>/dev/null || echo "?")
      dmodel=$(_sidecar_read "$did" "agent_model" 2>/dev/null || true)
      printf '%s\t%s\t%s\t%s\n' "$did" "${dagent:-?}" "${dmodel:-}" "$dtitle"
    done <<< "$DONE_RECENT"
  } | column -t -s $'\t'
else
  echo "(none)"
fi

# --- Currently in progress ---
section "In Progress"
IP_JSON=$(_list_issues_by_status "in_progress")
IP_LIST=$(printf '%s' "$IP_JSON" | jq -r \
  '.[] | select(.pull_request == null) | [(.number|tostring), .title] | @tsv' 2>/dev/null || true)
if [ -n "$IP_LIST" ]; then
  {
    printf 'ID\tAGENT\tMODEL\tATT\tTITLE\n'
    while IFS=$'\t' read -r iid ititle; do
      iagent=$(_sidecar_read "$iid" "agent" 2>/dev/null || echo "?")
      imodel=$(_sidecar_read "$iid" "agent_model" 2>/dev/null || true)
      iattempts=$(_sidecar_read "$iid" "attempts" 2>/dev/null || echo "0")
      printf '%s\t%s\t%s\t%s\t%s\n' "$iid" "${iagent:-?}" "${imodel:-}" "${iattempts:-0}" "$ititle"
    done <<< "$IP_LIST"
  } | column -t -s $'\t'
else
  echo "(none)"
fi

# --- Needs review ---
section "Needs Review"
NR_JSON=$(_list_issues_by_status "needs_review")
NR_LIST=$(printf '%s' "$NR_JSON" | jq -r \
  '.[] | select(.pull_request == null) | [(.number|tostring), .title] | @tsv' 2>/dev/null | head -10 || true)
if [ -n "$NR_LIST" ]; then
  {
    printf 'ID\tAGENT\tATT\tLAST_ERROR\tTITLE\n'
    while IFS=$'\t' read -r nid ntitle; do
      nagent=$(_sidecar_read "$nid" "agent" 2>/dev/null || echo "?")
      nattempts=$(_sidecar_read "$nid" "attempts" 2>/dev/null || echo "0")
      nerror=$(_sidecar_read "$nid" "last_error" 2>/dev/null || true)
      nerror=${nerror:0:60}
      printf '%s\t%s\t%s\t%s\t%s\n' "$nid" "${nagent:-?}" "${nattempts:-0}" "${nerror:-}" "$ntitle"
    done <<< "$NR_LIST"
  } | column -t -s $'\t'
else
  echo "(none)"
fi

# --- Tasks queued ---
section "Queued (new/routed)"
QUEUED=""
for qs in new routed; do
  Q_JSON=$(_list_issues_by_status "$qs")
  Q_LIST=$(printf '%s' "$Q_JSON" | jq -r \
    '.[] | select(.pull_request == null) | [(.number|tostring), .title] | @tsv' 2>/dev/null | head -10 || true)
  [ -n "$Q_LIST" ] && QUEUED="${QUEUED:+${QUEUED}
}${Q_LIST}"
done
if [ -n "$QUEUED" ]; then
  {
    printf 'ID\tAGENT\tTITLE\n'
    while IFS=$'\t' read -r qid qtitle; do
      qagent=$(_sidecar_read "$qid" "agent" 2>/dev/null || echo "?")
      printf '%s\t%s\t%s\n' "$qid" "${qagent:-?}" "$qtitle"
    done <<< "$QUEUED"
  } | column -t -s $'\t'
else
  echo "(none)"
fi

# --- Recent activity from comments ---
section "Recent Activity"
echo "(check individual issue comments for activity log)"

# --- PR Status (check all projects with repos) ---
section "Open PRs"
PROJECTS=$(db_task_projects 2>/dev/null || true)
if [ -n "$PROJECTS" ]; then
  while IFS= read -r pdir; do
    [ -n "$pdir" ] || continue
    [ -d "$pdir" ] || continue
    REPO=""
    if [ -d "$pdir/.git" ] || is_bare_repo "$pdir" 2>/dev/null; then
      REPO=$(cd "$pdir" && gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || true)
    fi
    [ -z "$REPO" ] && continue

    PR_LIST=$(gh pr list --repo "$REPO" --state open --json number,title,headRefName,statusCheckRollup --limit 10 2>/dev/null || true)
    [ -z "$PR_LIST" ] || [ "$PR_LIST" = "[]" ] && continue

    PNAME=$(basename "$pdir" .git)
    echo ""
    echo "  $PNAME ($REPO):"
    printf '%s' "$PR_LIST" | jq -r '.[] | "    #\(.number) [\(.statusCheckRollup | map(.state) | if length == 0 then "?" elif all(. == "SUCCESS") then "CI:pass" elif any(. == "FAILURE") then "CI:fail" else "CI:pending" end)] \(.title)"' 2>/dev/null || true
  done <<< "$PROJECTS"
fi

# --- Permission Denials ---
DENIAL_LOG="${STATE_DIR}/permission-denials.log"
if [ -f "$DENIAL_LOG" ]; then
  RECENT_DENIALS=$(awk -v since="$SINCE_ISO" '$1 >= since' "$DENIAL_LOG" 2>/dev/null | wc -l | tr -d ' ')
  if [ "$RECENT_DENIALS" -gt 0 ]; then
    section "Permission Denials (${RECENT_DENIALS} recent)"
    awk -v since="$SINCE_ISO" '$1 >= since' "$DENIAL_LOG" | tail -5
  fi
fi

echo ""
echo "=== End Report ==="

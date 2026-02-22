#!/usr/bin/env bash
# Shared output formatting helpers. Source this file, don't execute it.

# Print a table from tab-separated input with headers
# Usage: table "ID\tSTATUS\tTITLE" "data lines..."
#   or:  some_command | table_with_header "ID\tSTATUS\tTITLE"
table_with_header() {
  local header="$1"
  { printf '%b\n' "$header"; cat; } | column -t -s $'\t'
}

# Print a key-value pair
kv() {
  printf '%-15s %s\n' "$1:" "$2"
}

# Print a section header
section() {
  printf '\n%s\n' "$1"
}

# Print a colored status
status_icon() {
  case "$1" in
    new)          printf '○' ;;
    routed)       printf '◎' ;;
    in_progress)  printf '◉' ;;
    done)         printf '✓' ;;
    blocked)      printf '✗' ;;
    needs_review) printf '!' ;;
    *)            printf '-' ;;
  esac
}

# Format a task row: id, status, agent, issue, title
task_row() {
  local id="$1" status="$2" agent="${3:--}" issue="${4:--}" title="$5"
  printf '%s\t%s\t%s\t%s\t%s\n' "$id" "$status" "$agent" "$issue" "$title"
}

# Standard task table header
TASK_HEADER="ID\tSTATUS\tAGENT\tISSUE\tTITLE"
TASK_HEADER_GLOBAL="ID\tSTATUS\tAGENT\tISSUE\tPROJECT\tTITLE"

# yq expression fragments (use inside yq -r "... | [${YQ_TASK_COLS}] | @tsv")
YQ_ISSUE='(.gh_issue_number | select(. != null and . > 0) | "#" + tostring) // "-"'
YQ_AGENT='(.agent // "-")'
YQ_PROJECT='(.dir // "-" | split("/") | .[-1])'
YQ_TASK_COLS=".id, .status, ${YQ_AGENT}, ${YQ_ISSUE}, .title"
YQ_TASK_COLS_GLOBAL=".id, .status, ${YQ_AGENT}, ${YQ_ISSUE}, ${YQ_PROJECT}, .title"

# Format gh_issue_number for display
fmt_issue() {
  local num="$1"
  if [ -n "$num" ] && [ "$num" != "null" ] && [ "$num" != "0" ]; then
    printf '#%s' "$num"
  else
    printf '-'
  fi
}

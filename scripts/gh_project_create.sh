#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
source "${SCRIPT_DIR}/lib.sh"
require_yq
PROJECT_DIR="${PROJECT_DIR:-$(pwd)}"
export PROJECT_DIR
init_config_file
load_project_config

require_gh() {
  if ! command -v gh >/dev/null 2>&1; then
    echo "gh is required but not found in PATH." >&2
    exit 1
  fi
}

require_gh

CONFIG_FILE="${PROJECT_DIR}/orchestrator.yml"

REPO=${GITHUB_REPO:-$(config_get '.gh.repo // ""')}
if [ -z "$REPO" ] || [ "$REPO" = "null" ]; then
  REPO=$(git -C "$PROJECT_DIR" config remote.origin.url 2>/dev/null \
    | sed -E 's#^https?://github\.com/##; s#^git@github\.com:##; s#\.git$##' || true)
fi
if [ -z "$REPO" ] || [ "$REPO" = "null" ]; then
  echo "No GitHub repo configured. Run 'orchestrator init' first or set gh.repo in orchestrator.yml." >&2
  exit 1
fi

TITLE="${1:-$(basename "$PROJECT_DIR" .git)}"
repo_owner=$(printf '%s' "$REPO" | cut -d'/' -f1)
owner_type=$(gh api "users/$repo_owner" -q .type 2>/dev/null || echo "User")

# --- List existing projects and let user pick or create ---
user_login=$(gh api user -q .login 2>/dev/null || true)
orgs=$(gh api user/orgs -q '.[].login' 2>/dev/null || true)

projects=()
ids=()

if [ -n "$user_login" ]; then
  user_json=$(gh api graphql -f query='query($user:String!){ user(login:$user){ projectsV2(first:100){ nodes{ id number title } } } }' -f user="$user_login" 2>/dev/null || true)
  if [ -n "$user_json" ]; then
    while IFS=$'\t' read -r number title id; do
      [ -n "$id" ] || continue
      projects+=("user:${user_login} #${number} ${title}")
      ids+=("$id")
    done < <(printf '%s' "$user_json" | yq -r '.data.user.projectsV2.nodes[] | [.number, .title, .id] | @tsv' 2>/dev/null)
  fi
fi

if [ -n "$orgs" ]; then
  while IFS= read -r org; do
    [ -n "$org" ] || continue
    org_json=$(gh api graphql -f query='query($org:String!){ organization(login:$org){ projectsV2(first:100){ nodes{ id number title } } } }' -f org="$org" 2>/dev/null || true)
    if [ -n "$org_json" ]; then
      while IFS=$'\t' read -r number title id; do
        [ -n "$id" ] || continue
        projects+=("org:${org} #${number} ${title}")
        ids+=("$id")
      done < <(printf '%s' "$org_json" | yq -r '.data.organization.projectsV2.nodes[] | [.number, .title, .id] | @tsv' 2>/dev/null)
    fi
  done <<< "$orgs"
fi

PROJECT_ID=""

if [ ${#projects[@]} -gt 0 ] && [ -t 0 ]; then
  echo "Existing GitHub Projects:"
  i=1
  for p in "${projects[@]}"; do
    echo "  [$i] $p"
    i=$((i + 1))
  done
  echo "  [n] Create new project"
  read -r -p "Select project to link (or 'n' to create new) [skip]: " selection

  if [ "$selection" = "n" ] || [ "$selection" = "N" ]; then
    : # fall through to create
  elif [ -n "$selection" ] && [[ "$selection" =~ ^[0-9]+$ ]]; then
    idx=$((selection - 1))
    if [ "$idx" -ge 0 ] && [ "$idx" -lt ${#ids[@]} ]; then
      PROJECT_ID="${ids[$idx]}"
      echo "Linking to existing project: ${projects[$idx]}"
    fi
  else
    echo "Skipped."
    exit 0
  fi
fi

# Create new project if none selected
if [ -z "$PROJECT_ID" ]; then
  echo "Creating GitHub Project '$TITLE' for $REPO..."

  if [ "$owner_type" = "Organization" ]; then
    create_json=$(gh api graphql \
      -f query='mutation($owner:ID!,$title:String!){ createProjectV2(input:{ownerId:$owner, title:$title}){ projectV2{ id number } } }' \
      -f owner="$(gh api "orgs/$repo_owner" -q .node_id)" \
      -f title="$TITLE")
  else
    create_json=$(gh api graphql \
      -f query='mutation($owner:ID!,$title:String!){ createProjectV2(input:{ownerId:$owner, title:$title}){ projectV2{ id number } } }' \
      -f owner="$(gh api user -q .node_id)" \
      -f title="$TITLE")
  fi

  PROJECT_ID=$(printf '%s' "$create_json" | yq -r '.data.createProjectV2.projectV2.id // ""')
  PROJECT_NUM=$(printf '%s' "$create_json" | yq -r '.data.createProjectV2.projectV2.number // ""')

  if [ -z "$PROJECT_ID" ] || [ "$PROJECT_ID" = "null" ]; then
    echo "Failed to create project." >&2
    printf '%s\n' "$create_json" >&2
    exit 1
  fi

  echo "Created project #$PROJECT_NUM: $TITLE"

  # Configure status columns and board view on new project
  configure_project_status_field "$PROJECT_ID"
  create_project_board_view "$PROJECT_NUM" "$repo_owner" "$owner_type"
fi

# Link project to repository
link_project_to_repo "$PROJECT_ID" "$REPO"

# Save to config
export GH_PROJECT_ID="$PROJECT_ID"
if [ -f "$CONFIG_FILE" ]; then
  yq -i ".gh.project_id = strenv(GH_PROJECT_ID)" "$CONFIG_FILE"
else
  export REPO
  cat > "$CONFIG_FILE" <<YAML
gh:
  repo: "$REPO"
YAML
  yq -i ".gh.project_id = strenv(GH_PROJECT_ID)" "$CONFIG_FILE"
fi

# Auto-detect status field options
auto_detect_status() {
  local config_file="$1" project_id="$2"
  local status_json
  status_json=$(gh api graphql -f query='query($project:ID!){ node(id:$project){ ... on ProjectV2 { fields(first:100){ nodes{ ... on ProjectV2SingleSelectField { id name options { id name } } } } } } }' -f project="$project_id" 2>/dev/null || true)
  [ -n "$status_json" ] || return 0

  local status_field_id
  status_field_id=$(printf '%s' "$status_json" | yq -r '.data.node.fields.nodes[] | select(.name == "Status") | .id' 2>/dev/null | head -n1)

  read_status_names() {
    local key="$1"
    if command -v jq >/dev/null 2>&1; then
      yq -o=json ".gh.project_status_names.${key} // null" "$config_file" 2>/dev/null | \
        jq -r 'if . == null then empty elif type == "array" then .[] elif type == "string" then . else empty end' 2>/dev/null
      return 0
    fi

    local val
    val=$(yq -r ".gh.project_status_names.${key} // \"\"" "$config_file" 2>/dev/null || true)
    [ -n "$val" ] && [ "$val" != "null" ] && printf '%s\n' "$val"
  }

  find_option_id() {
    local json="$1"; shift
    for name in "$@"; do
      [ -n "$name" ] || continue
      local opt_id
      local name_lower
      name_lower=$(printf '%s' "$name" | tr '[:upper:]' '[:lower:]')
      opt_id=$(printf '%s' "$json" | NAME_LOWER="$name_lower" yq -r '.data.node.fields.nodes[] | select(.name == "Status") | .options[] | select(.name | downcase == strenv(NAME_LOWER)) | .id' 2>/dev/null | head -n1)
      if [ -n "$opt_id" ] && [ "$opt_id" != "null" ]; then
        echo "$opt_id"
        return
      fi
    done
  }

  mapfile -t backlog_names < <(read_status_names backlog)
  mapfile -t inprog_names < <(read_status_names in_progress)
  mapfile -t review_names < <(read_status_names review)
  mapfile -t done_names < <(read_status_names done)

  if [ ${#backlog_names[@]} -eq 0 ]; then backlog_names=("Backlog"); fi
  if [ ${#inprog_names[@]} -eq 0 ]; then inprog_names=("In Progress"); fi
  if [ ${#review_names[@]} -eq 0 ]; then review_names=("Review"); fi
  if [ ${#done_names[@]} -eq 0 ]; then done_names=("Done"); fi

  local backlog_id inprog_id review_id done_id
  backlog_id=$(find_option_id "$status_json" "${backlog_names[@]}")
  inprog_id=$(find_option_id "$status_json" "${inprog_names[@]}")
  review_id=$(find_option_id "$status_json" "${review_names[@]}")
  done_id=$(find_option_id "$status_json" "${done_names[@]}")

  export status_field_id backlog_id inprog_id review_id done_id
  if [ -n "$status_field_id" ] && [ "$status_field_id" != "null" ]; then
    yq -i ".gh.project_status_field_id = strenv(status_field_id)" "$config_file"
  fi
  [ -n "${backlog_id:-}" ] && [ "$backlog_id" != "null" ] && yq -i '.gh.project_status_map.backlog = strenv(backlog_id)' "$config_file"
  [ -n "${inprog_id:-}" ] && [ "$inprog_id" != "null" ] && yq -i '.gh.project_status_map.in_progress = strenv(inprog_id)' "$config_file"
  [ -n "${review_id:-}" ] && [ "$review_id" != "null" ] && yq -i '.gh.project_status_map.review = strenv(review_id)' "$config_file"
  [ -n "${done_id:-}" ] && [ "$done_id" != "null" ] && yq -i '.gh.project_status_map.done = strenv(done_id)' "$config_file"

  echo "Detected status options:"
  [ -n "${backlog_id:-}" ] && echo "  backlog -> $backlog_id" || echo "  backlog -> (not found)"
  [ -n "${inprog_id:-}" ] && echo "  in_progress -> $inprog_id" || echo "  in_progress -> (not found)"
  [ -n "${review_id:-}" ] && echo "  review -> $review_id" || echo "  review -> (not found)"
  [ -n "${done_id:-}" ] && echo "  done -> $done_id" || echo "  done -> (not found)"
}

auto_detect_status "$CONFIG_FILE" "$PROJECT_ID"

echo ""
echo "Project ID: $PROJECT_ID"

# Auto-add guidance
PROJECT_NUM="${PROJECT_NUM:-}"
if [ -z "$PROJECT_NUM" ]; then
  # Extract project number for linked projects
  PROJECT_NUM=$(gh api graphql -f query='query($id:ID!){ node(id:$id){ ... on ProjectV2 { number } } }' -f id="$PROJECT_ID" -q '.data.node.number' 2>/dev/null || true)
fi

if [ -n "$PROJECT_NUM" ]; then
  if [ "$owner_type" = "Organization" ]; then
    workflows_url="https://github.com/orgs/$repo_owner/projects/$PROJECT_NUM/workflows"
  else
    workflows_url="https://github.com/users/$repo_owner/projects/$PROJECT_NUM/workflows"
  fi
  echo ""
  echo "To enable auto-add (new issues -> project), visit:"
  echo "  $workflows_url"
  echo "and enable 'Auto-add to project' for this repo."
fi

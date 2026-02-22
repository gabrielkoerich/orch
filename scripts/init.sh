#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
PROJECT_DIR=${PROJECT_DIR:-$(pwd)}

# Non-interactive mode via flags or env vars
GH_REPO="${ORCH_GH_REPO:-}"
GH_PROJECT_ID="${ORCH_GH_PROJECT_ID:-}"
FLAG_MODE=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo)       GH_REPO="$2"; FLAG_MODE=true; shift 2 ;;
    --project-id) GH_PROJECT_ID="$2"; FLAG_MODE=true; shift 2 ;;
    *)            echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done

CONFIG_FILE="$PROJECT_DIR/orchestrator.yml"

# Load existing config if present (idempotent re-init)
EXISTING_REPO=""
EXISTING_PROJECT=""
if [ -f "$CONFIG_FILE" ]; then
  command -v yq >/dev/null 2>&1 && {
    EXISTING_REPO=$(yq -r '.gh.repo // ""' "$CONFIG_FILE" 2>/dev/null || true)
    EXISTING_PROJECT=$(yq -r '.gh.project_id // ""' "$CONFIG_FILE" 2>/dev/null || true)
    [ -n "$EXISTING_REPO" ] && [ "$EXISTING_REPO" != "null" ] && GH_REPO="${GH_REPO:-$EXISTING_REPO}"
    [ -n "$EXISTING_PROJECT" ] && [ "$EXISTING_PROJECT" != "null" ] && GH_PROJECT_ID="${GH_PROJECT_ID:-$EXISTING_PROJECT}"
  }
fi

echo "Initialized orchestrator for $(basename "$PROJECT_DIR" .git)"
echo "  Project: $PROJECT_DIR"
[ -f "$CONFIG_FILE" ] && echo "  Config: orchestrator.yml (existing)"

write_config() {
  if [ -z "$GH_REPO" ]; then return; fi
  export GH_REPO
  if [ -f "$CONFIG_FILE" ]; then
    yq -i ".gh.repo = strenv(GH_REPO)" "$CONFIG_FILE"
  else
    cat > "$CONFIG_FILE" <<YAML
gh:
  repo: "$GH_REPO"
  sync_label: ""
YAML
  fi
  # Ensure sync_label exists (prevents global config leak)
  existing_sync_label=$(yq -r '.gh.sync_label // "MISSING"' "$CONFIG_FILE" 2>/dev/null || echo "MISSING")
  if [ "$existing_sync_label" = "MISSING" ]; then
    yq -i '.gh.sync_label = ""' "$CONFIG_FILE"
  fi
  echo "Saved orchestrator.yml"

  # Add orchestrator runtime files to .gitignore
  if [ -f ".gitignore" ]; then
    if ! rg -q '\.orchestrator/' .gitignore 2>/dev/null; then
      printf '\n# orchestrator runtime\n.orchestrator/\n' >> .gitignore
    fi
  else
    printf '# orchestrator runtime\n.orchestrator/\n' > .gitignore
  fi

  if [ -n "$GH_PROJECT_ID" ]; then
    export GH_PROJECT_ID
    yq -i ".gh.project_id = strenv(GH_PROJECT_ID)" "$CONFIG_FILE"
  fi

  # Auto-detect status field if gh is available and project_id is set
  local project_id="${GH_PROJECT_ID:-}"
  if [ -z "$project_id" ]; then
    project_id=$(yq -r '.gh.project_id // ""' "$CONFIG_FILE" 2>/dev/null || true)
  fi
  if [ -n "$project_id" ] && [ "$project_id" != "null" ] && command -v gh >/dev/null 2>&1; then
    auto_detect_status "$CONFIG_FILE" "$project_id"
  fi
}

configure_project_status_field() {
  local project_id="$1"
  local fields_json status_field_id
  fields_json=$(gh api graphql \
    -f query='query($project:ID!){ node(id:$project){ ... on ProjectV2 { fields(first:100){ nodes{ ... on ProjectV2SingleSelectField { id name } } } } } }' \
    -f project="$project_id" 2>/dev/null || true)
  [ -n "$fields_json" ] || return 0
  status_field_id=$(printf '%s' "$fields_json" | yq -r '.data.node.fields.nodes[] | select(.name == "Status") | .id' 2>/dev/null | head -n1)
  if [ -z "$status_field_id" ] || [ "$status_field_id" = "null" ]; then
    return 0
  fi
  gh api graphql \
    -f query="mutation(\$fieldId:ID!){ updateProjectV2Field(input:{fieldId:\$fieldId, singleSelectOptions:[{name:\"Backlog\",color:GRAY,description:\"\"},{name:\"In Progress\",color:YELLOW,description:\"\"},{name:\"Review\",color:ORANGE,description:\"\"},{name:\"Done\",color:GREEN,description:\"\"}]}){ projectV2Field{ ... on ProjectV2SingleSelectField { id name options { id name } } } } }" \
    -f fieldId="$status_field_id" >/dev/null 2>&1 || true
  echo "Configured project status columns: Backlog, In Progress, Review, Done"
}

create_project_board_view() {
  local project_num="$1" owner="$2" owner_type="$3"
  local endpoint
  if [ "$owner_type" = "Organization" ]; then
    endpoint="orgs/$owner/projectsV2/$project_num/views"
  else
    endpoint="users/$owner/projectsV2/$project_num/views"
  fi
  gh api "$endpoint" -X POST \
    -f name="Board" -f layout="board" \
    --header "X-GitHub-Api-Version:2022-11-28" >/dev/null 2>&1 || true
  echo "Created Board view"
}

link_project_to_repo() {
  local project_id="$1" repo="$2"
  local repo_node_id
  repo_node_id=$(gh api "repos/$repo" -q '.node_id' 2>/dev/null || true)
  if [ -n "$repo_node_id" ] && [ "$repo_node_id" != "null" ]; then
    gh api graphql \
      -f query='mutation($projectId:ID!,$repoId:ID!){ linkProjectV2ToRepository(input:{projectId:$projectId, repositoryId:$repoId}){ repository{ id } } }' \
      -f projectId="$project_id" \
      -f repoId="$repo_node_id" >/dev/null 2>&1 || true
    echo "Linked project to $repo"
  fi
}

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
  [ -n "$backlog_id" ] && [ "$backlog_id" != "null" ] && yq -i '.gh.project_status_map.backlog = strenv(backlog_id)' "$config_file"
  [ -n "$inprog_id" ] && [ "$inprog_id" != "null" ] && yq -i '.gh.project_status_map.in_progress = strenv(inprog_id)' "$config_file"
  [ -n "$review_id" ] && [ "$review_id" != "null" ] && yq -i '.gh.project_status_map.review = strenv(review_id)' "$config_file"
  [ -n "$done_id" ] && [ "$done_id" != "null" ] && yq -i '.gh.project_status_map.done = strenv(done_id)' "$config_file"

  echo "Detected status options:"
  [ -n "$backlog_id" ] && echo "  backlog -> $backlog_id" || echo "  backlog -> (not found)"
  [ -n "$inprog_id" ] && echo "  in_progress -> $inprog_id" || echo "  in_progress -> (not found)"
  [ -n "$review_id" ] && echo "  review -> $review_id" || echo "  review -> (not found)"
  [ -n "$done_id" ] && echo "  done -> $done_id" || echo "  done -> (not found)"
}

# Non-interactive: explicit flags provided
if [ "$FLAG_MODE" = true ]; then
  write_config
# Interactive: terminal attached
elif [ -t 0 ]; then
  # If repo already configured, skip the "Configure?" prompt
  if [ -n "${EXISTING_REPO:-}" ] && [ "$EXISTING_REPO" != "null" ]; then
    SETUP_GH="y"
  else
    read -r -p "Configure GitHub integration? (y/N): " SETUP_GH
  fi
  if [ "$SETUP_GH" = "y" ] || [ "$SETUP_GH" = "Y" ]; then
    DETECTED_REPO=${GH_REPO:-$(cd "$PROJECT_DIR" && git remote get-url origin 2>/dev/null | sed -E 's#(https://github.com/|git@github.com:)##; s#\.git$##' || true)}
    if [ -n "$DETECTED_REPO" ]; then
      read -r -p "GitHub repo (owner/repo) [$DETECTED_REPO]: " GH_REPO_INPUT
      GH_REPO="${GH_REPO_INPUT:-$DETECTED_REPO}"
    else
      read -r -p "GitHub repo (owner/repo): " GH_REPO
    fi
    if [ -n "$GH_REPO" ]; then
      write_config

      # Configure git credential helper to use gh OAuth token (enables HTTPS push)
      if command -v gh >/dev/null 2>&1; then
        gh auth setup-git 2>/dev/null || true
      fi

      # Interactive project selection if gh available and no project yet
      if command -v gh >/dev/null 2>&1 && [ -z "$GH_PROJECT_ID" ]; then
        read -r -p "GitHub Project ID [auto-detect if blank]: " GH_PROJECT_ID_INPUT

        if [ -z "$GH_PROJECT_ID_INPUT" ]; then
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

          if [ ${#projects[@]} -gt 0 ]; then
            echo "Detected GitHub Projects:"
            i=1
            for p in "${projects[@]}"; do
              echo "  [$i] $p"
              i=$((i + 1))
            done
            echo "  [n] Create new project"
            read -r -p "Select project number to use [skip]: " selection
            if [ "$selection" = "n" ] || [ "$selection" = "N" ]; then
              :  # fall through to create below
            elif [ -n "$selection" ] && [[ "$selection" =~ ^[0-9]+$ ]]; then
              idx=$((selection - 1))
              if [ $idx -ge 0 ] && [ $idx -lt ${#ids[@]} ]; then
                GH_PROJECT_ID_INPUT="${ids[$idx]}"
              fi
            fi
          fi

          # Create a new project if none selected
          if [ -z "${GH_PROJECT_ID_INPUT:-}" ]; then
            project_name=$(basename "$PROJECT_DIR" .git)
            read -r -p "Create a new GitHub Project? (y/N): " CREATE_PROJECT
            if [ "$CREATE_PROJECT" = "y" ] || [ "$CREATE_PROJECT" = "Y" ]; then
              read -r -p "Project title [$project_name]: " PROJECT_TITLE
              [ -z "$PROJECT_TITLE" ] && PROJECT_TITLE="$project_name"

              repo_owner=$(printf '%s' "$GH_REPO" | cut -d'/' -f1)
              # Determine if owner is an org or user
              owner_type=$(gh api "users/$repo_owner" -q .type 2>/dev/null || echo "User")
              if [ "$owner_type" = "Organization" ]; then
                create_json=$(gh api graphql -f query='mutation($org:ID!,$title:String!){ createProjectV2(input:{ownerId:$org, title:$title}){ projectV2{ id number } } }' \
                  -f org="$(gh api "orgs/$repo_owner" -q .node_id 2>/dev/null)" \
                  -f title="$PROJECT_TITLE" 2>/dev/null || true)
              else
                create_json=$(gh api graphql -f query='mutation($owner:ID!,$title:String!){ createProjectV2(input:{ownerId:$owner, title:$title}){ projectV2{ id number } } }' \
                  -f owner="$(gh api user -q .node_id 2>/dev/null)" \
                  -f title="$PROJECT_TITLE" 2>/dev/null || true)
              fi
              if [ -n "$create_json" ]; then
                new_id=$(printf '%s' "$create_json" | yq -r '.data.createProjectV2.projectV2.id // ""' 2>/dev/null)
                new_num=$(printf '%s' "$create_json" | yq -r '.data.createProjectV2.projectV2.number // ""' 2>/dev/null)
                if [ -n "$new_id" ] && [ "$new_id" != "null" ]; then
                  echo "Created project #$new_num: $PROJECT_TITLE"
                  configure_project_status_field "$new_id"
                  create_project_board_view "$new_num" "$repo_owner" "$owner_type"
                  link_project_to_repo "$new_id" "$GH_REPO"
                  if [ "$owner_type" = "Organization" ]; then
                    workflows_url="https://github.com/orgs/$repo_owner/projects/$new_num/workflows"
                  else
                    workflows_url="https://github.com/users/$repo_owner/projects/$new_num/workflows"
                  fi
                  echo ""
                  echo "To enable auto-add (new issues -> project), visit:"
                  echo "  $workflows_url"
                  echo "and enable 'Auto-add to project' for this repo."
                  GH_PROJECT_ID_INPUT="$new_id"
                else
                  echo "Failed to create project." >&2
                fi
              else
                echo "Failed to create project." >&2
              fi
            fi
          fi
        fi

        if [ -n "${GH_PROJECT_ID_INPUT:-}" ]; then
          GH_PROJECT_ID="$GH_PROJECT_ID_INPUT"
          export GH_PROJECT_ID
          yq -i ".gh.project_id = strenv(GH_PROJECT_ID)" "$CONFIG_FILE"
          auto_detect_status "$CONFIG_FILE" "$GH_PROJECT_ID"
        fi
      fi
    fi
  fi
fi

# Sync skills on first init
echo ""
echo "Syncing skills..."
"$SCRIPT_DIR/skills_sync.sh" 2>&1 || echo "skills sync failed (non-fatal)." >&2

echo ""
echo "Add tasks with: orchestrator add \"title\" \"body\" \"labels\""
echo "Start the server: orchestrator serve"

# Verify GitHub connection after init
if [ -n "${GH_REPO:-}" ] && command -v gh >/dev/null 2>&1; then
  echo ""
  ISSUE_COUNT=$(gh issue list --repo "$GH_REPO" --state open --limit 1 --json number -q 'length' 2>/dev/null || echo "?")
  echo "GitHub connected: $GH_REPO ($ISSUE_COUNT open issues)"
fi

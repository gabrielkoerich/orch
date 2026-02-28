//! GitHub Projects V2 integration — keeps project board columns in sync with task status.
//!
//! Uses GitHub's Projects V2 GraphQL API via the native `GhHttp` reqwest client.
//! All operations are best-effort: failures are logged but never block task execution.

use crate::backends::Status;
use crate::config;
use crate::github::http::GhHttp;
use std::collections::HashMap;

/// Project board info returned by list queries.
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub id: String,
    pub number: u64,
    pub title: String,
}

/// Keeps a GitHub Projects V2 board in sync with orch task statuses.
///
/// Config keys (in `~/.orch/config.yml`):
/// ```yaml
/// gh:
///   project_id: "PVT_kwHO..."
///   project_status_field_id: "PVTSSF_..."
///   project_status_map:
///     backlog: "option-id-1"
///     in_progress: "option-id-2"
///     review: "option-id-3"
///     done: "option-id-4"
/// ```
pub struct ProjectSync {
    project_id: String,
    status_field_id: String,
    status_map: HashMap<String, String>,
    gh: GhHttp,
}

impl ProjectSync {
    /// Load from config. Returns `None` if project integration is not configured.
    pub fn from_config() -> Option<Self> {
        let project_id = config::get("gh.project_id").ok()?;
        if project_id.is_empty() {
            return None;
        }
        let status_field_id = config::get("gh.project_status_field_id").ok()?;

        let mut status_map = HashMap::new();
        for key in &["backlog", "in_progress", "review", "done"] {
            if let Ok(val) = config::get(&format!("gh.project_status_map.{key}")) {
                status_map.insert(key.to_string(), val);
            }
        }

        Some(Self {
            project_id,
            status_field_id,
            status_map,
            gh: GhHttp::new(),
        })
    }

    /// Discover the Status field ID and option IDs from a project.
    ///
    /// Queries the project's fields via GraphQL and finds the single-select
    /// "Status" field, returning a `ProjectSync` populated with field/option IDs.
    pub async fn discover_fields(project_id: &str) -> anyhow::Result<Self> {
        let gh = GhHttp::new();
        let query = format!(
            r#"{{ node(id: "{}") {{ ... on ProjectV2 {{ fields(first: 100) {{ nodes {{ ... on ProjectV2SingleSelectField {{ id name options {{ id name }} }} }} }} }} }} }}"#,
            project_id
        );

        let result = gh.graphql(&query).await?;

        let fields = result
            .pointer("/data/node/fields/nodes")
            .and_then(|n| n.as_array())
            .ok_or_else(|| anyhow::anyhow!("failed to parse project fields response"))?;

        // Find the "Status" field (case-insensitive)
        let status_field = fields
            .iter()
            .find(|f| {
                f.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n.eq_ignore_ascii_case("status"))
                    .unwrap_or(false)
            })
            .ok_or_else(|| anyhow::anyhow!("no 'Status' field found in project"))?;

        let field_id = status_field
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Status field missing id"))?
            .to_string();

        let options = status_field
            .get("options")
            .and_then(|o| o.as_array())
            .ok_or_else(|| anyhow::anyhow!("Status field missing options"))?;

        // Map option names to their IDs, normalizing to our column keys
        let mut status_map = HashMap::new();
        for opt in options {
            let opt_id = opt.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let opt_name = opt.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let lower = opt_name.to_lowercase();

            let key = if lower.contains("backlog") || lower.contains("todo") || lower == "new" {
                "backlog"
            } else if lower.contains("progress") || lower.contains("doing") {
                "in_progress"
            } else if lower.contains("review") {
                "review"
            } else if lower.contains("done") || lower.contains("complete") {
                "done"
            } else {
                continue;
            };

            status_map.insert(key.to_string(), opt_id.to_string());
        }

        Ok(Self {
            project_id: project_id.to_string(),
            status_field_id: field_id,
            status_map,
            gh,
        })
    }

    /// List all accessible projects for the authenticated user and their orgs.
    pub async fn list_projects() -> anyhow::Result<Vec<ProjectInfo>> {
        let gh = GhHttp::new();
        let mut projects = Vec::new();

        // Get current user login
        let user = gh.get_whoami().await?;

        // User projects
        let query = format!(
            r#"{{ user(login: "{}") {{ projectsV2(first: 100) {{ nodes {{ id number title }} }} }} }}"#,
            user
        );
        if let Ok(result) = gh.graphql(&query).await {
            if let Some(nodes) = result
                .pointer("/data/user/projectsV2/nodes")
                .and_then(|n| n.as_array())
            {
                for node in nodes {
                    if let Some(info) = parse_project_node(node) {
                        projects.push(info);
                    }
                }
            }
        }

        // Try org projects for the repo owner (if different from user)
        if let Ok(repo) = config::get_current_repo() {
            if let Some(owner) = repo.split('/').next() {
                if owner != user {
                    let query = format!(
                        r#"{{ organization(login: "{}") {{ projectsV2(first: 100) {{ nodes {{ id number title }} }} }} }}"#,
                        owner
                    );
                    if let Ok(result) = gh.graphql(&query).await {
                        if let Some(nodes) = result
                            .pointer("/data/organization/projectsV2/nodes")
                            .and_then(|n| n.as_array())
                        {
                            for node in nodes {
                                if let Some(info) = parse_project_node(node) {
                                    projects.push(info);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(projects)
    }

    /// Create a new project under the given owner.
    ///
    /// `owner_id` is the GraphQL node ID of the user or org.
    #[allow(dead_code)]
    pub async fn create_project(owner_id: &str, title: &str) -> anyhow::Result<ProjectInfo> {
        let gh = GhHttp::new();
        let query = format!(
            r#"mutation {{ createProjectV2(input: {{ownerId: "{}", title: "{}"}}) {{ projectV2 {{ id number title }} }} }}"#,
            owner_id,
            title.replace('"', "\\\"")
        );

        let result = gh.graphql(&query).await?;
        let project = result
            .pointer("/data/createProjectV2/projectV2")
            .ok_or_else(|| anyhow::anyhow!("failed to create project"))?;

        parse_project_node(project)
            .ok_or_else(|| anyhow::anyhow!("failed to parse created project"))
    }

    /// Get the GraphQL node ID for a user or org login.
    #[allow(dead_code)]
    pub async fn get_owner_id(login: &str) -> anyhow::Result<String> {
        let gh = GhHttp::new();

        // Try user first
        let query = format!(r#"{{ user(login: "{}") {{ id }} }}"#, login);
        if let Ok(result) = gh.graphql(&query).await {
            if let Some(id) = result.pointer("/data/user/id").and_then(|v| v.as_str()) {
                return Ok(id.to_string());
            }
        }

        // Try org
        let query = format!(r#"{{ organization(login: "{}") {{ id }} }}"#, login);
        let result = gh.graphql(&query).await?;
        result
            .pointer("/data/organization/id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("could not find user or org: {login}"))
    }

    /// Add an issue to the project board. Returns the project item ID.
    pub async fn add_item(&self, issue_node_id: &str) -> anyhow::Result<String> {
        let query = format!(
            r#"mutation {{ addProjectV2ItemById(input: {{projectId: "{}", contentId: "{}"}}) {{ item {{ id }} }} }}"#,
            self.project_id, issue_node_id
        );

        let result = self.gh.graphql(&query).await?;
        result
            .pointer("/data/addProjectV2ItemById/item/id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("failed to add item to project"))
    }

    /// Update an item's status column on the project board.
    pub async fn update_item_status(&self, item_id: &str, status: &Status) -> anyhow::Result<()> {
        let column = Self::status_to_column(status);
        let option_id = self
            .status_map
            .get(column)
            .ok_or_else(|| anyhow::anyhow!("no option ID for column: {column}"))?;

        let query = format!(
            r#"mutation {{ updateProjectV2ItemFieldValue(input: {{projectId: "{}", itemId: "{}", fieldId: "{}", value: {{singleSelectOptionId: "{}"}}}}) {{ projectV2Item {{ id }} }} }}"#,
            self.project_id, item_id, self.status_field_id, option_id
        );

        self.gh.graphql(&query).await?;
        Ok(())
    }

    /// Sync an issue's project board status: add to project if needed, then update column.
    ///
    /// This is the main entry point called from `update_status()`.
    pub async fn sync_item_status(
        &self,
        issue_node_id: &str,
        status: &Status,
    ) -> anyhow::Result<()> {
        // Add item to project (idempotent — returns existing item if already added)
        let item_id = self.add_item(issue_node_id).await?;

        // Update the status column
        self.update_item_status(&item_id, status).await
    }

    /// Link a repository to the project.
    #[allow(dead_code)]
    pub async fn link_repo(&self, repo_id: &str) -> anyhow::Result<()> {
        let query = format!(
            r#"mutation {{ linkProjectV2ToRepository(input: {{projectId: "{}", repositoryId: "{}"}}) {{ repository {{ id }} }} }}"#,
            self.project_id, repo_id
        );

        // Linking may fail if already linked — that's fine
        match self.gh.graphql(&query).await {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("already") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Map orch `Status` to a project board column key.
    pub fn status_to_column(status: &Status) -> &'static str {
        match status {
            Status::New | Status::Routed => "backlog",
            Status::InProgress | Status::Blocked => "in_progress",
            Status::InReview | Status::NeedsReview => "review",
            Status::Done => "done",
        }
    }

    /// Get the project ID.
    #[allow(dead_code)]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// Get the status field ID.
    pub fn status_field_id(&self) -> &str {
        &self.status_field_id
    }

    /// Get the status map (column key → option ID).
    pub fn status_map(&self) -> &HashMap<String, String> {
        &self.status_map
    }
}

/// Parse a project node from GraphQL response.
fn parse_project_node(node: &serde_json::Value) -> Option<ProjectInfo> {
    Some(ProjectInfo {
        id: node.get("id")?.as_str()?.to_string(),
        number: node.get("number")?.as_u64()?,
        title: node.get("title")?.as_str()?.to_string(),
    })
}

/// Write project config fields to `~/.orch/config.yml`.
pub fn write_project_config(sync: &ProjectSync) -> anyhow::Result<()> {
    let config_path = crate::home::config_path()?;
    let content = if config_path.exists() {
        std::fs::read_to_string(&config_path)?
    } else {
        String::new()
    };

    // Parse existing YAML or start fresh
    let mut doc: serde_yml::Value = if content.is_empty() {
        serde_yml::Value::Mapping(serde_yml::Mapping::new())
    } else {
        serde_yml::from_str(&content)?
    };

    let root = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("config is not a YAML mapping"))?;

    // Ensure gh section exists
    let gh_key = serde_yml::Value::String("gh".to_string());
    if !root.contains_key(&gh_key) {
        root.insert(
            gh_key.clone(),
            serde_yml::Value::Mapping(serde_yml::Mapping::new()),
        );
    }
    let gh = root
        .get_mut(&gh_key)
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| anyhow::anyhow!("gh config is not a mapping"))?;

    // Set project fields
    gh.insert(
        serde_yml::Value::String("project_id".to_string()),
        serde_yml::Value::String(sync.project_id.clone()),
    );
    gh.insert(
        serde_yml::Value::String("project_status_field_id".to_string()),
        serde_yml::Value::String(sync.status_field_id.clone()),
    );

    // Build status map
    let mut map = serde_yml::Mapping::new();
    for (key, val) in &sync.status_map {
        map.insert(
            serde_yml::Value::String(key.clone()),
            serde_yml::Value::String(val.clone()),
        );
    }
    gh.insert(
        serde_yml::Value::String("project_status_map".to_string()),
        serde_yml::Value::Mapping(map),
    );

    std::fs::write(&config_path, serde_yml::to_string(&doc)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::Status;

    #[test]
    fn status_to_column_mapping() {
        assert_eq!(ProjectSync::status_to_column(&Status::New), "backlog");
        assert_eq!(ProjectSync::status_to_column(&Status::Routed), "backlog");
        assert_eq!(
            ProjectSync::status_to_column(&Status::InProgress),
            "in_progress"
        );
        assert_eq!(
            ProjectSync::status_to_column(&Status::Blocked),
            "in_progress"
        );
        assert_eq!(ProjectSync::status_to_column(&Status::InReview), "review");
        assert_eq!(
            ProjectSync::status_to_column(&Status::NeedsReview),
            "review"
        );
        assert_eq!(ProjectSync::status_to_column(&Status::Done), "done");
    }

    #[test]
    fn parse_project_node_valid() {
        let node = serde_json::json!({
            "id": "PVT_kwHOA123",
            "number": 42,
            "title": "My Project"
        });
        let info = parse_project_node(&node).unwrap();
        assert_eq!(info.id, "PVT_kwHOA123");
        assert_eq!(info.number, 42);
        assert_eq!(info.title, "My Project");
    }

    #[test]
    fn parse_project_node_missing_field() {
        let node = serde_json::json!({ "id": "PVT_kwHOA123" });
        assert!(parse_project_node(&node).is_none());
    }

    #[test]
    fn from_config_returns_none_when_not_configured() {
        // In test environment, config won't have gh.project_id
        assert!(ProjectSync::from_config().is_none());
    }
}

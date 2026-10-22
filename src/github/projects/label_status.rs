//! Opt-in, label-authoritative Project V2 status reconciliation.
//!
//! This is a projection of existing issue labels, not a task state machine:
//! it never edits labels, closes issues, changes membership, or dispatches work.

use super::super::http::GhHttp;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LabelSyncConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_max_updates")]
    max_updates_per_sync: usize,
    #[serde(default)]
    projects: Vec<BoardConfig>,
}

fn default_max_updates() -> usize {
    5
}

fn default_field_name() -> String {
    "Status".into()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoardConfig {
    repo: String,
    project_id: String,
    #[serde(default = "default_field_name")]
    field_name: String,
    /// Exact status label -> exact single-select option name on this board.
    options: BTreeMap<String, String>,
}

impl LabelSyncConfig {
    pub(crate) fn from_config() -> Result<Self> {
        match crate::config::get("gh.project_label_sync") {
            Ok(raw) => Self::parse(&raw),
            Err(error)
                if error.to_string().starts_with("config key not found:")
                    || error.to_string().starts_with("key not found:") =>
            {
                Ok(Self::default())
            }
            Err(error) => Err(error),
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        let config: Self =
            serde_norway::from_str(raw).context("invalid gh.project_label_sync configuration")?;
        if !config.enabled {
            return Ok(config);
        }
        if !(1..=100).contains(&config.max_updates_per_sync) {
            bail!("project label sync max_updates_per_sync must be 1..=100");
        }
        let mut boards = HashSet::new();
        for board in &config.projects {
            if board.repo.split('/').count() != 2
                || board
                    .repo
                    .split('/')
                    .any(|part| part.is_empty() || part.trim() != part)
                || board.project_id.trim().is_empty()
                || board.field_name.trim().is_empty()
                || board.options.is_empty()
            {
                bail!("project label sync requires repo, project_id, field_name and options");
            }
            if !boards.insert((board.repo.to_lowercase(), board.project_id.clone())) {
                bail!("duplicate project label sync configuration for the same repo and board");
            }
            for (label, option) in &board.options {
                if !label.starts_with("status:")
                    || label.len() == "status:".len()
                    || label.trim() != label
                    || option.trim().is_empty()
                {
                    bail!("project label sync options must map status:* labels to option names");
                }
            }
        }
        Ok(config)
    }

    /// Suppress the legacy internal-status projection only for boards explicitly
    /// managed by this configuration. A malformed configuration returns an error
    /// rather than silently falling back to a different source of truth.
    pub(crate) fn manages(&self, repo: &str, project_id: &str) -> bool {
        self.enabled
            && self.projects.iter().any(|board| {
                board.repo.eq_ignore_ascii_case(repo) && board.project_id == project_id
            })
    }

    pub(crate) fn configured_for_repo(&self, repo: &str) -> bool {
        self.enabled
            && self
                .projects
                .iter()
                .any(|board| board.repo.eq_ignore_ascii_case(repo))
    }

    pub(crate) async fn reconcile(
        &self,
        api: &impl ProjectApi,
        repo: &str,
        state: &mut LabelSyncState,
    ) -> Result<usize> {
        if !self.enabled {
            return Ok(0);
        }
        let mut updated = 0;
        for board in self
            .projects
            .iter()
            .filter(|b| b.repo.eq_ignore_ascii_case(repo))
        {
            updated +=
                reconcile_board(api, board, self.max_updates_per_sync - updated, state).await?;
            if updated == self.max_updates_per_sync {
                break;
            }
        }
        Ok(updated)
    }

    #[cfg(test)]
    async fn reconcile_with(&self, api: &impl ProjectApi, repo: &str) -> Result<usize> {
        self.reconcile(api, repo, &mut LabelSyncState::default())
            .await
    }
}

/// Per-backend safety state: stop repeatedly writing an unacknowledged update.
/// Every attempt still requires a fresh board read. A different desired option
/// or an observed matching value clears the item's consecutive failure streak.
#[derive(Default)]
pub(crate) struct LabelSyncState {
    unknown: HashMap<(String, String), (String, usize)>,
}

impl LabelSyncState {
    fn may_update(&mut self, project: &str, item: &str, option: &str) -> bool {
        let entry = self
            .unknown
            .entry((project.into(), item.into()))
            .or_insert_with(|| (option.into(), 0));
        if entry.0 != option {
            *entry = (option.into(), 0);
        }
        entry.1 < 3
    }

    fn record_unknown(&mut self, project: &str, item: &str, option: &str) {
        let entry = self
            .unknown
            .entry((project.into(), item.into()))
            .or_insert_with(|| (option.into(), 0));
        entry.1 += 1;
    }

    fn clear(&mut self, project: &str, item: &str) {
        self.unknown.remove(&(project.into(), item.into()));
    }
}

#[async_trait]
pub(crate) trait ProjectApi: Sync {
    async fn query(&self, query: &str, variables: Value) -> Result<Value>;
}

#[async_trait]
impl ProjectApi for GhHttp {
    async fn query(&self, query: &str, variables: Value) -> Result<Value> {
        let response = self.graphql_with_vars(query, variables).await?;
        // GhHttp permits partial GraphQL data. A projection must not interpret
        // partial label/field data as an authoritative snapshot.
        check_response(&response)?;
        Ok(response)
    }
}

fn check_response(response: &Value) -> Result<()> {
    if response.get("errors").is_some_and(|errors| {
        !errors.is_null() && errors.as_array().is_none_or(|items| !items.is_empty())
    }) {
        bail!("project label sync received GraphQL errors; refusing incomplete response");
    }
    Ok(())
}

const BOARD_QUERY: &str = r#"
query ProjectLabelStatus($project: ID!, $field: String!, $cursor: String) {
  node(id: $project) {
    ... on ProjectV2 {
      id closed
      fields(first: 100) {
        pageInfo { hasNextPage endCursor }
        nodes {
          ... on ProjectV2FieldCommon { id name }
          ... on ProjectV2SingleSelectField { options { id name } }
        }
      }
      items(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id isArchived
          fieldValueByName(name: $field) {
            ... on ProjectV2ItemFieldSingleSelectValue {
              optionId field { ... on ProjectV2SingleSelectField { id } }
            }
          }
          content {
            __typename
            ... on Issue {
              id repository { nameWithOwner }
              labels(first: 100) { nodes { name } pageInfo { hasNextPage endCursor } }
            }
          }
        }
      }
    }
  }
}"#;

const UPDATE_STATUS: &str = r#"
mutation ProjectLabelStatusUpdate($project: ID!, $item: ID!, $field: ID!, $option: String!) {
  updateProjectV2ItemFieldValue(input: {
    projectId: $project, itemId: $item, fieldId: $field,
    value: { singleSelectOptionId: $option }
  }) { projectV2Item { id } }
}"#;

const FIELDS_QUERY: &str = r#"
query ProjectLabelStatusFields($node: ID!, $cursor: String!) {
  node(id: $node) { ... on ProjectV2 {
    id fields(first: 100, after: $cursor) {
      pageInfo { hasNextPage endCursor }
      nodes {
          ... on ProjectV2FieldCommon { id name }
          ... on ProjectV2SingleSelectField { options { id name } }
        }
    }
  } }
}"#;

const LABELS_QUERY: &str = r#"
query ProjectLabelStatusLabels($node: ID!, $cursor: String!) {
  node(id: $node) { ... on Issue {
    id labels(first: 100, after: $cursor) {
      pageInfo { hasNextPage endCursor } nodes { name }
    }
  } }
}"#;

// A safety ceiling above GitHub's maximum Project item count. Detect both
// repeated and endlessly advancing malformed cursors without unbounded reads.
const MAX_CONNECTION_PAGES: usize = 1000;

async fn complete_connection(
    api: &impl ProjectApi,
    node: &mut Value,
    connection: &str,
    query: &str,
    node_id: &str,
) -> Result<()> {
    let mut seen = HashSet::new();
    loop {
        let page = &node[connection]["pageInfo"];
        match page["hasNextPage"].as_bool() {
            Some(false) => return Ok(()),
            Some(true) => {}
            None => bail!("missing {connection} pagination state"),
        }
        let cursor = page["endCursor"]
            .as_str()
            .filter(|value| !value.is_empty())
            .with_context(|| format!("missing {connection} pagination cursor"))?;
        if !seen.insert(cursor.to_owned()) || seen.len() > MAX_CONNECTION_PAGES {
            bail!("invalid or excessive {connection} pagination");
        }
        let response = api
            .query(query, json!({"node":node_id,"cursor":cursor}))
            .await?;
        check_response(&response)?;
        let next = response
            .pointer("/data/node")
            .context("missing pagination response")?;
        if next["id"].as_str() != Some(node_id) {
            bail!("pagination response belongs to a different node");
        }
        let rows = next[connection]["nodes"]
            .as_array()
            .context("missing pagination nodes")?;
        node[connection]["nodes"]
            .as_array_mut()
            .context("missing initial pagination nodes")?
            .extend(rows.iter().cloned());
        node[connection]["pageInfo"] = next[connection]["pageInfo"].clone();
    }
}

/// Resolve every configured option before writing anything on this board.
/// Names must match exactly and uniquely; no substring/nearest-column fallback.
fn resolve_options(
    board: &BoardConfig,
    project: &Value,
) -> Result<(String, BTreeMap<String, String>)> {
    if project.get("id").and_then(Value::as_str) != Some(board.project_id.as_str()) {
        bail!("project label sync response has no matching project");
    }
    let fields = project
        .pointer("/fields/nodes")
        .and_then(Value::as_array)
        .context("missing project fields")?;
    if project
        .pointer("/fields/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        != Some(false)
    {
        bail!("project fields incomplete");
    }
    let matching: Vec<_> = fields
        .iter()
        .filter(|field| {
            field.get("name").and_then(Value::as_str) == Some(board.field_name.as_str())
        })
        .collect();
    if matching.len() != 1 {
        bail!("project status field is missing or ambiguous");
    }
    let field = matching[0];
    let field_id = field
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("project status field missing id")?;
    let options = field
        .get("options")
        .and_then(Value::as_array)
        .context("project status field is not a single-select field")?;
    let mut resolved = BTreeMap::new();
    for (label, name) in &board.options {
        let matches: Vec<_> = options
            .iter()
            .filter(|option| option.get("name").and_then(Value::as_str) == Some(name.as_str()))
            .collect();
        if matches.len() != 1 {
            bail!("configured project status option is missing or ambiguous: {name}");
        }
        let id = matches[0]
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .context("project status option missing id")?;
        resolved.insert(label.clone(), id.to_owned());
    }
    Ok((field_id.to_owned(), resolved))
}

/// Returns only necessary updates. Missing/multiple/unmapped status labels do
/// not erase an existing field value. Archived items and other repositories
/// are not mutated; closed issues remain eligible for a display-only update.
fn desired_update<'a>(
    board: &BoardConfig,
    item: &'a Value,
    field_id: &str,
    options: &'a BTreeMap<String, String>,
) -> Result<Option<(&'a str, &'a str)>> {
    if item.get("isArchived").and_then(Value::as_bool) != Some(false) {
        return Ok(None);
    }
    let content = &item["content"];
    if content["__typename"].as_str() != Some("Issue")
        || !content["repository"]["nameWithOwner"]
            .as_str()
            .is_some_and(|repo| repo.eq_ignore_ascii_case(&board.repo))
    {
        return Ok(None);
    }
    let item_id = item["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("project item missing id")?;
    if content["labels"]["pageInfo"]["hasNextPage"].as_bool() != Some(false) {
        bail!("issue labels incomplete; leaving project item {item_id} unchanged");
    }
    let labels = content["labels"]["nodes"]
        .as_array()
        .context("issue labels missing")?;
    let mut statuses = Vec::new();
    for label in labels {
        let name = label["name"].as_str().context("issue label missing name")?;
        if name.starts_with("status:") {
            statuses.push(name);
        }
    }
    if statuses.is_empty() {
        return Ok(None);
    }
    if statuses.len() != 1 {
        bail!("multiple status labels; leaving project item {item_id} unchanged");
    }
    let desired = options.get(statuses[0]).with_context(|| {
        format!("unmapped status label; leaving project item {item_id} unchanged")
    })?;
    let current = item
        .get("fieldValueByName")
        .context("missing project field value response")?;
    if !current.is_null() {
        if current["field"]["id"].as_str() != Some(field_id) {
            bail!("project field value does not belong to the configured field");
        }
        let option_id = current["optionId"]
            .as_str()
            .context("project field value missing option id")?;
        if option_id == desired {
            return Ok(None);
        }
    }
    Ok(Some((item_id, desired.as_str())))
}

async fn reconcile_board(
    api: &impl ProjectApi,
    board: &BoardConfig,
    limit: usize,
    state: &mut LabelSyncState,
) -> Result<usize> {
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();
    let mut seen_items = HashSet::new();
    let mut updated = 0;
    loop {
        let response = api
            .query(
                BOARD_QUERY,
                json!({
                    "project": board.project_id, "field": board.field_name, "cursor": cursor,
                }),
            )
            .await?;
        check_response(&response)?;
        let mut project = response
            .pointer("/data/node")
            .context("missing project response")?
            .clone();
        // Closed Projects are user archives. Never reopen or rewrite them.
        if project["closed"].as_bool() == Some(true) {
            return Ok(updated);
        }
        if project["closed"].as_bool() != Some(false) {
            bail!("project response missing closed state");
        }
        complete_connection(api, &mut project, "fields", FIELDS_QUERY, &board.project_id).await?;
        let (field_id, options) = resolve_options(board, &project)?;
        let items = project["items"]["nodes"]
            .as_array()
            .context("missing project items")?;
        let page = &project["items"]["pageInfo"];
        let has_next = page["hasNextPage"]
            .as_bool()
            .context("missing project pagination state")?;
        let next = if has_next {
            let next = page["endCursor"]
                .as_str()
                .filter(|value| !value.is_empty())
                .context("missing project pagination cursor")?;
            if !seen_cursors.insert(next.to_owned()) || seen_cursors.len() > MAX_CONNECTION_PAGES {
                bail!("invalid or excessive project pagination");
            }
            Some(next.to_owned())
        } else {
            None
        };
        for original_item in items {
            let mut item = original_item.clone();
            if item["isArchived"].as_bool() == Some(false)
                && item["content"]["__typename"].as_str() == Some("Issue")
                && item["content"]["repository"]["nameWithOwner"]
                    .as_str()
                    .is_some_and(|repo| repo.eq_ignore_ascii_case(&board.repo))
            {
                let issue_id = item["content"]["id"]
                    .as_str()
                    .context("issue node id missing")?
                    .to_owned();
                complete_connection(api, &mut item["content"], "labels", LABELS_QUERY, &issue_id)
                    .await?;
            }
            let update = match desired_update(board, &item, &field_id, &options) {
                Ok(update) => update,
                Err(error) => {
                    tracing::warn!(%error, "project label status skipped");
                    continue;
                }
            };
            let Some((item_id, option_id)) = update else {
                if let Some(item_id) = item["id"].as_str() {
                    state.clear(&board.project_id, item_id);
                }
                continue;
            };
            if !state.may_update(&board.project_id, item_id, option_id) {
                tracing::warn!(item_id, "project status update suspended after three unknown outcomes; awaiting observed change or operator restart");
                continue;
            }
            if !seen_items.insert(item_id.to_owned()) {
                continue;
            }
            let result = api.query(UPDATE_STATUS, json!({
                "project": board.project_id, "item": item_id, "field": field_id, "option": option_id,
            })).await.and_then(|result| {
                check_response(&result)?;
                Ok(result)
            });
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    // Timeouts, malformed JSON and GraphQL errors may arrive
                    // after a write reached GitHub. Count every attempted but
                    // unacknowledged mutation, never a failed read.
                    state.record_unknown(&board.project_id, item_id, option_id);
                    return Err(error.context(
                        "project status update not acknowledged; reconciliation required",
                    ));
                }
            };
            if result
                .pointer("/data/updateProjectV2ItemFieldValue/projectV2Item/id")
                .and_then(Value::as_str)
                != Some(item_id)
            {
                // Unknown outcome: stop, then read fresh state on the next sync.
                state.record_unknown(&board.project_id, item_id, option_id);
                bail!("project status update was not acknowledged; reconciliation required");
            }
            state.clear(&board.project_id, item_id);
            updated += 1;
            if updated == limit {
                return Ok(updated);
            }
        }
        if !has_next {
            return Ok(updated);
        }
        cursor = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeApi {
        replies: Mutex<VecDeque<std::result::Result<Value, String>>>,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakeApi {
        fn new(replies: Vec<Value>) -> Self {
            Self {
                replies: Mutex::new(replies.into_iter().map(Ok).collect()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_errors(replies: Vec<std::result::Result<Value, String>>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn mutations(&self) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(query, _)| query.starts_with("\nmutation"))
                .map(|(_, vars)| vars.clone())
                .collect()
        }
    }

    #[async_trait]
    impl ProjectApi for FakeApi {
        async fn query(&self, query: &str, variables: Value) -> Result<Value> {
            self.calls.lock().unwrap().push((query.into(), variables));
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .context("unexpected API call")?
                .map_err(anyhow::Error::msg)
        }
    }

    fn config() -> LabelSyncConfig {
        LabelSyncConfig::parse(
            r#"
enabled: true
max_updates_per_sync: 20
projects:
  - repo: example/work
    project_id: PVT_A
    options:
      status:todo: Todo
      status:held: Held
      status:done: Done
"#,
        )
        .unwrap()
    }

    fn item(id: &str, labels: &[&str], option: Option<&str>) -> Value {
        json!({
            "id": id, "isArchived": false,
            "fieldValueByName": option.map(|option| json!({"optionId": option, "field": {"id": "FIELD_A"}})),
            "content": {
                "__typename": "Issue", "id": format!("ISSUE_{id}"),
                "repository": {"nameWithOwner": "example/work"},
                "labels": {"nodes": labels.iter().map(|name| json!({"name":name})).collect::<Vec<_>>(),
                    "pageInfo": {"hasNextPage": false}}
            }
        })
    }

    fn page(items: Vec<Value>, next: Option<&str>) -> Value {
        json!({"data":{"node":{
            "id":"PVT_A", "closed":false,
            "fields":{"nodes":[{"id":"FIELD_A","name":"Status","options":[
                {"id":"TODO_A","name":"Todo"}, {"id":"HELD_A","name":"Held"}, {"id":"DONE_A","name":"Done"}
            ]}], "pageInfo":{"hasNextPage":false,"endCursor":null}},
            "items":{"nodes":items,"pageInfo":{"hasNextPage":next.is_some(),"endCursor":next}}
        }}})
    }

    fn ack(id: &str) -> Value {
        json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":id}}}})
    }

    #[tokio::test]
    async fn repairs_missing_and_drifted_status_then_next_poll_is_noop() {
        let api = FakeApi::new(vec![
            page(
                vec![
                    item(
                        "ONE",
                        &["status:todo", "execution:held", "source-status:done"],
                        None,
                    ),
                    item("TWO", &["status:held"], Some("TODO_A")),
                ],
                None,
            ),
            ack("ONE"),
            ack("TWO"),
            page(
                vec![
                    item("ONE", &["status:todo", "execution:held"], Some("TODO_A")),
                    item("TWO", &["status:held"], Some("HELD_A")),
                ],
                None,
            ),
        ]);
        let config = config();
        assert_eq!(
            config.reconcile_with(&api, "example/work").await.unwrap(),
            2
        );
        assert_eq!(
            config.reconcile_with(&api, "example/work").await.unwrap(),
            0
        );
        assert_eq!(
            api.mutations(),
            vec![
                json!({"project":"PVT_A","item":"ONE","field":"FIELD_A","option":"TODO_A"}),
                json!({"project":"PVT_A","item":"TWO","field":"FIELD_A","option":"HELD_A"}),
            ]
        );
        // Only field mutations: no issue state, label, or membership operations.
        assert!(api
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|(query, _)| query == BOARD_QUERY || query == UPDATE_STATUS));
    }

    #[tokio::test]
    async fn label_change_moves_existing_item_without_local_task_transition() {
        let api = FakeApi::new(vec![
            page(vec![item("ONE", &["status:todo"], Some("TODO_A"))], None),
            page(
                vec![item(
                    "ONE",
                    &["status:held", "execution:held"],
                    Some("TODO_A"),
                )],
                None,
            ),
            ack("ONE"),
            page(vec![item("ONE", &["status:done"], Some("HELD_A"))], None),
            ack("ONE"),
        ]);
        let config = config();
        assert_eq!(
            config.reconcile_with(&api, "example/work").await.unwrap(),
            0
        );
        assert_eq!(
            config.reconcile_with(&api, "example/work").await.unwrap(),
            1
        );
        assert_eq!(
            config.reconcile_with(&api, "example/work").await.unwrap(),
            1
        );
        assert_eq!(api.mutations()[0]["option"], "HELD_A");
        assert_eq!(api.mutations()[1]["option"], "DONE_A");
    }

    #[tokio::test]
    async fn traverses_pages_and_does_not_write_duplicate_item() {
        let api = FakeApi::new(vec![
            page(vec![item("ONE", &["status:held"], None)], Some("PAGE2")),
            ack("ONE"),
            page(
                vec![
                    item("ONE", &["status:held"], None),
                    item("TWO", &["status:todo"], None),
                ],
                None,
            ),
            ack("TWO"),
        ]);
        assert_eq!(
            config().reconcile_with(&api, "example/work").await.unwrap(),
            2
        );
        let calls = api.calls.lock().unwrap();
        assert!(calls[0].1["cursor"].is_null());
        assert_eq!(calls[2].1["cursor"], "PAGE2");
    }

    #[tokio::test]
    async fn ignores_missing_ambiguous_unknown_and_nonworkflow_labels() {
        let api = FakeApi::new(vec![page(
            vec![
                item(
                    "ONE",
                    &["source-status:held", "execution:held", "agent:worker"],
                    Some("TODO_A"),
                ),
                item("TWO", &["status:todo", "status:held"], Some("DONE_A")),
                item("THREE", &["status:unknown"], None),
            ],
            None,
        )]);
        assert_eq!(
            config().reconcile_with(&api, "example/work").await.unwrap(),
            0
        );
        assert!(api.mutations().is_empty());
    }

    #[tokio::test]
    async fn scopes_items_to_repo_and_preserves_archived_items_and_prs() {
        let mut foreign = item("FOREIGN", &["status:todo"], None);
        foreign["content"]["repository"]["nameWithOwner"] = json!("another/repo");
        let mut archived = item("ARCHIVED", &["status:todo"], None);
        archived["isArchived"] = json!(true);
        let mut pr = item("PR", &["status:todo"], None);
        pr["content"]["__typename"] = json!("PullRequest");
        let api = FakeApi::new(vec![page(
            vec![
                foreign,
                archived,
                pr,
                json!({"id":"DRAFT","isArchived":false,"content":null}),
            ],
            None,
        )]);
        assert_eq!(
            config().reconcile_with(&api, "example/work").await.unwrap(),
            0
        );
        assert!(api.mutations().is_empty());
    }

    #[tokio::test]
    async fn incomplete_labels_and_wrong_field_values_are_not_authoritative() {
        let mut incomplete = item("ONE", &["status:todo"], None);
        incomplete["content"]["labels"]["nodes"] = json!([{}]);
        let mut wrong_field = item("TWO", &["status:todo"], Some("DONE_A"));
        wrong_field["fieldValueByName"]["field"]["id"] = json!("OTHER_FIELD");
        let api = FakeApi::new(vec![page(vec![incomplete, wrong_field], None)]);
        assert_eq!(
            config().reconcile_with(&api, "example/work").await.unwrap(),
            0
        );
        assert!(api.mutations().is_empty());
    }

    #[tokio::test]
    async fn missing_or_ambiguous_option_stops_board_before_writing() {
        for options in [
            json!([]),
            json!([
                {"id":"A","name":"Todo"}, {"id":"B","name":"Todo"},
                {"id":"C","name":"Held"}, {"id":"D","name":"Done"}
            ]),
        ] {
            let mut response = page(vec![item("ONE", &["status:todo"], None)], None);
            response["data"]["node"]["fields"]["nodes"][0]["options"] = options;
            let api = FakeApi::new(vec![response]);
            assert!(config().reconcile_with(&api, "example/work").await.is_err());
            assert!(api.mutations().is_empty());
        }
    }

    #[tokio::test]
    async fn partial_graphql_error_prevents_writes() {
        let mut response = page(vec![item("ONE", &["status:todo"], None)], None);
        response["errors"] = json!([{"message":"partial label permission failure"}]);
        let api = FakeApi::new(vec![response]);
        assert!(config().reconcile_with(&api, "example/work").await.is_err());
        assert!(api.mutations().is_empty());
    }

    #[tokio::test]
    async fn unknown_mutation_outcome_stops_and_next_poll_reconciles() {
        let api = FakeApi::new(vec![
            page(
                vec![
                    item("ONE", &["status:todo"], None),
                    item("TWO", &["status:todo"], None),
                ],
                None,
            ),
            json!({"data":{"updateProjectV2ItemFieldValue":null}}),
            page(
                vec![
                    item("ONE", &["status:todo"], Some("TODO_A")),
                    item("TWO", &["status:todo"], None),
                ],
                None,
            ),
            ack("TWO"),
        ]);
        let config = config();
        assert!(config.reconcile_with(&api, "example/work").await.is_err());
        assert_eq!(api.mutations().len(), 1);
        assert_eq!(
            config.reconcile_with(&api, "example/work").await.unwrap(),
            1
        );
        assert_eq!(api.mutations()[1]["item"], "TWO");
    }

    #[tokio::test]
    async fn bounded_updates_resume_from_fresh_board_state() {
        let mut config = config();
        config.max_updates_per_sync = 1;
        let api = FakeApi::new(vec![
            page(
                vec![
                    item("ONE", &["status:todo"], None),
                    item("TWO", &["status:todo"], None),
                ],
                None,
            ),
            ack("ONE"),
            page(
                vec![
                    item("ONE", &["status:todo"], Some("TODO_A")),
                    item("TWO", &["status:todo"], None),
                ],
                None,
            ),
            ack("TWO"),
        ]);
        assert_eq!(
            config.reconcile_with(&api, "example/work").await.unwrap(),
            1
        );
        assert_eq!(api.mutations().len(), 1);
        assert_eq!(
            config.reconcile_with(&api, "example/work").await.unwrap(),
            1
        );
        assert_eq!(api.mutations()[1]["item"], "TWO");
    }

    #[tokio::test]
    async fn multiple_projects_resolve_their_own_field_and_option_ids() {
        let mut config = config();
        config.projects.push(BoardConfig {
            repo: "example/work".into(),
            project_id: "PVT_B".into(),
            field_name: "Status".into(),
            options: BTreeMap::from([("status:todo".into(), "Todo".into())]),
        });
        let mut second = page(vec![item("TWO", &["status:todo"], None)], None);
        second["data"]["node"]["id"] = json!("PVT_B");
        second["data"]["node"]["fields"]["nodes"][0]["id"] = json!("FIELD_B");
        second["data"]["node"]["fields"]["nodes"][0]["options"][0]["id"] = json!("TODO_B");
        let api = FakeApi::new(vec![
            page(vec![item("ONE", &["status:todo"], None)], None),
            ack("ONE"),
            second,
            ack("TWO"),
        ]);
        assert_eq!(
            config.reconcile_with(&api, "example/work").await.unwrap(),
            2
        );
        assert_eq!(
            api.mutations()[1],
            json!({"project":"PVT_B","item":"TWO","field":"FIELD_B","option":"TODO_B"})
        );
    }

    #[tokio::test]
    async fn disabled_wrong_repo_and_closed_projects_make_no_mutations() {
        let mut config = config();
        config.enabled = false;
        let no_calls = FakeApi::new(vec![]);
        assert_eq!(
            config
                .reconcile_with(&no_calls, "example/work")
                .await
                .unwrap(),
            0
        );
        config.enabled = true;
        assert_eq!(
            config
                .reconcile_with(&no_calls, "other/repo")
                .await
                .unwrap(),
            0
        );
        assert!(no_calls.calls.lock().unwrap().is_empty());
        let mut closed = page(vec![item("ONE", &["status:todo"], None)], None);
        closed["data"]["node"]["closed"] = json!(true);
        let api = FakeApi::new(vec![closed]);
        assert_eq!(
            config.reconcile_with(&api, "example/work").await.unwrap(),
            0
        );
        assert!(api.mutations().is_empty());
    }

    #[tokio::test]
    async fn invalid_cursor_stops_instead_of_looping() {
        let api = FakeApi::new(vec![page(vec![], Some("SAME")), page(vec![], Some("SAME"))]);
        assert!(config().reconcile_with(&api, "example/work").await.is_err());
        assert_eq!(api.calls.lock().unwrap().len(), 2);
        assert!(api.mutations().is_empty());
    }

    #[tokio::test]
    async fn paginates_fields_and_labels_before_resolving_status() {
        let mut first = page(vec![item("ONE", &["execution:held"], None)], None);
        let field = first["data"]["node"]["fields"]["nodes"][0].take();
        first["data"]["node"]["fields"] =
            json!({"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":"FIELDS2"}});
        first["data"]["node"]["items"]["nodes"][0]["content"]["labels"]["pageInfo"] =
            json!({"hasNextPage":true,"endCursor":"LABELS2"});
        let api = FakeApi::new(vec![
            first,
            json!({"data":{"node":{"id":"PVT_A","fields":{"nodes":[field],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}),
            json!({"data":{"node":{"id":"ISSUE_ONE","labels":{"nodes":[{"name":"status:held"}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}),
            ack("ONE"),
        ]);
        assert_eq!(
            config().reconcile_with(&api, "example/work").await.unwrap(),
            1
        );
        assert_eq!(api.mutations()[0]["option"], "HELD_A");
        let calls = api.calls.lock().unwrap();
        assert_eq!(calls[1].1, json!({"node":"PVT_A","cursor":"FIELDS2"}));
        assert_eq!(calls[2].1, json!({"node":"ISSUE_ONE","cursor":"LABELS2"}));
    }

    #[tokio::test]
    async fn second_page_status_label_makes_ambiguous_issue_a_noop() {
        let mut first = page(vec![item("ONE", &["status:todo"], None)], None);
        first["data"]["node"]["items"]["nodes"][0]["content"]["labels"]["pageInfo"] =
            json!({"hasNextPage":true,"endCursor":"LABELS2"});
        let api = FakeApi::new(vec![
            first,
            json!({"data":{"node":{"id":"ISSUE_ONE","labels":{"nodes":[{"name":"status:held"}],"pageInfo":{"hasNextPage":false}}}}}),
        ]);
        assert_eq!(
            config().reconcile_with(&api, "example/work").await.unwrap(),
            0
        );
        assert!(api.mutations().is_empty());
    }

    #[tokio::test]
    async fn duplicate_field_names_across_pages_are_not_guessed() {
        let mut first = page(vec![item("ONE", &["status:todo"], None)], None);
        let mut field = first["data"]["node"]["fields"]["nodes"][0].clone();
        field["id"] = json!("OTHER_FIELD");
        first["data"]["node"]["fields"]["pageInfo"] =
            json!({"hasNextPage":true,"endCursor":"FIELDS2"});
        let api = FakeApi::new(vec![
            first,
            json!({"data":{"node":{"id":"PVT_A","fields":{"nodes":[field],"pageInfo":{"hasNextPage":false}}}}}),
        ]);
        assert!(config().reconcile_with(&api, "example/work").await.is_err());
        assert!(api.mutations().is_empty());
    }

    #[tokio::test]
    async fn incomplete_label_pages_stop_before_mutation() {
        for response in [
            json!({"data":{"node":null}}),
            json!({"data":{"node":{"id":"OTHER_ISSUE","labels":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}),
            json!({"data":{"node":{"id":"ISSUE_ONE","labels":{"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":"LABELS2"}}}}}),
        ] {
            let mut first = page(vec![item("ONE", &["status:todo"], None)], None);
            first["data"]["node"]["items"]["nodes"][0]["content"]["labels"]["pageInfo"] =
                json!({"hasNextPage":true,"endCursor":"LABELS2"});
            let api = FakeApi::new(vec![first, response]);
            assert!(config().reconcile_with(&api, "example/work").await.is_err());
            assert!(api.mutations().is_empty());
        }
    }

    #[tokio::test]
    async fn repeated_unknown_updates_are_bounded_without_starving_later_items() {
        let mut replies = Vec::new();
        for _ in 0..3 {
            replies.push(page(vec![item("ONE", &["status:todo"], None)], None));
            replies.push(json!({"data":{"updateProjectV2ItemFieldValue":null}}));
        }
        replies.push(page(
            vec![
                item("ONE", &["status:todo"], None),
                item("TWO", &["status:held"], None),
            ],
            None,
        ));
        replies.push(ack("TWO"));
        // A new desired option explicitly resets the consecutive unknown result streak.
        replies.push(page(vec![item("ONE", &["status:held"], None)], None));
        replies.push(ack("ONE"));
        let api = FakeApi::new(replies);
        let mut state = LabelSyncState::default();
        let config = config();
        for _ in 0..3 {
            assert!(config
                .reconcile(&api, "example/work", &mut state)
                .await
                .is_err());
        }
        assert_eq!(api.mutations().len(), 3);
        assert_eq!(
            config
                .reconcile(&api, "example/work", &mut state)
                .await
                .unwrap(),
            1
        );
        assert_eq!(api.mutations()[3]["item"], "TWO");
        assert_eq!(
            config
                .reconcile(&api, "example/work", &mut state)
                .await
                .unwrap(),
            1
        );
        assert_eq!(api.mutations()[4]["item"], "ONE");
        assert!(state.unknown.is_empty());
    }

    #[tokio::test]
    async fn failed_mutation_responses_are_bounded_but_read_failures_are_not_counted() {
        for failure in [
            Err("transport timeout after sending write".into()),
            Err("malformed JSON in HTTP 200 response".into()),
            Ok(
                json!({"data":{"updateProjectV2ItemFieldValue":null},"errors":[{"message":"partial response"}]}),
            ),
        ] {
            let mut replies = vec![Err("read failed".into())];
            for _ in 0..3 {
                replies.push(Ok(page(vec![item("ONE", &["status:todo"], None)], None)));
                replies.push(failure.clone());
            }
            replies.push(Ok(page(
                vec![
                    item("ONE", &["status:todo"], None),
                    item("TWO", &["status:held"], None),
                ],
                None,
            )));
            replies.push(Ok(ack("TWO")));
            let api = FakeApi::with_errors(replies);
            let mut state = LabelSyncState::default();
            let config = config();
            assert!(config
                .reconcile(&api, "example/work", &mut state)
                .await
                .is_err());
            assert!(state.unknown.is_empty());
            for _ in 0..3 {
                assert!(config
                    .reconcile(&api, "example/work", &mut state)
                    .await
                    .is_err());
            }
            assert_eq!(api.mutations().len(), 3);
            assert_eq!(
                config
                    .reconcile(&api, "example/work", &mut state)
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(api.mutations()[3]["item"], "TWO");
        }
    }

    #[tokio::test]
    async fn board_reconcile_cli_uses_same_display_only_path_and_truthful_result() {
        let api = FakeApi::new(vec![
            page(
                vec![item("ONE", &["status:held", "execution:held"], None)],
                None,
            ),
            ack("ONE"),
        ]);
        assert_eq!(
            crate::cli::board_reconcile_with(&config(), &api, "example/work")
                .await
                .unwrap(),
            1
        );
        assert!(api.replies.lock().unwrap().is_empty());
        assert!(api
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|(query, _)| query == BOARD_QUERY || query == UPDATE_STATUS));
        let no_calls = FakeApi::new(vec![]);
        assert!(crate::cli::board_reconcile_with(
            &LabelSyncConfig::default(),
            &no_calls,
            "example/work"
        )
        .await
        .is_err());
        assert!(
            crate::cli::board_reconcile_with(&config(), &no_calls, "other/repo")
                .await
                .is_err()
        );
        assert!(no_calls.calls.lock().unwrap().is_empty());
        let noop = FakeApi::new(vec![page(
            vec![item("ONE", &["status:todo"], Some("TODO_A"))],
            None,
        )]);
        assert_eq!(
            crate::cli::board_reconcile_with(&config(), &noop, "example/work")
                .await
                .unwrap(),
            0
        );
        let unknown = FakeApi::new(vec![
            page(vec![item("ONE", &["status:held"], None)], None),
            json!({"data":null}),
        ]);
        assert!(
            crate::cli::board_reconcile_with(&config(), &unknown, "example/work")
                .await
                .is_err()
        );
    }

    #[test]
    fn invalid_configuration_is_rejected_without_legacy_fallback() {
        assert!(LabelSyncConfig::parse("enabled: true\nmax_updates_per_sync: 0").is_err());
        assert!(LabelSyncConfig::parse("enabled: true\nmax_updates_per_sync: 101").is_err());
        assert!(LabelSyncConfig::parse("enabled: true\nunknown: true").is_err());
        assert!(LabelSyncConfig::parse("enabled: true\nprojects:\n  - repo: example/work\n    project_id: PVT_A\n    options:\n      execution:held: Held").is_err());
        assert!(LabelSyncConfig::parse("enabled: true\nprojects:\n  - repo: example/work\n    project_id: PVT_A\n    options:\n      source-status:todo: Todo").is_err());
        let config = config();
        assert!(config.manages("EXAMPLE/WORK", "PVT_A"));
        assert!(!config.manages("other/repo", "PVT_A"));
        assert!(!config.manages("example/work", "PVT_B"));
    }
}

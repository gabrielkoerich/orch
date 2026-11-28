//! CLI task references and their resolution

use crate::store::TaskStore;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use std::path::{MAIN_SEPARATOR_STR, Path, PathBuf};

/// How an operator names a task on the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliTaskSpec {
    /// `internal:58`
    Internal(i64),
    /// `owner/repo#58`
    Issue { repo: String, number: i64 },
    /// `58` or `#58`
    Bare(i64),
}

/// A task picked by [`TaskStore::resolve_cli_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliTaskRef {
    pub store_id: Option<i64>,
    pub repo: String,
    pub origin: String,
    pub title: String,
    pub external_id: String,
    pub status: String,
}

impl CliTaskRef {
    /// The store id, or an error naming the task when it has no store row
    pub fn require_store_id(&self) -> anyhow::Result<i64> {
        self.store_id
            .ok_or_else(|| anyhow::anyhow!("{} is not in the store", self.display_spec()))
    }

    /// The review dir and the task's own dir under a state root. The runner names a task dir after
    /// its external id, which is `internal:<store id>` for an internal task
    pub fn state_dirs_under(&self, state: &Path) -> (PathBuf, PathBuf) {
        let tasks = state
            .join(self.repo.replace('/', MAIN_SEPARATOR_STR))
            .join("tasks");
        let name = &self.external_id;
        (tasks.join(format!("{name}-review")), tasks.join(name))
    }

    pub fn is_internal(&self) -> bool {
        self.origin == "internal"
    }

    pub fn unsynced_issue(repo: String, number: i64) -> Self {
        Self {
            store_id: None,
            repo,
            origin: "github".to_string(),
            title: String::new(),
            external_id: number.to_string(),
            status: String::new(),
        }
    }

    pub fn display_spec(&self) -> String {
        if self.is_internal() {
            match self.store_id {
                Some(id) => format!("internal:{id}"),
                None => self.external_id.clone(),
            }
        } else {
            format!("{}#{}", self.repo, self.external_id)
        }
    }

    fn from_row(row: &SqliteRow) -> anyhow::Result<Self> {
        let id: i64 = row.try_get("id")?;
        let origin: String = row.try_get("origin")?;
        let external_id: Option<String> = row.try_get("external_id")?;
        let external_id = external_id.unwrap_or_else(|| {
            if origin == "internal" {
                format!("internal:{id}")
            } else {
                String::new()
            }
        });
        Ok(Self {
            store_id: Some(id),
            repo: row.try_get("repo")?,
            origin,
            title: row.try_get("title")?,
            external_id,
            status: row.try_get("status")?,
        })
    }

    fn format_match(&self) -> String {
        format!(
            "  {}  [{}]  {}",
            self.display_spec(),
            if self.status.is_empty() {
                "unknown"
            } else {
                &self.status
            },
            if self.title.is_empty() {
                "-"
            } else {
                &self.title
            }
        )
    }
}

/// Parse `internal:N`, `owner/repo#N`, `#N`, or a bare number.
pub fn parse_cli_task_spec(raw: &str) -> anyhow::Result<CliTaskSpec> {
    let spec = raw.trim();
    if spec.is_empty() {
        anyhow::bail!("empty task id");
    }
    if let Some(suffix) = spec.strip_prefix("internal:") {
        let number: i64 = suffix
            .parse()
            .map_err(|_| anyhow::anyhow!("internal task id '{suffix}' is not numeric"))?;
        if number <= 0 {
            anyhow::bail!("internal task id must be a positive number");
        }
        return Ok(CliTaskSpec::Internal(number));
    }
    if let Some((repo, number)) = parse_repo_issue(spec) {
        return Ok(CliTaskSpec::Issue { repo, number });
    }
    let bare = spec.strip_prefix('#').unwrap_or(spec);
    if let Ok(number) = bare.parse::<i64>()
        && number > 0
    {
        return Ok(CliTaskSpec::Bare(number));
    }
    anyhow::bail!("invalid task id '{spec}': use a number, owner/repo#N, or internal:N")
}

fn parse_repo_issue(spec: &str) -> Option<(String, i64)> {
    let (repo, number) = spec.rsplit_once('#')?;
    if repo.is_empty() {
        return None;
    }
    let number: i64 = number.parse().ok()?;
    if number <= 0 {
        return None;
    }
    let mut parts = repo.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return None;
    }
    let ok = |part: &str| {
        part.chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
    };
    if !ok(owner) || !ok(name) {
        return None;
    }
    Some((repo.to_string(), number))
}

fn ambiguous_cli_id_error(spec: &str, matches: &[CliTaskRef]) -> anyhow::Error {
    let mut lines = vec![format!("task ID {spec} matches more than one task:")];
    for task in matches {
        lines.push(task.format_match());
    }
    lines.push(ambiguous_cli_id_hint(matches));
    anyhow::anyhow!(lines.join("\n"))
}

fn ambiguous_cli_id_hint(matches: &[CliTaskRef]) -> String {
    let github = matches
        .iter()
        .find(|task| !task.is_internal())
        .map(CliTaskRef::display_spec);
    let internal = matches
        .iter()
        .find(|task| task.is_internal())
        .map(CliTaskRef::display_spec);
    match (github, internal) {
        (Some(github), Some(internal)) => {
            format!("Use {github} for the GitHub task or {internal} for the internal one")
        }
        _ => "Use owner/repo#N for an issue, or internal:N for an internal task.".to_string(),
    }
}

fn dedup_cli_refs(rows: Vec<SqliteRow>) -> anyhow::Result<Vec<CliTaskRef>> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for row in rows {
        let task = CliTaskRef::from_row(&row)?;
        if let Some(id) = task.store_id {
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
        }
        out.push(task);
    }
    Ok(out)
}

/// A reference to an issue the store has not synced yet, or the error for a task that is not there
fn unsynced_ref(spec: &str, current_repo: Option<&str>, shown: &str) -> anyhow::Result<CliTaskRef> {
    match parse_cli_task_spec(spec)? {
        CliTaskSpec::Issue { repo, number } => Ok(CliTaskRef::unsynced_issue(repo, number)),
        CliTaskSpec::Bare(number) => {
            let repo = current_repo
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("task not found: {shown}"))?;
            Ok(CliTaskRef::unsynced_issue(repo, number))
        }
        CliTaskSpec::Internal(_) => anyhow::bail!("internal task '{shown}' not found"),
    }
}

impl TaskStore {
    /// The task `spec` names, or for an issue number the store has not synced yet, a reference to
    /// it. `shown` is the id as the operator typed it, for the error
    pub async fn resolve_cli_ref(
        &self,
        spec: &str,
        current_repo: Option<&str>,
        shown: &str,
    ) -> anyhow::Result<CliTaskRef> {
        match self.resolve_cli_id(spec, current_repo).await? {
            Some(task) => Ok(task),
            None => unsynced_ref(spec, current_repo, shown),
        }
    }

    /// Resolve a task_id (e.g. "42" or "internal:3") to a store internal ID.
    /// Returns None if the task is not in the store yet.
    pub async fn resolve_task_id(&self, repo: &str, task_id: &str) -> anyhow::Result<Option<i64>> {
        if let Some(suffix) = task_id.strip_prefix("internal:") {
            // Internal tasks: look up by external_id="internal:{n}" with origin='internal'
            if let Some(id) = self.resolve_id_by_external(repo, task_id).await? {
                return Ok(Some(id));
            }
            // Fallback: try the numeric suffix as a direct store ID.
            // Must include the repo filter to avoid resolving a task that
            // belongs to a different repository in a multi-repo setup.
            if let Ok(id) = suffix.parse::<i64>() {
                let exists = sqlx::query("SELECT id FROM tasks WHERE id = ? AND repo = ?")
                    .bind(id)
                    .bind(repo)
                    .fetch_optional(&self.pool)
                    .await?;
                if exists.is_some() {
                    return Ok(Some(id));
                }
            }
            return Ok(None);
        }
        // External tasks: look up by external_id using the lightweight id-only
        // lookup to avoid deserializing the full Task row.
        self.resolve_id_by_external(repo, task_id).await
    }

    /// Resolve a task_id globally (across all repos) when a repo-scoped lookup fails.
    ///
    /// Returns `(internal_id, repo)` if found unambiguously, `None` if not found,
    /// or an error if the external_id matches tasks in multiple repos (GitHub issue
    /// numbers are not globally unique).
    pub async fn resolve_task_id_globally(
        &self,
        task_id: &str,
    ) -> anyhow::Result<Option<(i64, String)>> {
        if let Some(suffix) = task_id.strip_prefix("internal:") {
            // internal:N — external_id is globally unique; primary key is also unique.
            let row = sqlx::query("SELECT id, repo FROM tasks WHERE external_id = ?")
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await?;
            if let Some(r) = row {
                return Ok(Some((r.try_get("id")?, r.try_get("repo")?)));
            }
            if let Ok(id) = suffix.parse::<i64>() {
                let row = sqlx::query("SELECT id, repo FROM tasks WHERE id = ?")
                    .bind(id)
                    .fetch_optional(&self.pool)
                    .await?;
                if let Some(r) = row {
                    return Ok(Some((r.try_get("id")?, r.try_get("repo")?)));
                }
            }
            return Ok(None);
        }
        // External IDs (e.g. GitHub issue numbers) may collide across repos.
        let rows = sqlx::query("SELECT id, repo FROM tasks WHERE external_id = ?")
            .bind(task_id)
            .fetch_all(&self.pool)
            .await?;
        match rows.len() {
            0 => Ok(None),
            1 => Ok(Some((rows[0].try_get("id")?, rows[0].try_get("repo")?))),
            _ => {
                let repos: Vec<String> = rows
                    .iter()
                    .filter_map(|r| r.try_get::<String, _>("repo").ok())
                    .collect();
                anyhow::bail!(
                    "task ID {task_id:?} is ambiguous — found in multiple repos: {}. \
                     Run from the repo worktree so the current-repo lookup resolves it.",
                    repos.join(", ")
                )
            }
        }
    }

    /// Resolve a CLI task id. A bare number in a project is that project's issue first
    pub async fn resolve_cli_id(
        &self,
        spec: &str,
        current_repo: Option<&str>,
    ) -> anyhow::Result<Option<CliTaskRef>> {
        let current_repo = current_repo.filter(|repo| !repo.is_empty());
        match parse_cli_task_spec(spec)? {
            CliTaskSpec::Internal(number) => self.lookup_internal_cli_id(number).await,
            CliTaskSpec::Issue { repo, number } => self.lookup_issue_cli_id(&repo, number).await,
            CliTaskSpec::Bare(number) => {
                if let Some(repo) = current_repo {
                    if let Some(task) = self.lookup_issue_cli_id(repo, number).await? {
                        return Ok(Some(task));
                    }
                    if let Some(task) = self.lookup_internal_in_repo(number, repo).await? {
                        return Ok(Some(task));
                    }
                    return Ok(None);
                }
                let mut matches = self.lookup_bare_cli_id_matches(number).await?;
                match matches.len() {
                    0 => Ok(None),
                    1 => Ok(matches.pop()),
                    _ => Err(ambiguous_cli_id_error(spec, &matches)),
                }
            }
        }
    }

    async fn lookup_internal_cli_id(&self, number: i64) -> anyhow::Result<Option<CliTaskRef>> {
        let ext = format!("internal:{number}");
        let rows = sqlx::query(
            "SELECT id, repo, origin, title, external_id, status FROM tasks \
             WHERE origin = 'internal' AND (id = ? OR external_id = ?) \
             ORDER BY id",
        )
        .bind(number)
        .bind(&ext)
        .fetch_all(&self.pool)
        .await?;
        let mut matches = dedup_cli_refs(rows)?;
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.pop()),
            _ => Err(ambiguous_cli_id_error(&ext, &matches)),
        }
    }

    async fn lookup_internal_in_repo(
        &self,
        number: i64,
        repo: &str,
    ) -> anyhow::Result<Option<CliTaskRef>> {
        let row = sqlx::query(
            "SELECT id, repo, origin, title, external_id, status FROM tasks \
             WHERE origin = 'internal' AND id = ? AND repo = ?",
        )
        .bind(number)
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => Ok(Some(CliTaskRef::from_row(&row)?)),
            None => Ok(None),
        }
    }

    async fn lookup_issue_cli_id(
        &self,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Option<CliTaskRef>> {
        let ext = number.to_string();
        let rows = sqlx::query(
            "SELECT id, repo, origin, title, external_id, status FROM tasks \
             WHERE repo = ? AND origin != 'internal' AND external_id = ? \
             ORDER BY id",
        )
        .bind(repo)
        .bind(&ext)
        .fetch_all(&self.pool)
        .await?;
        let mut matches = dedup_cli_refs(rows)?;
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.pop()),
            _ => Err(ambiguous_cli_id_error(
                &format!("{repo}#{number}"),
                &matches,
            )),
        }
    }

    async fn lookup_bare_cli_id_matches(&self, number: i64) -> anyhow::Result<Vec<CliTaskRef>> {
        let ext = number.to_string();
        let rows = sqlx::query(
            "SELECT id, repo, origin, title, external_id, status FROM tasks \
             WHERE (origin = 'internal' AND id = ?) \
                OR (origin != 'internal' AND external_id = ?) \
             ORDER BY repo, id",
        )
        .bind(number)
        .bind(&ext)
        .fetch_all(&self.pool)
        .await?;
        dedup_cli_refs(rows)
    }
}

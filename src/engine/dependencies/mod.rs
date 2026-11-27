//! Sync GitHub blocked-by links into SQLite and hold tasks until those links are done.
//!
//! Native `blocked_by` links and `Depends on` lines in the issue body are both
//! stored, deduplicated by repository and number. A failed read leaves stored
//! rows in place. Body and CLI blockers are noted with live issue state on
//! each successful sync

use crate::backends::{BlockedByFetch, ExternalBackend, ExternalId, PullRequestCompletion};
use crate::channels::notification::TaskNotification;
use crate::channels::transport::Transport;
use crate::engine;
use crate::store::{
    BlockerSnapshot, DepSource, NewDependency, ObservedPullRequest, TaskStore,
    blocker_failure_kind, blocker_is_satisfied, dep_label,
};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
#[cfg(test)]
use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};

// Twice the default engine.sync_interval so ingest writes the stamp before this cache expires
const DEP_SYNC_TTL_SECS: i64 = 90;
// Each pass costs one to four REST calls per open blocker, so 40 blockers at 45s would pass 5000 an hour
const BLOCKER_REFRESH_TTL_SECS: i64 = 300;
// Every entry is first read in the same tick after a restart, so a fixed TTL expires them together
const BLOCKER_REFRESH_SPREAD_SECS: i64 = 120;

#[cfg(test)]
static TEST_NOW_TS: AtomicI64 = AtomicI64::new(0);

fn now_ts() -> i64 {
    #[cfg(test)]
    {
        let t = TEST_NOW_TS.load(AtomicOrdering::Relaxed);
        if t != 0 {
            return t;
        }
    }
    chrono::Utc::now().timestamp()
}

fn blocker_read_key(repo: &str, number: i64) -> String {
    format!("dep_blocker_read_at:{repo}#{number}")
}

/**
 * How long a blocker's snapshot is reused, spread over `BLOCKER_REFRESH_SPREAD_SECS`. The
 * offset is a hash of the blocker, not a fresh random number, so each blocker keeps its
 * own slot across ticks instead of drifting on every read
 **/
fn blocker_refresh_ttl_secs(repo: &str, number: i64) -> i64 {
    let mut hasher = DefaultHasher::new();
    repo.hash(&mut hasher);
    number.hash(&mut hasher);
    let offset = i64::try_from(hasher.finish() % BLOCKER_REFRESH_SPREAD_SECS as u64).unwrap_or(0);
    BLOCKER_REFRESH_TTL_SECS + offset
}

/**
 * Forget when a blocker was last read, so the next refresh reads it at once. Called when a
 * task reaches `done`. Without it a blocker that finishes right after a read holds its
 * dependents for the rest of the TTL, and a chain pays that at each step. A blocker orch
 * does not manage has no task to finish, so it keeps the full TTL
 **/
pub async fn clear_blocker_read_cache(store: &TaskStore, repo: &str, external_id: &str) {
    // An internal id never names a GitHub issue, so it can never be a blocker
    let Ok(number) = external_id.parse::<i64>() else {
        return;
    };
    if let Err(err) = store.kv_delete(&blocker_read_key(repo, number)).await {
        tracing::warn!(repo, number, err = %err, "clearing the blocker read cache failed");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepRef {
    pub repo: String,
    pub number: i64,
}

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub cache: bool,
    /// The read failed on a transport error, so GitHub cannot be reached for any task
    pub unreachable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepNotice {
    pub repo: String,
    pub issue_number: i64,
    pub comment: String,
    pub notify: bool,
    pub summary: String,
    pub task_id: i64,
    pub title: String,
}

/// Parse `Depends on` and `Blocked by` lines.
///
/// Bare `#12` uses `default_repo`. `owner/repo#12` keeps that repository.
/// Lines inside fenced code blocks are ignored
pub fn parse_body_dependencies(body: &str, default_repo: &str) -> Vec<DepRef> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut in_fence = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let Some(rest) = dependency_line_rest(trimmed) else {
            continue;
        };
        for dep in extract_refs(rest, default_repo) {
            if seen.insert((dep.repo.clone(), dep.number)) {
                out.push(dep);
            }
        }
    }
    out
}

fn dependency_line_rest(trimmed: &str) -> Option<&str> {
    let lower = trimmed.to_ascii_lowercase();
    let prefix = if lower.starts_with("depends on") {
        "depends on".len()
    } else if lower.starts_with("blocked by") {
        "blocked by".len()
    } else {
        return None;
    };
    let rest = trimmed[prefix..]
        .trim_start()
        .trim_start_matches(':')
        .trim();
    if rest.is_empty() { None } else { Some(rest) }
}

/// One `--depends-on` token: a bare issue number, `#12`, or `owner/repo#12`
pub fn parse_depends_on_token(token: &str, default_repo: &str) -> Option<DepRef> {
    let trimmed = token.trim().trim_matches(',');
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(number) = trimmed.parse::<i64>()
        && number > 0
        && !default_repo.is_empty()
    {
        return Some(DepRef {
            repo: default_repo.to_string(),
            number,
        });
    }
    extract_refs(trimmed, default_repo).into_iter().next()
}

fn extract_refs(text: &str, default_repo: &str) -> Vec<DepRef> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'#' {
            index += 1;
            continue;
        }
        let digits: String = text[index + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.is_empty() {
            index += 1;
            continue;
        }
        let number: i64 = match digits.parse() {
            Ok(number) if number > 0 => number,
            _ => {
                index += 1 + digits.len();
                continue;
            }
        };
        let repo = repo_before(text, index).unwrap_or(default_repo).to_string();
        if !repo.is_empty() {
            out.push(DepRef { repo, number });
        }
        index += 1 + digits.len();
    }
    out
}

fn repo_before(text: &str, hash_at: usize) -> Option<&str> {
    let before = &text[..hash_at];
    let mut start = hash_at;
    let bytes = before.as_bytes();
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-' | '/') {
            start -= 1;
        } else {
            break;
        }
    }
    let candidate = &before[start..];
    let mut parts = candidate.split('/');
    let owner = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or("");
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return None;
    }
    if !owner
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
    {
        return None;
    }
    Some(candidate)
}

/// Replace one issue's dependency rows from the native blocked-by links and the issue body.
///
/// Both sources are stored, deduplicated by repository and number. A `cli` row
/// is carried over, and a failed read keeps every stored row. Body and CLI
/// blockers are noted with live issue state on each successful sync
pub async fn sync_issue_dependencies(
    backend: &dyn ExternalBackend,
    store: &TaskStore,
    task_id: i64,
    repo: &str,
    issue_number: Option<i64>,
    body: &str,
) -> anyhow::Result<SyncOutcome> {
    let Some(issue_number) = issue_number.filter(|number| *number > 0) else {
        return Ok(SyncOutcome {
            cache: false,
            unreachable: false,
        });
    };
    let fetch = match backend.blocked_by(repo, issue_number).await {
        Ok(fetch) => fetch,
        Err(err) => BlockedByFetch::Failed(err.to_string()),
    };
    let native = match fetch {
        BlockedByFetch::Native(list) => list,
        BlockedByFetch::Unavailable => Vec::new(),
        BlockedByFetch::Failed(message) => {
            tracing::warn!(
                task_id,
                repo,
                issue_number,
                error = %message,
                "dependency read failed, keeping stored rows"
            );
            return Ok(SyncOutcome {
                cache: false,
                unreachable: false,
            });
        }
        BlockedByFetch::Unreachable(message) => {
            tracing::warn!(
                task_id,
                repo,
                issue_number,
                error = %message,
                "GitHub unreachable, keeping stored rows"
            );
            return Ok(SyncOutcome {
                cache: false,
                unreachable: true,
            });
        }
    };
    // An issue naming itself would wait forever, so it never becomes a row
    let mut seen = HashSet::from([(repo.to_string(), issue_number)]);
    let mut noted = HashSet::new();
    let mut rows = Vec::new();
    for item in &native {
        if !item.state.is_empty() {
            store
                .note_blocker_issue_state(
                    &item.repo,
                    item.number,
                    &item.state,
                    item.state_reason.as_deref(),
                )
                .await?;
            noted.insert((item.repo.clone(), item.number));
        }
        if seen.insert((item.repo.clone(), item.number)) {
            rows.push(NewDependency {
                repo: item.repo.clone(),
                number: item.number,
                source: DepSource::Github,
            });
        }
    }
    for dep in parse_body_dependencies(body, repo) {
        note_live_blocker_issue_state(backend, store, &mut noted, &dep.repo, dep.number).await?;
        if !seen.insert((dep.repo.clone(), dep.number)) {
            continue;
        }
        rows.push(NewDependency {
            repo: dep.repo,
            number: dep.number,
            source: DepSource::Body,
        });
    }
    for dep in store.list_dependencies(task_id).await? {
        if dep.source != DepSource::Cli.as_str() {
            continue;
        }
        note_live_blocker_issue_state(
            backend,
            store,
            &mut noted,
            &dep.depends_on_repo,
            dep.depends_on_number,
        )
        .await?;
        if seen.insert((dep.depends_on_repo.clone(), dep.depends_on_number)) {
            rows.push(NewDependency {
                repo: dep.depends_on_repo,
                number: dep.depends_on_number,
                source: DepSource::Cli,
            });
        }
    }
    store.replace_dependencies(task_id, &rows).await?;
    store.link_known_task(task_id, repo, issue_number).await?;
    // Ingest and the tick share this stamp, so one read per issue covers both within the TTL
    let now = now_ts();
    if let Err(err) = store
        .kv_set(&format!("dep_synced_at:{task_id}"), &now.to_string())
        .await
    {
        tracing::warn!(task_id, err = %err, "failed to cache dependency sync time");
    }
    Ok(SyncOutcome {
        cache: true,
        unreachable: false,
    })
}

/// Read live issue state for a body or CLI blocker the native list did not note
async fn note_live_blocker_issue_state(
    backend: &dyn ExternalBackend,
    store: &TaskStore,
    noted: &mut HashSet<(String, i64)>,
    repo: &str,
    number: i64,
) -> anyhow::Result<()> {
    if !noted.insert((repo.to_string(), number)) {
        return Ok(());
    }
    if let Ok(Some((state, reason))) = backend.issue_close_state(repo, number).await {
        store
            .note_blocker_issue_state(repo, number, &state, reason.as_deref())
            .await?;
    }
    Ok(())
}

/// Move `new` and `routed` tasks with open dependencies to `waiting`, and release
/// `waiting` tasks whose dependencies are done
pub async fn apply_claim_gates(store: &TaskStore) -> anyhow::Result<()> {
    let tasks = store.list_schedulable().await?;
    for task in tasks {
        store.block_claim_if_waiting(task.id).await?;
    }
    Ok(())
}

/// Refresh blocked-by links for schedulable tasks, then hold or release them.
///
/// A read from here or from ingest is cached for 90 seconds, twice the default
/// sync interval, so ingest is the reader for open issues in a synced project.
/// The pass ends at the first transport error
pub async fn reconcile_dependencies(
    backend: &dyn ExternalBackend,
    store: &TaskStore,
    transport: Option<&Transport>,
) -> anyhow::Result<()> {
    let tasks = store.list_schedulable().await?;
    let now = now_ts();
    for task in &tasks {
        let Some(number) = task
            .external_id
            .as_deref()
            .and_then(|id| id.parse::<i64>().ok())
            .filter(|number| *number > 0)
        else {
            continue;
        };
        let key = format!("dep_synced_at:{}", task.id);
        if let Ok(Some(prev)) = store.kv_get(&key).await
            && let Ok(prev) = prev.parse::<i64>()
            && now.saturating_sub(prev) < DEP_SYNC_TTL_SECS
        {
            continue;
        }
        match sync_issue_dependencies(
            backend,
            store,
            task.id,
            &task.repo,
            Some(number),
            &task.body,
        )
        .await
        {
            Ok(outcome) if outcome.unreachable => {
                tracing::warn!("GitHub unreachable, ending the dependency pass");
                break;
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(task_id = task.id, repo = %task.repo, err = %err, "dependency sync failed");
            }
        }
    }
    if let Err(err) = refresh_blocker_snapshots(backend, store).await {
        tracing::warn!(err = %err, "blocker snapshot refresh failed");
    }
    apply_claim_gates(store).await?;
    emit_dependency_notices(backend, store, transport).await?;
    Ok(())
}

/// Read each blocker's issue and pull request, then store the snapshot the claim gate reads.
///
/// A backend that returns no issue state and no pull request number leaves the
/// stored snapshot in place. A known pull request that cannot be read is stored
/// as open and unmerged
pub async fn refresh_blocker_snapshots(
    backend: &dyn ExternalBackend,
    store: &TaskStore,
) -> anyhow::Result<()> {
    let edges = store.list_dependency_edges().await?;
    let mut seen = HashSet::new();
    for edge in edges {
        if !seen.insert((edge.depends_on_repo.clone(), edge.depends_on_number)) {
            continue;
        }
        if let Err(err) = refresh_one_blocker(
            backend,
            store,
            &edge.depends_on_repo,
            edge.depends_on_number,
        )
        .await
        {
            tracing::warn!(
                repo = %edge.depends_on_repo,
                number = edge.depends_on_number,
                err = %err,
                "blocker snapshot refresh failed"
            );
        }
    }
    Ok(())
}

async fn refresh_one_blocker(
    backend: &dyn ExternalBackend,
    store: &TaskStore,
    repo: &str,
    number: i64,
) -> anyhow::Result<()> {
    // A satisfied blocker is terminal: the issue closed as completed, or its pull request
    // merged into the default branch. Re-reading it cost four REST calls per tick for an
    // answer that cannot change, and once the hourly quota dropped under the reserve the
    // throttle refused the issue ingest with it. A reopen still lands here through
    // `note_blocker_issue_state`, which `sync_issue_dependencies` calls with live state
    if store
        .blocker_snapshot(repo, number)
        .await?
        .is_some_and(|snap| snap.is_satisfied())
    {
        return Ok(());
    }

    let key = blocker_read_key(repo, number);
    let now = chrono::Utc::now().timestamp();
    if let Ok(Some(prev)) = store.kv_get(&key).await
        && let Ok(prev) = prev.parse::<i64>()
        && now.saturating_sub(prev) < blocker_refresh_ttl_secs(repo, number)
    {
        return Ok(());
    }

    store.kv_set(&key, &now.to_string()).await?;
    let close = match backend.issue_close_state(repo, number).await {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(repo, number, err = %err, "issue close state read failed");
            None
        }
    };
    let task = store.get_by_external_id(repo, &number.to_string()).await?;
    let pr_number = task
        .as_ref()
        .and_then(|row| row.pr_number)
        .filter(|n| *n > 0);
    if close.is_none() && pr_number.is_none() {
        return Ok(());
    }
    let (issue_state, reason) = match close {
        Some((state, reason)) => (state, reason),
        None => match store.blocker_snapshot(repo, number).await? {
            Some(snap) => (snap.issue_state, snap.state_reason),
            None => ("open".to_string(), None),
        },
    };
    let observed = if let Some(pr) = pr_number {
        let completion = match backend.pull_request_completion(repo, i64::from(pr)).await {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(repo, number, pr, err = %err, "pull request completion read failed");
                None
            }
        };
        Some(observe_completion(completion))
    } else {
        None
    };
    let snap = BlockerSnapshot::observe(&issue_state, reason.as_deref(), observed.as_ref());
    store.save_blocker_snapshot(repo, number, &snap).await
}

fn observe_completion(completion: Option<PullRequestCompletion>) -> ObservedPullRequest {
    match completion {
        Some(done) => ObservedPullRequest {
            merged: done.merged,
            base_is_default: !done.default_branch.is_empty()
                && done.base_ref == done.default_branch,
            approved: done.approved,
            closed_unmerged: done.closed && !done.merged,
        },
        None => ObservedPullRequest {
            merged: false,
            base_is_default: false,
            approved: false,
            closed_unmerged: false,
        },
    }
}

/// True when dispatch must not start this task. A check error also skips dispatch
pub async fn dispatch_is_waiting(store: &TaskStore, repo: &str, external_id: &str) -> bool {
    let id = match store.resolve_id_by_external(repo, external_id).await {
        Ok(Some(id)) => id,
        Ok(None) => return false,
        Err(err) => {
            tracing::warn!(
                task_id = external_id,
                err = %err,
                "dependency check failed, skipping dispatch"
            );
            return true;
        }
    };
    match store.block_claim_if_waiting(id).await {
        Ok(Some(blockers)) => {
            tracing::info!(
                task_id = external_id,
                blockers = %blockers.join(", "),
                "skipping dispatch, waiting on a dependency"
            );
            true
        }
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(
                task_id = external_id,
                err = %err,
                "dependency check failed, skipping dispatch"
            );
            true
        }
    }
}

/// Stop a manual route, run, or retry while dependencies are open.
pub async fn refuse_if_waiting(store: &TaskStore, task_id: i64) -> anyhow::Result<()> {
    if let Some(blockers) = store.block_claim_if_waiting(task_id).await? {
        anyhow::bail!("task {task_id} is waiting on {}", blockers.join(", "));
    }
    Ok(())
}

pub async fn emit_dependency_notices(
    backend: &dyn ExternalBackend,
    store: &TaskStore,
    transport: Option<&Transport>,
) -> anyhow::Result<Vec<DepNotice>> {
    let mut notices = Vec::new();
    notices.extend(cycle_notices(store).await?);
    notices.extend(failure_notices(store).await?);
    for notice in &notices {
        if notice.issue_number > 0 && !notice.comment.is_empty() {
            let id = ExternalId(notice.issue_number.to_string());
            if let Err(err) = backend
                .post_comment_in_repo(&notice.repo, &id, &notice.comment)
                .await
            {
                tracing::warn!(
                    repo = %notice.repo,
                    issue = notice.issue_number,
                    err = %err,
                    "failed to comment on dependency notice"
                );
            }
        }
        if notice.notify
            && let Some(transport) = transport
        {
            transport.push_notification(TaskNotification {
                task_id: if notice.issue_number > 0 {
                    notice.issue_number.to_string()
                } else {
                    notice.task_id.to_string()
                },
                title: notice.title.clone(),
                status: "failed".to_string(),
                agent: "orch".to_string(),
                duration_seconds: 0.0,
                summary: notice.summary.clone(),
                repo: Some(notice.repo.clone()),
                notify_target: None,
                pr_number: None,
            });
        }
    }
    Ok(notices)
}

async fn cycle_notices(store: &TaskStore) -> anyhow::Result<Vec<DepNotice>> {
    let edges = store.list_dependency_edges().await?;
    let mut snap_cache: HashMap<(String, i64), Option<BlockerSnapshot>> = HashMap::new();
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    let mut issue_for_node: HashMap<String, (String, i64, i64, String)> = HashMap::new();

    for edge in &edges {
        let waiter = match edge
            .task_external_id
            .as_deref()
            .and_then(|id| id.parse::<i64>().ok())
        {
            Some(number) if number > 0 => format!("{}#{}", edge.task_repo, number),
            _ => format!("task:{}", edge.task_id),
        };
        if let Some(number) = edge
            .task_external_id
            .as_deref()
            .and_then(|id| id.parse::<i64>().ok())
            .filter(|number| *number > 0)
        {
            issue_for_node.insert(
                waiter.clone(),
                (
                    edge.task_repo.clone(),
                    number,
                    edge.task_id,
                    edge.task_title.clone(),
                ),
            );
        }
        let blocker_node = format!("{}#{}", edge.depends_on_repo, edge.depends_on_number);
        let snap = cached_snapshot(
            store,
            &mut snap_cache,
            &edge.depends_on_repo,
            edge.depends_on_number,
        )
        .await?;
        if blocker_is_satisfied(snap.as_ref()) {
            continue;
        }
        adj.entry(waiter).or_default().push(blocker_node);
    }

    let mut notices = Vec::new();
    for cycle in find_cycles(&adj) {
        let signature = {
            let mut nodes = cycle.clone();
            nodes.sort();
            nodes.join(",")
        };
        let mut named = cycle.clone();
        if let Some(first) = named.first().cloned() {
            named.push(first);
        }
        let path = named.join(" -> ");
        let comment = format!(
            "Dependency cycle: {path}. Orch will not run these tasks until the cycle is removed.{}",
            engine::orch_footer()
        );
        for node in &cycle {
            let Some((repo, number, task_id, title)) = issue_for_node.get(node) else {
                continue;
            };
            let key = format!("dep_cycle_comment:{repo}#{number}:{signature}");
            if store.kv_get(&key).await?.is_some() {
                continue;
            }
            store.kv_set(&key, "1").await?;
            notices.push(DepNotice {
                repo: repo.clone(),
                issue_number: *number,
                comment: comment.clone(),
                notify: false,
                summary: comment.clone(),
                task_id: *task_id,
                title: title.clone(),
            });
        }
    }
    Ok(notices)
}

async fn failure_notices(store: &TaskStore) -> anyhow::Result<Vec<DepNotice>> {
    let edges = store.list_dependency_edges().await?;
    let mut status_cache: HashMap<(String, i64), Option<String>> = HashMap::new();
    let mut snap_cache: HashMap<(String, i64), Option<BlockerSnapshot>> = HashMap::new();
    let mut notices = Vec::new();
    for edge in edges {
        let status = blocker_status(
            store,
            &mut status_cache,
            &edge.depends_on_repo,
            edge.depends_on_number,
        )
        .await?;
        let snap = cached_snapshot(
            store,
            &mut snap_cache,
            &edge.depends_on_repo,
            edge.depends_on_number,
        )
        .await?;
        let Some(kind) = blocker_failure_kind(status.as_deref(), snap.as_ref()) else {
            continue;
        };
        let key = format!(
            "dep_blocker_notice:{}:{}#{}:{kind}",
            edge.task_id, edge.depends_on_repo, edge.depends_on_number
        );
        if store.kv_get(&key).await?.is_some() {
            continue;
        }
        store.kv_set(&key, "1").await?;
        let label = dep_label(
            &edge.task_repo,
            &edge.depends_on_repo,
            edge.depends_on_number,
        );
        let (comment, summary) = if kind == "not_planned" {
            let text = format!(
                "Blocked by {label}, which was closed as not planned. This task stays waiting."
            );
            (format!("{text}{}", engine::orch_footer()), text)
        } else {
            let text = format!(
                "Blocked by {label}, which is blocked and needs a person. This task stays waiting."
            );
            (format!("{text}{}", engine::orch_footer()), text)
        };
        let issue_number = edge
            .task_external_id
            .as_deref()
            .and_then(|id| id.parse::<i64>().ok())
            .filter(|number| *number > 0)
            .unwrap_or(0);
        notices.push(DepNotice {
            repo: edge.task_repo,
            issue_number,
            comment,
            notify: true,
            summary,
            task_id: edge.task_id,
            title: edge.task_title,
        });
    }
    Ok(notices)
}

async fn cached_snapshot(
    store: &TaskStore,
    cache: &mut HashMap<(String, i64), Option<BlockerSnapshot>>,
    repo: &str,
    number: i64,
) -> anyhow::Result<Option<BlockerSnapshot>> {
    if let Some(found) = cache.get(&(repo.to_string(), number)) {
        return Ok(found.clone());
    }
    let snap = store.blocker_snapshot(repo, number).await?;
    cache.insert((repo.to_string(), number), snap.clone());
    Ok(snap)
}

async fn blocker_status(
    store: &TaskStore,
    cache: &mut HashMap<(String, i64), Option<String>>,
    repo: &str,
    number: i64,
) -> anyhow::Result<Option<String>> {
    if let Some(status) = cache.get(&(repo.to_string(), number)) {
        return Ok(status.clone());
    }
    let status = store
        .get_by_external_id(repo, &number.to_string())
        .await?
        .map(|task| task.status.as_str().to_string());
    cache.insert((repo.to_string(), number), status.clone());
    Ok(status)
}

fn find_cycles(adj: &HashMap<String, Vec<String>>) -> Vec<Vec<String>> {
    let mut nodes: Vec<String> = adj.keys().cloned().collect();
    for targets in adj.values() {
        for target in targets {
            if !nodes.iter().any(|node| node == target) {
                nodes.push(target.clone());
            }
        }
    }
    nodes.sort();
    let mut color: HashMap<String, u8> = HashMap::new();
    let mut stack = Vec::new();
    let mut cycles = Vec::new();
    let mut seen = HashSet::new();
    for node in nodes {
        if color.get(&node).copied().unwrap_or(0) == 0 {
            visit(&node, adj, &mut color, &mut stack, &mut cycles, &mut seen);
        }
    }
    cycles
}

fn visit(
    node: &str,
    adj: &HashMap<String, Vec<String>>,
    color: &mut HashMap<String, u8>,
    stack: &mut Vec<String>,
    cycles: &mut Vec<Vec<String>>,
    seen: &mut HashSet<String>,
) {
    color.insert(node.to_string(), 1);
    stack.push(node.to_string());
    if let Some(nexts) = adj.get(node) {
        for next in nexts {
            match color.get(next).copied().unwrap_or(0) {
                1 => {
                    if let Some(start) = stack.iter().position(|item| item == next) {
                        let mut cycle = stack[start..].to_vec();
                        if let Some(min_at) = cycle
                            .iter()
                            .enumerate()
                            .min_by(|left, right| left.1.cmp(right.1))
                            .map(|(index, _)| index)
                        {
                            cycle.rotate_left(min_at);
                        }
                        let signature = {
                            let mut sorted = cycle.clone();
                            sorted.sort();
                            sorted.join(",")
                        };
                        if seen.insert(signature) {
                            cycles.push(cycle);
                        }
                    }
                }
                0 => visit(next, adj, color, stack, cycles, seen),
                _ => {}
            }
        }
    }
    stack.pop();
    color.insert(node.to_string(), 2);
}

#[cfg(test)]
mod tests;

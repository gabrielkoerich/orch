//! Sync GitHub blocked-by links into SQLite and hold tasks until those links are done.
//!
//! Native `blocked_by` links and `Depends on` lines in the issue body are both
//! stored, deduplicated by repository and number. A failed read leaves stored
//! rows in place.

use crate::backends::{BlockedByFetch, ExternalBackend, ExternalId, PullRequestCompletion};
use crate::channels::notification::TaskNotification;
use crate::channels::transport::Transport;
use crate::store::{
    blocker_failure_kind, blocker_is_satisfied, dep_label, BlockerSnapshot, DepSource,
    NewDependency, ObservedPullRequest, TaskStore,
};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

const DEP_SYNC_TTL_SECS: i64 = 45;
// Each pass costs one to four REST calls per open blocker, so 40 blockers at 45s would pass 5000 an hour
const BLOCKER_REFRESH_TTL_SECS: i64 = 300;
// Every entry is first read in the same tick after a restart, so a fixed TTL expires them together
const BLOCKER_REFRESH_SPREAD_SECS: i64 = 120;

fn blocker_read_key(repo: &str, number: i64) -> String {
    format!("dep_blocker_read_at:{repo}#{number}")
}

/**
 * How long a blocker's snapshot is reused, spread over `BLOCKER_REFRESH_SPREAD_SECS`. The
 * offset is a hash of the blocker, not a fresh random number, so each blocker keeps its
 * own slot across ticks instead of drifting on every read
 **/
fn blocker_refresh_ttl_secs(repo: &str, number: i64) -> i64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    repo.hash(&mut hasher);
    number.hash(&mut hasher);
    let offset = (hasher.finish() % BLOCKER_REFRESH_SPREAD_SECS as u64) as i64;
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
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

/// One `--depends-on` token: a bare issue number, `#12`, or `owner/repo#12`
pub fn parse_depends_on_token(token: &str, default_repo: &str) -> Option<DepRef> {
    let trimmed = token.trim().trim_matches(',');
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(number) = trimmed.parse::<i64>() {
        if number > 0 && !default_repo.is_empty() {
            return Some(DepRef {
                repo: default_repo.to_string(),
                number,
            });
        }
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
            .take_while(|ch| ch.is_ascii_digit())
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
/// is carried over, and a failed read keeps every stored row
pub async fn sync_issue_dependencies(
    backend: &dyn ExternalBackend,
    store: &TaskStore,
    task_id: i64,
    repo: &str,
    issue_number: Option<i64>,
    body: &str,
) -> anyhow::Result<SyncOutcome> {
    let Some(issue_number) = issue_number.filter(|number| *number > 0) else {
        return Ok(SyncOutcome { cache: false });
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
            return Ok(SyncOutcome { cache: false });
        }
    };
    // An issue naming itself would wait forever, so it never becomes a row
    let mut seen = HashSet::from([(repo.to_string(), issue_number)]);
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
        if !seen.insert((dep.repo.clone(), dep.number)) {
            continue;
        }
        // Seed a snapshot only once, `refresh_blocker_snapshots` keeps it current from then on
        if store
            .blocker_snapshot(&dep.repo, dep.number)
            .await?
            .is_none()
            && store
                .get_by_external_id(&dep.repo, &dep.number.to_string())
                .await?
                .is_none()
        {
            if let Ok(Some((state, reason))) =
                backend.issue_close_state(&dep.repo, dep.number).await
            {
                store
                    .note_blocker_issue_state(&dep.repo, dep.number, &state, reason.as_deref())
                    .await?;
            }
        }
        rows.push(NewDependency {
            repo: dep.repo,
            number: dep.number,
            source: DepSource::Body,
        });
    }
    for dep in store.list_dependencies(task_id).await? {
        if dep.source == DepSource::Cli.as_str()
            && seen.insert((dep.depends_on_repo.clone(), dep.depends_on_number))
        {
            rows.push(NewDependency {
                repo: dep.depends_on_repo,
                number: dep.depends_on_number,
                source: DepSource::Cli,
            });
        }
    }
    store.replace_dependencies(task_id, &rows).await?;
    store.link_known_task(task_id, repo, issue_number).await?;
    Ok(SyncOutcome { cache: true })
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
/// A successful read is cached for 45 seconds so a 10 second tick does not
/// fetch every task every time. Sync from ingest calls `sync_issue_dependencies`
/// directly and does not use this cache
pub async fn reconcile_dependencies(
    backend: &dyn ExternalBackend,
    store: &TaskStore,
    transport: Option<&Transport>,
) -> anyhow::Result<()> {
    let tasks = store.list_schedulable().await?;
    let now = chrono::Utc::now().timestamp();
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
        if let Ok(Some(prev)) = store.kv_get(&key).await {
            if let Ok(prev) = prev.parse::<i64>() {
                if now.saturating_sub(prev) < DEP_SYNC_TTL_SECS {
                    continue;
                }
            }
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
            Ok(outcome) if outcome.cache => {
                if let Err(err) = store.kv_set(&key, &now.to_string()).await {
                    tracing::warn!(task_id = task.id, err = %err, "failed to cache dependency sync time");
                }
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
    if let Ok(Some(prev)) = store.kv_get(&key).await {
        if let Ok(prev) = prev.parse::<i64>() {
            if now.saturating_sub(prev) < blocker_refresh_ttl_secs(repo, number) {
                return Ok(());
            }
        }
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
        if notice.notify {
            if let Some(transport) = transport {
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
            crate::engine::orch_footer()
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
        let (comment, summary) = match kind {
            "not_planned" => {
                let text = format!(
                    "Blocked by {label}, which was closed as not planned. This task stays waiting."
                );
                (format!("{text}{}", crate::engine::orch_footer()), text)
            }
            _ => {
                let text = format!(
                    "Blocked by {label}, which is blocked and needs a person. This task stays waiting."
                );
                (format!("{text}{}", crate::engine::orch_footer()), text)
            }
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
mod tests {
    use super::*;
    use crate::backends::{BlockedByIssue, ExternalTask, Status};
    use crate::store::{NewTask, TaskStatus};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    #[test]
    fn body_parser_reads_the_three_forms_and_skips_fences() {
        let body = "\
Depends on #12
Blocked by #12, #13
Depends on owner/repo#12
```
Depends on #99
```
See #4 in the notes
";
        let deps = parse_body_dependencies(body, "acme/app");
        assert_eq!(
            deps,
            vec![
                DepRef {
                    repo: "acme/app".into(),
                    number: 12
                },
                DepRef {
                    repo: "acme/app".into(),
                    number: 13
                },
                DepRef {
                    repo: "owner/repo".into(),
                    number: 12
                },
            ]
        );
    }

    #[derive(Clone)]
    struct ScriptedReviewComment {
        login: String,
        body: String,
        created_at: String,
    }

    struct ScriptedBackend {
        blocked: Mutex<HashMap<i64, BlockedByFetch>>,
        comments: Mutex<Vec<(String, String)>>,
        close_state: Mutex<HashMap<i64, (String, Option<String>)>>,
        pulls: Mutex<HashMap<i64, Option<crate::backends::PullRequestCompletion>>>,
        review_comments: Mutex<HashMap<i64, Vec<ScriptedReviewComment>>>,
        reviewer: Mutex<Option<String>>,
        /// Counts the blocker reads that cost a GitHub call.
        reads: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedBackend {
        fn new(blocked: HashMap<i64, BlockedByFetch>) -> Self {
            Self {
                blocked: Mutex::new(blocked),
                comments: Mutex::new(Vec::new()),
                close_state: Mutex::new(HashMap::new()),
                pulls: Mutex::new(HashMap::new()),
                review_comments: Mutex::new(HashMap::new()),
                reads: std::sync::atomic::AtomicUsize::new(0),
                reviewer: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl ExternalBackend for ScriptedBackend {
        fn name(&self) -> &str {
            "scripted"
        }

        async fn create_task(
            &self,
            _title: &str,
            _body: &str,
            _labels: &[String],
        ) -> anyhow::Result<ExternalId> {
            Ok(ExternalId("1".into()))
        }

        async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
            Ok(ExternalTask {
                id: id.clone(),
                title: "t".into(),
                body: String::new(),
                state: "open".into(),
                labels: vec![],
                author: "bot".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                url: String::new(),
            })
        }

        async fn list_by_status(&self, _status: Status) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }

        async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()> {
            self.comments
                .lock()
                .unwrap()
                .push((id.0.clone(), body.to_string()));
            Ok(())
        }

        async fn set_labels(&self, _id: &ExternalId, _labels: &[String]) -> anyhow::Result<()> {
            Ok(())
        }

        async fn remove_label(&self, _id: &ExternalId, _label: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
            Ok(vec![])
        }

        async fn health_check(&self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn blocked_by(
            &self,
            _repo: &str,
            issue_number: i64,
        ) -> anyhow::Result<BlockedByFetch> {
            Ok(self
                .blocked
                .lock()
                .unwrap()
                .get(&issue_number)
                .cloned()
                .unwrap_or(BlockedByFetch::Unavailable))
        }

        async fn issue_close_state(
            &self,
            _repo: &str,
            issue_number: i64,
        ) -> anyhow::Result<Option<(String, Option<String>)>> {
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self.close_state.lock().unwrap().get(&issue_number).cloned())
        }

        async fn pull_request_completion(
            &self,
            _repo: &str,
            pr_number: i64,
        ) -> anyhow::Result<Option<crate::backends::PullRequestCompletion>> {
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut completion = self
                .pulls
                .lock()
                .unwrap()
                .get(&pr_number)
                .cloned()
                .unwrap_or(None);
            if let Some(ref mut done) = completion {
                if !done.approved {
                    let comments = self
                        .review_comments
                        .lock()
                        .unwrap()
                        .get(&pr_number)
                        .cloned()
                        .unwrap_or_default();
                    let reviewer = self.reviewer.lock().unwrap().clone().unwrap_or_default();
                    let mapped: Vec<crate::backends::github::AutomatedReviewComment<'_>> = comments
                        .iter()
                        .map(|comment| crate::backends::github::AutomatedReviewComment {
                            login: &comment.login,
                            body: &comment.body,
                            created_at: &comment.created_at,
                        })
                        .collect();
                    done.approved = crate::backends::github::latest_automated_review_approves(
                        &mapped, &reviewer,
                    );
                }
            }
            Ok(completion)
        }
    }

    async fn insert(store: &TaskStore, repo: &str, number: &str, title: &str, body: &str) -> i64 {
        store
            .create(&NewTask {
                external_id: Some(number.to_string()),
                repo: repo.to_string(),
                origin: "github".to_string(),
                title: title.to_string(),
                body: body.to_string(),
                source: "manual".to_string(),
                source_id: String::new(),
                author: String::new(),
                url: String::new(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap()
    }

    fn native(repo: &str, number: i64, state: &str, reason: Option<&str>) -> BlockedByIssue {
        BlockedByIssue {
            github_id: number,
            repo: repo.to_string(),
            number,
            state: state.to_string(),
            state_reason: reason.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn native_list_replaces_rows_and_empty_native_clears_them() {
        let store = TaskStore::open_memory().await.unwrap();
        let b = insert(&store, "owner/repo", "2", "B", "Depends on #1").await;
        let mut blocked = HashMap::new();
        blocked.insert(
            2,
            BlockedByFetch::Native(vec![native("owner/repo", 1, "open", None)]),
        );
        let backend = ScriptedBackend::new(blocked);
        sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #1")
            .await
            .unwrap();
        let deps = store.list_dependencies(b).await.unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].source, "github");

        backend
            .blocked
            .lock()
            .unwrap()
            .insert(2, BlockedByFetch::Native(vec![]));
        sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
            .await
            .unwrap();
        assert!(store.list_dependencies(b).await.unwrap().is_empty());
        apply_claim_gates(&store).await.unwrap();
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::New);
    }

    #[tokio::test]
    async fn unavailable_keeps_rows_when_the_body_has_no_deps_and_failed_keeps_rows() {
        let store = TaskStore::open_memory().await.unwrap();
        let b = insert(&store, "owner/repo", "2", "B", "").await;
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".into(),
                    number: 1,
                    source: DepSource::Cli,
                }],
            )
            .await
            .unwrap();
        let backend = ScriptedBackend::new(HashMap::new());
        sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
            .await
            .unwrap();
        assert_eq!(store.list_dependencies(b).await.unwrap()[0].source, "cli");

        backend
            .blocked
            .lock()
            .unwrap()
            .insert(2, BlockedByFetch::Failed("boom".into()));
        sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #9")
            .await
            .unwrap();
        assert_eq!(
            store.list_dependencies(b).await.unwrap()[0].depends_on_number,
            1
        );

        backend
            .blocked
            .lock()
            .unwrap()
            .insert(2, BlockedByFetch::Native(vec![]));
        sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
            .await
            .unwrap();
        assert_eq!(store.list_dependencies(b).await.unwrap()[0].source, "cli");
    }

    #[tokio::test]
    async fn body_lines_become_rows_in_all_three_forms() {
        let store = TaskStore::open_memory().await.unwrap();
        let b = insert(
            &store,
            "acme/app",
            "8",
            "B",
            "Blocked by #12, #13\nDepends on owner/repo#12",
        )
        .await;
        let backend = ScriptedBackend::new(HashMap::new());
        sync_issue_dependencies(
            &backend,
            &store,
            b,
            "acme/app",
            Some(8),
            "Blocked by #12, #13\nDepends on owner/repo#12",
        )
        .await
        .unwrap();
        let deps = store.list_dependencies(b).await.unwrap();
        let labels: Vec<_> = deps
            .iter()
            .map(|dep| format!("{}#{}", dep.depends_on_repo, dep.depends_on_number))
            .collect();
        assert_eq!(
            labels,
            vec![
                "acme/app#12".to_string(),
                "acme/app#13".to_string(),
                "owner/repo#12".to_string()
            ]
        );
        assert!(deps.iter().all(|dep| dep.source == "body"));
    }

    #[tokio::test]
    async fn body_dependency_is_stored_when_the_native_list_is_empty() {
        let store = TaskStore::open_memory().await.unwrap();
        let b = insert(&store, "owner/repo", "2", "B", "Depends on #1").await;
        let mut blocked = HashMap::new();
        blocked.insert(2, BlockedByFetch::Native(vec![]));
        let backend = ScriptedBackend::new(blocked);
        sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #1")
            .await
            .unwrap();
        let deps = store.list_dependencies(b).await.unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].depends_on_number, 1);
        assert_eq!(deps[0].source, "body");
        apply_claim_gates(&store).await.unwrap();
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
    }

    #[tokio::test]
    async fn native_and_body_dependencies_are_both_stored() {
        let store = TaskStore::open_memory().await.unwrap();
        let body = "Depends on #3\nBlocked by #1";
        let b = insert(&store, "owner/repo", "2", "B", body).await;
        let mut blocked = HashMap::new();
        blocked.insert(
            2,
            BlockedByFetch::Native(vec![native("owner/repo", 1, "open", None)]),
        );
        let backend = ScriptedBackend::new(blocked);
        sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), body)
            .await
            .unwrap();
        let deps = store.list_dependencies(b).await.unwrap();
        let rows: Vec<_> = deps
            .iter()
            .map(|dep| (dep.depends_on_number, dep.source.as_str()))
            .collect();
        assert_eq!(rows, vec![(1, "github"), (3, "body")]);
    }

    #[tokio::test]
    async fn removing_the_body_line_drops_only_the_body_row() {
        let store = TaskStore::open_memory().await.unwrap();
        let b = insert(&store, "owner/repo", "2", "B", "Depends on #3").await;
        let mut blocked = HashMap::new();
        blocked.insert(
            2,
            BlockedByFetch::Native(vec![native("owner/repo", 1, "open", None)]),
        );
        let backend = ScriptedBackend::new(blocked);
        sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #3")
            .await
            .unwrap();
        assert_eq!(store.list_dependencies(b).await.unwrap().len(), 2);

        sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
            .await
            .unwrap();
        let deps = store.list_dependencies(b).await.unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].depends_on_number, 1);
        assert_eq!(deps[0].source, "github");
    }

    #[tokio::test]
    async fn an_issue_naming_itself_in_the_body_stores_no_row() {
        let store = TaskStore::open_memory().await.unwrap();
        let b = insert(&store, "owner/repo", "2", "B", "Depends on #2").await;
        let backend = ScriptedBackend::new(HashMap::new());
        sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #2")
            .await
            .unwrap();
        assert!(store.list_dependencies(b).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cycle_comments_once_and_neither_task_is_claimable() {
        let store = TaskStore::open_memory().await.unwrap();
        let a = insert(&store, "owner/repo", "1", "A", "").await;
        let b = insert(&store, "owner/repo", "2", "B", "").await;
        let mut blocked = HashMap::new();
        blocked.insert(
            1,
            BlockedByFetch::Native(vec![native("owner/repo", 2, "open", None)]),
        );
        blocked.insert(
            2,
            BlockedByFetch::Native(vec![native("owner/repo", 1, "open", None)]),
        );
        let backend = ScriptedBackend::new(blocked);
        sync_issue_dependencies(&backend, &store, a, "owner/repo", Some(1), "")
            .await
            .unwrap();
        sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
            .await
            .unwrap();
        apply_claim_gates(&store).await.unwrap();
        assert_eq!(store.get(a).await.unwrap().status, TaskStatus::Waiting);
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
        let first = emit_dependency_notices(&backend, &store, None)
            .await
            .unwrap();
        assert_eq!(first.len(), 2);
        assert!(first
            .iter()
            .all(|notice| notice.comment.contains("owner/repo#1")));
        assert!(first
            .iter()
            .all(|notice| notice.comment.contains("owner/repo#2")));
        let second = emit_dependency_notices(&backend, &store, None)
            .await
            .unwrap();
        assert!(second.is_empty());
        assert_eq!(backend.comments.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn not_planned_keeps_waiting_and_notifies_once() {
        let store = TaskStore::open_memory().await.unwrap();
        let b = insert(&store, "owner/repo", "2", "B", "").await;
        let mut blocked = HashMap::new();
        blocked.insert(
            2,
            BlockedByFetch::Native(vec![native("owner/repo", 1, "closed", Some("not_planned"))]),
        );
        let backend = ScriptedBackend::new(blocked);
        let transport = Transport::new();
        let mut rx = transport.subscribe_notifications();
        sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
            .await
            .unwrap();
        apply_claim_gates(&store).await.unwrap();
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
        let notices = emit_dependency_notices(&backend, &store, Some(&transport))
            .await
            .unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].notify);
        assert!(notices[0].summary.contains("not planned"));
        let again = emit_dependency_notices(&backend, &store, Some(&transport))
            .await
            .unwrap();
        assert!(again.is_empty());
        let note = rx.try_recv().unwrap();
        assert_eq!(note.status, "failed");
        assert!(rx.try_recv().is_err());
        assert_eq!(backend.comments.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn waiting_task_is_not_routed_until_the_blocker_is_done() {
        use crate::engine::router::{Router, RouterConfig};
        use crate::engine::tasks::TaskManager;
        use crate::engine::tick::{tick_route_tasks, RoutingTickQuota};

        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let a = insert(&store, "owner/repo", "1", "A", "").await;
        let b = insert(&store, "owner/repo", "2", "B", "").await;
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".into(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();

        let backend: Arc<dyn ExternalBackend> =
            Arc::new(crate::backends::test_helpers::NoopBackend);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".into(),
        ));
        let mut tier = HashMap::new();
        tier.insert("claude".to_string(), vec!["haiku".to_string()]);
        let mut model_map = HashMap::new();
        model_map.insert("simple".into(), tier.clone());
        model_map.insert("medium".into(), tier.clone());
        model_map.insert("complex".into(), tier);
        let round_robin = RouterConfig {
            mode: "round_robin".into(),
            agents: vec!["claude".into()],
            model_map: model_map.clone(),
            ..Default::default()
        };
        let mut router = Router::new_for_test(round_robin, vec!["claude".into()]);
        let mut quota = RoutingTickQuota::new(10);
        tick_route_tasks(
            &backend,
            &task_manager,
            &mut router,
            &store,
            "owner/repo",
            &mut quota,
        )
        .await
        .unwrap();
        let waiting = store.get(b).await.unwrap();
        assert_eq!(waiting.status, TaskStatus::Waiting);
        assert!(waiting.agent.is_none());

        let llm = RouterConfig {
            mode: "llm".into(),
            max_route_attempts: 0,
            agents: vec!["claude".into()],
            model_map,
            ..Default::default()
        };
        let mut llm_router = Router::new_for_test(llm, vec!["claude".into()]);
        let ext = crate::engine::tasks::store_task_to_external(&store.get(b).await.unwrap());
        store.update_status(b, TaskStatus::New).await.unwrap();
        let err = llm_router
            .route(&ext, &store, "owner/repo")
            .await
            .unwrap_err();
        assert!(err
            .downcast_ref::<crate::store::WaitingOnDependency>()
            .is_some());
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
        assert_eq!(store.get(b).await.unwrap().route_attempts, 0);

        store.update_status(a, TaskStatus::Done).await.unwrap();
        let mut quota = RoutingTickQuota::new(10);
        tick_route_tasks(
            &backend,
            &task_manager,
            &mut router,
            &store,
            "owner/repo",
            &mut quota,
        )
        .await
        .unwrap();
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);

        store
            .save_blocker_snapshot(
                "owner/repo",
                1,
                &BlockerSnapshot::observe(
                    "closed",
                    Some("completed"),
                    Some(&ObservedPullRequest {
                        merged: true,
                        base_is_default: true,
                        approved: true,
                        closed_unmerged: false,
                    }),
                ),
            )
            .await
            .unwrap();
        let mut quota = RoutingTickQuota::new(10);
        tick_route_tasks(
            &backend,
            &task_manager,
            &mut router,
            &store,
            "owner/repo",
            &mut quota,
        )
        .await
        .unwrap();
        let routed = store.get(b).await.unwrap();
        assert_eq!(routed.status, TaskStatus::Routed);
        assert!(routed.agent.is_some());
    }

    #[tokio::test]
    async fn round_robin_route_returns_waiting_on_dependency() {
        use crate::engine::router::{Router, RouterConfig};
        use crate::engine::tasks::store_task_to_external;

        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let b = insert(&store, "owner/repo", "2", "B", "").await;
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".into(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();
        let ext = store_task_to_external(&store.get(b).await.unwrap());
        let mut tier = HashMap::new();
        tier.insert("claude".to_string(), vec!["haiku".to_string()]);
        let mut model_map = HashMap::new();
        model_map.insert("medium".into(), tier);
        let config = RouterConfig {
            mode: "round_robin".into(),
            agents: vec!["claude".into()],
            model_map,
            ..Default::default()
        };
        let mut router = Router::new_for_test(config, vec!["claude".into()]);
        let err = router.route(&ext, &store, "owner/repo").await.unwrap_err();
        assert!(err
            .downcast_ref::<crate::store::WaitingOnDependency>()
            .is_some());
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
        assert_eq!(store.get(b).await.unwrap().route_attempts, 0);
    }

    async fn refresh_uncached(backend: &ScriptedBackend, store: &TaskStore) {
        store
            .kv_delete("dep_blocker_read_at:owner/repo#1")
            .await
            .unwrap();
        refresh_blocker_snapshots(backend, store).await.unwrap();
    }

    #[tokio::test]
    async fn an_open_blocker_is_read_once_per_ttl() {
        let store = TaskStore::open_memory().await.unwrap();
        insert(&store, "owner/repo", "1", "A", "").await;
        let b = insert(&store, "owner/repo", "2", "B", "").await;
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".into(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();
        let backend = ScriptedBackend::new(HashMap::new());
        backend
            .close_state
            .lock()
            .unwrap()
            .insert(1, ("open".into(), None));
        let reads = || backend.reads.load(std::sync::atomic::Ordering::Relaxed);

        refresh_blocker_snapshots(&backend, &store).await.unwrap();
        let first = reads();
        assert!(first > 0, "the first refresh has to read the blocker");

        refresh_blocker_snapshots(&backend, &store).await.unwrap();
        refresh_blocker_snapshots(&backend, &store).await.unwrap();
        assert_eq!(
            reads(),
            first,
            "a refresh inside the TTL must not call GitHub"
        );

        let stale = chrono::Utc::now().timestamp()
            - (BLOCKER_REFRESH_TTL_SECS + BLOCKER_REFRESH_SPREAD_SECS);
        store
            .kv_set("dep_blocker_read_at:owner/repo#1", &stale.to_string())
            .await
            .unwrap();
        refresh_blocker_snapshots(&backend, &store).await.unwrap();
        assert!(
            reads() > first,
            "an expired entry has to read the blocker again"
        );
    }

    /// Set up `#1` as an open blocker of `#2` and read it once, so the TTL is live
    async fn read_open_blocker_once(store: &TaskStore) -> Arc<ScriptedBackend> {
        let b = insert(store, "owner/repo", "2", "B", "").await;
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".into(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();
        let backend = Arc::new(ScriptedBackend::new(HashMap::new()));
        backend
            .close_state
            .lock()
            .unwrap()
            .insert(1, ("open".into(), None));
        refresh_blocker_snapshots(&*backend, store).await.unwrap();
        backend
    }

    #[tokio::test]
    async fn a_blockers_task_going_done_makes_the_next_refresh_read_it() {
        use crate::engine::tasks::TaskManager;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        insert(&store, "owner/repo", "1", "A", "").await;
        let backend = read_open_blocker_once(&store).await;
        let reads = || backend.reads.load(std::sync::atomic::Ordering::Relaxed);
        let first = reads();
        assert!(first > 0, "the first refresh has to read the blocker");

        let task_manager = TaskManager::with_store(
            backend.clone() as Arc<dyn ExternalBackend>,
            store.clone(),
            "owner/repo".into(),
        );
        task_manager
            .update_task_status(&ExternalId("1".into()), Status::Done)
            .await
            .unwrap();

        refresh_blocker_snapshots(&*backend, &store).await.unwrap();
        assert!(
            reads() > first,
            "a blocker whose task is done has to be read inside the TTL"
        );
    }

    #[tokio::test]
    async fn a_blocker_orch_does_not_manage_keeps_its_ttl() {
        use crate::engine::tasks::TaskManager;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend = read_open_blocker_once(&store).await;
        let reads = || backend.reads.load(std::sync::atomic::Ordering::Relaxed);
        let first = reads();
        assert!(first > 0, "the first refresh has to read the blocker");

        // `#1` has no task row here, so finishing the dependent must not clear its entry
        let task_manager = TaskManager::with_store(
            backend.clone() as Arc<dyn ExternalBackend>,
            store.clone(),
            "owner/repo".into(),
        );
        task_manager
            .update_task_status(&ExternalId("2".into()), Status::Done)
            .await
            .unwrap();

        refresh_blocker_snapshots(&*backend, &store).await.unwrap();
        assert_eq!(
            reads(),
            first,
            "a blocker orch does not manage is read once per TTL"
        );
    }

    #[test]
    fn blocker_ttls_spread_within_a_bounded_range() {
        let ttls: Vec<i64> = (1..200)
            .map(|n| blocker_refresh_ttl_secs("owner/repo", n))
            .collect();
        for ttl in &ttls {
            assert!(
                (BLOCKER_REFRESH_TTL_SECS..BLOCKER_REFRESH_TTL_SECS + BLOCKER_REFRESH_SPREAD_SECS)
                    .contains(ttl),
                "{ttl} is outside the TTL range"
            );
        }
        let spread: HashSet<i64> = ttls.iter().copied().collect();
        assert!(spread.len() > 50, "the TTLs have to spread the reads out");
        assert_eq!(
            blocker_refresh_ttl_secs("owner/repo", 1),
            blocker_refresh_ttl_secs("owner/repo", 1),
            "one blocker keeps the same TTL across ticks"
        );
    }

    #[tokio::test]
    async fn a_blocker_already_known_done_costs_no_github_call() {
        let store = TaskStore::open_memory().await.unwrap();
        insert(&store, "owner/repo", "1", "A", "").await;
        let b = insert(&store, "owner/repo", "2", "B", "").await;
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".into(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();
        let backend = ScriptedBackend::new(HashMap::new());
        backend
            .close_state
            .lock()
            .unwrap()
            .insert(1, ("closed".into(), Some("completed".into())));

        refresh_blocker_snapshots(&backend, &store).await.unwrap();
        let first = backend.reads.load(std::sync::atomic::Ordering::Relaxed);
        assert!(first > 0, "the first refresh has to read the blocker");
        assert!(store.block_claim_if_waiting(b).await.unwrap().is_none());

        refresh_blocker_snapshots(&backend, &store).await.unwrap();
        refresh_blocker_snapshots(&backend, &store).await.unwrap();
        assert_eq!(
            backend.reads.load(std::sync::atomic::Ordering::Relaxed),
            first,
            "a blocker closed as completed must not be read again"
        );
    }

    #[tokio::test]
    async fn refresh_holds_until_the_pr_is_merged_into_the_default_branch() {
        use crate::backends::PullRequestCompletion;

        let store = TaskStore::open_memory().await.unwrap();
        let a = insert(&store, "owner/repo", "1", "A", "").await;
        let b = insert(&store, "owner/repo", "2", "B", "").await;
        store
            .set_fields(a, &[("pr_number", serde_json::json!(10))])
            .await
            .unwrap();
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".into(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();
        let backend = ScriptedBackend::new(HashMap::new());
        backend
            .close_state
            .lock()
            .unwrap()
            .insert(1, ("closed".into(), Some("completed".into())));

        let open_pr = PullRequestCompletion {
            merged: false,
            base_ref: "main".into(),
            default_branch: "main".into(),
            approved: false,
            closed: false,
        };
        backend.pulls.lock().unwrap().insert(10, Some(open_pr));
        refresh_blocker_snapshots(&backend, &store).await.unwrap();
        assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());

        backend.pulls.lock().unwrap().insert(
            10,
            Some(PullRequestCompletion {
                merged: false,
                base_ref: "main".into(),
                default_branch: "main".into(),
                approved: true,
                closed: false,
            }),
        );
        refresh_uncached(&backend, &store).await;
        assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());

        backend.pulls.lock().unwrap().insert(
            10,
            Some(PullRequestCompletion {
                merged: true,
                base_ref: "feature".into(),
                default_branch: "main".into(),
                approved: true,
                closed: true,
            }),
        );
        refresh_uncached(&backend, &store).await;
        assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());

        backend.pulls.lock().unwrap().insert(
            10,
            Some(PullRequestCompletion {
                merged: false,
                base_ref: "main".into(),
                default_branch: "main".into(),
                approved: true,
                closed: true,
            }),
        );
        refresh_uncached(&backend, &store).await;
        assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());

        backend.pulls.lock().unwrap().insert(
            10,
            Some(PullRequestCompletion {
                merged: true,
                base_ref: "main".into(),
                default_branch: "main".into(),
                approved: false,
                closed: true,
            }),
        );
        refresh_uncached(&backend, &store).await;
        assert_eq!(store.block_claim_if_waiting(b).await.unwrap(), None);
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::New);
    }

    #[tokio::test]
    async fn merged_unapproved_pr_releases_the_waiter() {
        use crate::backends::PullRequestCompletion;

        let store = TaskStore::open_memory().await.unwrap();
        let a = insert(&store, "owner/repo", "1", "A", "").await;
        let b = insert(&store, "owner/repo", "2", "B", "").await;
        store
            .set_fields(a, &[("pr_number", serde_json::json!(10))])
            .await
            .unwrap();
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".into(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();
        let backend = ScriptedBackend::new(HashMap::new());
        backend
            .close_state
            .lock()
            .unwrap()
            .insert(1, ("closed".into(), Some("completed".into())));
        backend.pulls.lock().unwrap().insert(
            10,
            Some(PullRequestCompletion {
                merged: true,
                base_ref: "main".into(),
                default_branch: "main".into(),
                approved: false,
                closed: true,
            }),
        );
        refresh_blocker_snapshots(&backend, &store).await.unwrap();
        assert_eq!(store.block_claim_if_waiting(b).await.unwrap(), None);
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::New);
    }

    #[tokio::test]
    async fn blocker_closed_before_import_releases_on_refresh() {
        let store = TaskStore::open_memory().await.unwrap();
        let waiter = insert(&store, "owner/repo", "24", "later", "").await;
        let mut blocked = HashMap::new();
        blocked.insert(
            24,
            BlockedByFetch::Native(vec![native("owner/repo", 14, "open", None)]),
        );
        let backend = ScriptedBackend::new(blocked);
        backend
            .close_state
            .lock()
            .unwrap()
            .insert(14, ("closed".into(), Some("completed".into())));
        reconcile_dependencies(&backend, &store, None)
            .await
            .unwrap();
        assert_eq!(store.get(waiter).await.unwrap().status, TaskStatus::New);
        let snap = store
            .blocker_snapshot("owner/repo", 14)
            .await
            .unwrap()
            .unwrap();
        assert!(snap.is_satisfied());
        assert_eq!(snap.issue_state, "closed");
    }

    #[test]
    fn depends_on_token_accepts_a_bare_number_and_a_cross_repo_ref() {
        assert_eq!(
            parse_depends_on_token("12", "acme/app"),
            Some(DepRef {
                repo: "acme/app".into(),
                number: 12
            })
        );
        assert_eq!(
            parse_depends_on_token("owner/repo#12", "acme/app"),
            Some(DepRef {
                repo: "owner/repo".into(),
                number: 12
            })
        );
    }

    #[tokio::test]
    async fn two_route_calls_cannot_claim_a_waiting_task() {
        use crate::engine::router::{Router, RouterConfig};
        use crate::engine::tasks::store_task_to_external;

        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let b = insert(&store, "owner/repo", "2", "B", "").await;
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".into(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();
        let ext = store_task_to_external(&store.get(b).await.unwrap());
        let mut tier = HashMap::new();
        tier.insert("claude".to_string(), vec!["haiku".to_string()]);
        let mut model_map = HashMap::new();
        model_map.insert("medium".into(), tier);
        let config = RouterConfig {
            mode: "round_robin".into(),
            agents: vec!["claude".into()],
            model_map,
            ..Default::default()
        };
        let left_store = store.clone();
        let right_store = store.clone();
        let left_task = ext.clone();
        let right_task = ext;
        let right_config = config.clone();
        let left = tokio::spawn(async move {
            let mut router = Router::new_for_test(config, vec!["claude".into()]);
            router.route(&left_task, &left_store, "owner/repo").await
        });
        let right = tokio::spawn(async move {
            let mut router = Router::new_for_test(right_config, vec!["claude".into()]);
            router.route(&right_task, &right_store, "owner/repo").await
        });
        assert!(left.await.unwrap().is_err());
        assert!(right.await.unwrap().is_err());
        let task = store.get(b).await.unwrap();
        assert_eq!(task.status, TaskStatus::Waiting);
        assert!(task.agent.is_none());
    }

    #[tokio::test]
    async fn refuse_if_waiting_blocks_manual_claim() {
        let store = TaskStore::open_memory().await.unwrap();
        let b = insert(&store, "owner/repo", "2", "B", "").await;
        store
            .replace_dependencies(
                b,
                &[NewDependency {
                    repo: "owner/repo".into(),
                    number: 1,
                    source: DepSource::Cli,
                }],
            )
            .await
            .unwrap();
        let err = refuse_if_waiting(&store, b).await.unwrap_err();
        assert!(err.to_string().contains("#1"));
        assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
    }
}

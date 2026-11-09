//! After a CI bootstrap task merges, require its default-branch checks

use crate::backends::{ExternalId, ExternalTask, Status};
use crate::engine;
use crate::engine::auto_merge;
use crate::engine::review_gate;
use crate::engine::tasks::TaskManager;
use crate::github::http::{
    GhHttp, REQUIRED_CHECK_RULESET_NAME, RulesetCreateKind, classify_ruleset_create_error,
};
use crate::github::types::{GitHubCheckRun, RequiredStatusCheck};
use crate::plan;
use crate::store;
use crate::store::TaskStore;
use std::sync::Arc;

const KV_PENDING: &str = "pending";
const KV_APPLIED: &str = "applied";
const KV_REFUSED: &str = "refused";

#[derive(Debug, PartialEq)]
pub(crate) enum ChecksReady {
    Ready(Vec<RequiredStatusCheck>),
    /// A real check is still queued or in progress
    Pending,
    /// A real check finished as failure, error, cancelled, or timed out
    Failed(Vec<FailedCheck>),
    /// No real check has started
    Empty,
}

#[derive(Debug, PartialEq)]
pub(crate) struct FailedCheck {
    name: String,
    conclusion: String,
}

pub(crate) fn is_ci_bootstrap_task(body: &str) -> bool {
    plan::is_ci_bootstrap_body(body)
        || plan::marker_id(body).is_some_and(|id| plan::is_ci_bootstrap_id(&id))
}

pub(crate) fn kv_key(repo: &str, task_id: &str) -> String {
    format!("ci-bootstrap:{repo}:{task_id}")
}

/// True when this merged CI bootstrap task must stay approved until a ruleset exists
pub(crate) async fn hold_after_merge(
    task: &ExternalTask,
    store: &Arc<TaskStore>,
    repo: &str,
) -> bool {
    if !is_ci_bootstrap_task(&task.body) {
        return false;
    }
    let key = kv_key(repo, &task.id.0);
    match store.kv_get(&key).await {
        Ok(Some(state)) if state == KV_APPLIED => false,
        Ok(Some(_)) => true,
        Ok(None) => {
            if let Err(e) = store.kv_set(&key, KV_PENDING).await {
                tracing::warn!(
                    task_id = %task.id.0,
                    err = %e,
                    "failed to record CI bootstrap pending"
                );
            }
            true
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task.id.0,
                err = %e,
                "failed to read CI bootstrap state"
            );
            true
        }
    }
}

pub(crate) async fn apply_pending(
    store: &Arc<TaskStore>,
    task_manager: &Arc<TaskManager>,
    repo: &str,
) -> anyhow::Result<()> {
    let gh = GhHttp::owner()?;
    apply_pending_with(&gh, store, task_manager, repo).await
}

pub(crate) async fn apply_pending_with(
    gh: &GhHttp,
    store: &Arc<TaskStore>,
    task_manager: &Arc<TaskManager>,
    repo: &str,
) -> anyhow::Result<()> {
    let prefix = format!("ci-bootstrap:{repo}:");
    let entries = store.kv_list_prefix(&prefix).await?;
    for (key, state) in entries {
        if state == KV_APPLIED {
            continue;
        }
        let Some(task_id) = key.strip_prefix(&prefix) else {
            continue;
        };
        if let Err(e) = apply_one(gh, store, task_manager, repo, task_id, &state).await {
            tracing::warn!(task_id, err = %e, "CI bootstrap ruleset apply failed");
        }
    }
    Ok(())
}

pub(crate) fn checks_ready_for_ruleset(
    runs: &[GitHubCheckRun],
    statuses: &[(String, String)],
) -> ChecksReady {
    let mut ready: Vec<RequiredStatusCheck> = Vec::new();
    let mut pending = false;
    let mut failed: Vec<FailedCheck> = Vec::new();

    for run in runs {
        if review_gate::is_own_check(&run.name) {
            continue;
        }
        match run.status.as_str() {
            "completed" => match run.conclusion.as_deref() {
                Some("success") => {
                    let integration_id = run.app.as_ref().map(|app| app.id).filter(|&id| id > 0);
                    if !ready.iter().any(|check| check.context == run.name) {
                        ready.push(RequiredStatusCheck {
                            context: run.name.clone(),
                            integration_id,
                        });
                    }
                }
                Some("skipped" | "neutral") => {}
                Some(conclusion) => failed.push(FailedCheck {
                    name: run.name.clone(),
                    conclusion: conclusion.to_string(),
                }),
                None => pending = true,
            },
            _ => pending = true,
        }
    }

    for (context, state) in statuses {
        if review_gate::is_own_check(context) {
            continue;
        }
        match state.as_str() {
            "pending" => pending = true,
            "success" => {
                if !ready.iter().any(|check| &check.context == context) {
                    ready.push(RequiredStatusCheck {
                        context: context.clone(),
                        integration_id: None,
                    });
                }
            }
            "failure" | "error" => failed.push(FailedCheck {
                name: context.clone(),
                conclusion: state.clone(),
            }),
            _ => {}
        }
    }

    if !failed.is_empty() {
        ChecksReady::Failed(failed)
    } else if pending {
        ChecksReady::Pending
    } else if ready.is_empty() {
        ChecksReady::Empty
    } else {
        ChecksReady::Ready(ready)
    }
}

fn required_contexts_cover(
    required: &[RequiredStatusCheck],
    needed: &[RequiredStatusCheck],
) -> bool {
    needed.iter().all(|check| {
        required
            .iter()
            .any(|existing| existing.context == check.context)
    })
}

fn merge_missing_checks(
    mut existing: Vec<RequiredStatusCheck>,
    needed: &[RequiredStatusCheck],
) -> Vec<RequiredStatusCheck> {
    for check in needed {
        if !existing.iter().any(|e| e.context == check.context) {
            existing.push(check.clone());
        }
    }
    existing
}

async fn apply_one(
    gh: &GhHttp,
    store: &Arc<TaskStore>,
    task_manager: &Arc<TaskManager>,
    repo: &str,
    task_id: &str,
    state: &str,
) -> anyhow::Result<()> {
    if store::opt_store_get_task(&Some(Arc::clone(store)), repo, task_id)
        .await
        .is_none()
    {
        return Ok(());
    }

    let branch = gh.get_default_branch(repo).await?;
    let required = gh.get_required_status_check_contexts(repo, &branch).await?;
    let sha = gh.get_branch_sha(repo, &branch).await?;
    let runs = gh.get_check_runs(repo, &sha).await?;
    let statuses = gh.get_commit_status_contexts(repo, &sha).await?;
    let checks = match checks_ready_for_ruleset(&runs, &statuses) {
        ChecksReady::Pending => {
            tracing::info!(
                task_id,
                sha = %sha,
                "CI bootstrap waiting for default-branch checks"
            );
            reopen_if_closed(gh, repo, task_id).await;
            clear_wait(store, repo, task_id).await;
            return Ok(());
        }
        ChecksReady::Empty => {
            reopen_if_closed(gh, repo, task_id).await;
            return wait_for_checks(gh, store, repo, task_id, &sha).await;
        }
        ChecksReady::Failed(failed) => {
            reopen_if_closed(gh, repo, task_id).await;
            clear_wait(store, repo, task_id).await;
            return handle_failed_checks(gh, store, task_manager, repo, task_id, &sha, &failed)
                .await;
        }
        ChecksReady::Ready(checks) => {
            clear_wait(store, repo, task_id).await;
            checks
        }
    };

    if required_contexts_cover(&required, &checks) {
        return finish_applied(store, task_manager, task_id, repo).await;
    }

    // GitHub may have closed the issue via Closes on the PR, keep it open until the ruleset exists
    reopen_if_closed(gh, repo, task_id).await;
    if state == KV_REFUSED {
        return Ok(());
    }

    match gh.create_required_check_ruleset(repo, &checks).await {
        Ok(()) => finish_applied(store, task_manager, task_id, repo).await,
        Err(e) => {
            let err = e.to_string();
            match classify_ruleset_create_error(&err) {
                RulesetCreateKind::NameTaken => {
                    apply_existing_orch_ruleset(gh, store, task_manager, repo, task_id, &checks)
                        .await
                }
                kind => handle_ruleset_write_error(gh, store, repo, task_id, &err, kind).await,
            }
        }
    }
}

async fn apply_existing_orch_ruleset(
    gh: &GhHttp,
    store: &Arc<TaskStore>,
    task_manager: &Arc<TaskManager>,
    repo: &str,
    task_id: &str,
    needed: &[RequiredStatusCheck],
) -> anyhow::Result<()> {
    let Some((id, existing)) = gh
        .get_named_ruleset_required_checks(repo, REQUIRED_CHECK_RULESET_NAME)
        .await?
    else {
        tracing::info!(
            task_id,
            "CI bootstrap ruleset name taken, existing ruleset not found"
        );
        return Ok(());
    };
    if required_contexts_cover(&existing, needed) {
        return finish_applied(store, task_manager, task_id, repo).await;
    }
    let merged = merge_missing_checks(existing, needed);
    match gh.update_required_check_ruleset(repo, id, &merged).await {
        Ok(()) => finish_applied(store, task_manager, task_id, repo).await,
        Err(e) => {
            let err = e.to_string();
            handle_ruleset_write_error(
                gh,
                store,
                repo,
                task_id,
                &err,
                classify_ruleset_create_error(&err),
            )
            .await
        }
    }
}

async fn handle_ruleset_write_error(
    gh: &GhHttp,
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    err: &str,
    kind: RulesetCreateKind,
) -> anyhow::Result<()> {
    match kind {
        RulesetCreateKind::NameTaken | RulesetCreateKind::Retryable | RulesetCreateKind::Other => {
            tracing::info!(
                task_id,
                err = %err,
                "CI bootstrap ruleset create retrying"
            );
            Ok(())
        }
        RulesetCreateKind::CheckNeverRun => {
            tracing::info!(
                task_id,
                err = %err,
                "CI bootstrap ruleset create waiting, check has not run"
            );
            comment_never_run_once(gh, store, repo, task_id, err).await;
            Ok(())
        }
        RulesetCreateKind::Refused => {
            comment_refused(gh, repo, task_id, err).await;
            store.kv_set(&kv_key(repo, task_id), KV_REFUSED).await?;
            Ok(())
        }
    }
}

async fn finish_applied(
    store: &Arc<TaskStore>,
    task_manager: &Arc<TaskManager>,
    task_id: &str,
    repo: &str,
) -> anyhow::Result<()> {
    store.kv_set(&kv_key(repo, task_id), KV_APPLIED).await?;
    task_manager
        .update_task_status(&ExternalId(task_id.to_string()), Status::Done)
        .await?;
    Ok(())
}

async fn reopen_if_closed(gh: &GhHttp, repo: &str, task_id: &str) {
    match gh.get_issue(repo, task_id).await {
        Ok(issue) if issue.state.eq_ignore_ascii_case("closed") => {
            if let Err(e) = gh.reopen_issue(repo, task_id).await {
                tracing::warn!(
                    task_id,
                    err = %e,
                    "failed to reopen CI bootstrap issue"
                );
            }
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                task_id,
                err = %e,
                "failed to read CI bootstrap issue"
            );
        }
    }
}

fn wait_prefix(repo: &str, task_id: &str) -> String {
    format!("ci_bootstrap_wait_since:{repo}#{task_id}#")
}

fn no_checks_comment_key(repo: &str, task_id: &str, sha: &str) -> String {
    format!("ci_bootstrap_no_checks_comment:{repo}#{task_id}#{sha}")
}

fn never_run_comment_key(repo: &str, task_id: &str) -> String {
    format!("ci_bootstrap_never_run_comment:{repo}#{task_id}")
}

fn format_failed(failed: &[FailedCheck]) -> String {
    failed
        .iter()
        .map(|check| format!("{}: {}", check.name, check.conclusion))
        .collect::<Vec<_>>()
        .join(", ")
}

async fn clear_wait(store: &Arc<TaskStore>, repo: &str, task_id: &str) {
    let prefix = wait_prefix(repo, task_id);
    let keys = match store.kv_list_prefix(&prefix).await {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(task_id, err = %e, "failed to list CI bootstrap wait keys");
            return;
        }
    };
    for (key, _) in keys {
        if let Err(e) = store.kv_delete(&key).await {
            tracing::warn!(task_id, err = %e, "failed to clear CI bootstrap wait key");
        }
    }
}

async fn wait_for_checks(
    gh: &GhHttp,
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    sha: &str,
) -> anyhow::Result<()> {
    let prefix = wait_prefix(repo, task_id);
    let key = format!("{prefix}{sha}");
    let now = chrono::Utc::now().timestamp();
    let since = review_gate::wait_since(store, &prefix, &key, now).await?;
    if now - since < review_gate::NO_CHECKS_WAIT_SECS {
        tracing::info!(
            task_id,
            sha = %sha,
            "CI bootstrap waiting for a default-branch check to start"
        );
        return Ok(());
    }
    let wait_mins = review_gate::NO_CHECKS_WAIT_SECS / 60;
    tracing::warn!(
        task_id,
        sha = %sha,
        wait_mins,
        "CI bootstrap has no default-branch check after the wait"
    );
    let comment_key = no_checks_comment_key(repo, task_id, sha);
    if store.kv_get(&comment_key).await?.is_none() {
        comment_issue(
            gh,
            repo,
            task_id,
            &format!(
                "No default-branch check has started {wait_mins} minutes after the CI bootstrap merge. Later tasks stay waiting until a check runs."
            ),
        )
        .await;
        store.kv_set(&comment_key, "1").await?;
    }
    Ok(())
}

async fn handle_failed_checks(
    gh: &GhHttp,
    store: &Arc<TaskStore>,
    task_manager: &Arc<TaskManager>,
    repo: &str,
    task_id: &str,
    sha: &str,
    failed: &[FailedCheck],
) -> anyhow::Result<()> {
    let details = format_failed(failed);
    let failures = match auto_merge::count_ci_failure_for_head(store, repo, task_id, sha).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            tracing::info!(
                task_id,
                sha = %sha,
                details,
                "CI bootstrap default-branch checks still failed on the same head"
            );
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(
                task_id,
                err = %e,
                "failed to increment ci_merge_failures for CI bootstrap"
            );
            return Ok(());
        }
    };

    comment_issue(
        gh,
        repo,
        task_id,
        &format!(
            "Default-branch checks failed after the CI bootstrap merge ({details}). Later tasks stay waiting until those checks pass."
        ),
    )
    .await;

    let max = auto_merge::MAX_CI_MERGE_FAILURES;
    let id = ExternalId(task_id.to_string());
    if failures >= max {
        tracing::error!(
            task_id,
            failures,
            details,
            "CI bootstrap default-branch checks failed, CI failure limit reached"
        );
        let fields = [(
            "block_reason",
            serde_json::json!(format!(
                "CI failure limit ({max}) reached after the CI bootstrap merge"
            )),
        )];
        if let Err(e) = task_manager
            .update_task_status_and_result(&id, Status::Blocked, &fields)
            .await
        {
            tracing::error!(
                task_id,
                err = %e,
                "failed to block CI bootstrap after failed default-branch checks"
            );
        }
    } else {
        tracing::warn!(
            task_id,
            failures,
            details,
            "CI bootstrap default-branch checks failed, routing back to the task agent"
        );
        let fields = [
            (
                "last_error",
                serde_json::json!(format!(
                    "CI failed on the default branch after the CI bootstrap merge: {details}. Fix the failing checks"
                )),
            ),
            (
                "route_reason",
                serde_json::json!("re-dispatch after CI bootstrap default-branch checks failed"),
            ),
        ];
        if let Err(e) = task_manager
            .update_task_status_and_result(&id, Status::Routed, &fields)
            .await
        {
            tracing::error!(
                task_id,
                err = %e,
                "failed to re-route CI bootstrap after failed default-branch checks"
            );
        }
    }
    Ok(())
}

async fn comment_never_run_once(
    gh: &GhHttp,
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    err: &str,
) {
    let key = never_run_comment_key(repo, task_id);
    match store.kv_get(&key).await {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                task_id,
                err = %e,
                "failed to read CI bootstrap never-run comment key"
            );
            return;
        }
    }
    comment_issue(
        gh,
        repo,
        task_id,
        &format!(
            "GitHub does not treat the default-branch checks as valid required checks ({err}). Later tasks stay waiting until a required-checks ruleset can be created."
        ),
    )
    .await;
    if let Err(e) = store.kv_set(&key, "1").await {
        tracing::warn!(
            task_id,
            err = %e,
            "failed to record CI bootstrap never-run comment"
        );
    }
}

async fn comment_refused(gh: &GhHttp, repo: &str, task_id: &str, err: &str) {
    comment_issue(
        gh,
        repo,
        task_id,
        &format!(
            "GitHub refused a required-checks ruleset on the default branch ({err}). Turn branch protection on for this repository. Later tasks stay waiting until that happens."
        ),
    )
    .await;
}

async fn comment_issue(gh: &GhHttp, repo: &str, task_id: &str, message: &str) {
    let body = format!("{message}{}", engine::orch_footer());
    if let Err(e) = gh.add_comment(repo, task_id, &body).await {
        tracing::warn!(
            task_id,
            err = %e,
            "failed to comment on CI bootstrap issue"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::ExternalBackend;
    use crate::backends::test_helpers::NoopBackend;
    use crate::github::types::GitHubApp;
    use crate::plan::{CI_BOOTSTRAP_MARKER, ci_bootstrap_task, issue_body};
    use crate::store::TaskStatus;
    use crate::store::UpsertExternal;
    use std::env;
    use std::ffi::OsString;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = env::var_os(key);
            // SAFETY: nextest runs each test in its own process
            unsafe { env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                // SAFETY: nextest runs each test in its own process
                Some(value) => unsafe { env::set_var(self.key, value) },
                // SAFETY: nextest runs each test in its own process
                None => unsafe { env::remove_var(self.key) },
            }
        }
    }

    fn run(
        name: &str,
        status: &str,
        conclusion: Option<&str>,
        app_id: Option<i64>,
    ) -> GitHubCheckRun {
        GitHubCheckRun {
            id: 1,
            name: name.to_string(),
            status: status.to_string(),
            conclusion: conclusion.map(String::from),
            app: app_id.map(|id| GitHubApp { id }),
        }
    }

    fn bootstrap_body() -> String {
        issue_body(
            &ci_bootstrap_task(false),
            "acme/app",
            "main",
            "docs/ROADMAP.md",
        )
    }

    async fn pending_store() -> (Arc<TaskStore>, Arc<TaskManager>, i64) {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            Arc::clone(&store),
            "o/r".to_string(),
        ));
        let id = store
            .upsert_external(&UpsertExternal {
                repo: "o/r",
                ext_id: "12",
                title: "CI-01 Set up CI and tests",
                body: &bootstrap_body(),
                author: "u",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store.update_status(id, TaskStatus::Approved).await.unwrap();
        store
            .kv_set(&kv_key("o/r", "12"), KV_PENDING)
            .await
            .unwrap();
        (store, task_manager, id)
    }

    async fn mock_unprotected_main(server: &MockServer, sha: &str) {
        Mock::given(method("GET"))
            .and(path("/repos/o/r"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_branch": "main"
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/branches/main/protection"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_string("{\"message\":\"Branch not protected\"}"),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/rules/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commit": { "sha": sha }
            })))
            .mount(server)
            .await;
    }

    async fn mock_classic_required(server: &MockServer, context: &str, sha: &str) {
        Mock::given(method("GET"))
            .and(path("/repos/o/r"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_branch": "main"
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/branches/main/protection"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "required_status_checks": {
                    "checks": [{ "context": context, "app_id": 15368 }]
                }
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/rules/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commit": { "sha": sha }
            })))
            .mount(server)
            .await;
    }

    async fn mock_name_taken_ruleset(server: &MockServer, ruleset_checks: serde_json::Value) {
        Mock::given(method("POST"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
                "message": "Name has already been taken"
            })))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": 9, "name": REQUIRED_CHECK_RULESET_NAME }
            ])))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/rulesets/9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 9,
                "name": REQUIRED_CHECK_RULESET_NAME,
                "rules": [{
                    "type": "required_status_checks",
                    "parameters": {
                        "required_status_checks": ruleset_checks
                    }
                }]
            })))
            .mount(server)
            .await;
    }

    async fn mock_runs_and_empty_status(
        server: &MockServer,
        sha: &str,
        check_runs: serde_json::Value,
    ) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/o/r/commits/{sha}/check-runs")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 1,
                "check_runs": check_runs
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/o/r/commits/{sha}/status")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "pending",
                "total_count": 0,
                "statuses": []
            })))
            .mount(server)
            .await;
    }

    async fn mock_issue_comment(server: &MockServer, expected: u64) {
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/12/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 1
            })))
            .expect(expected)
            .mount(server)
            .await;
    }

    #[test]
    fn ruleset_write_keeps_429_transport_and_422_with_401_digits_pending() {
        for err in [
            "GitHub API POST /rulesets failed (429 Too Many Requests): {\"message\":\"API rate limit exceeded\"}",
            "HTTP request failed to send (transport error): connection reset by peer",
            "failed (422 Unprocessable Entity): {\"message\":\"Ruleset validation failed for rule 401\"}",
        ] {
            assert!(
                !matches!(
                    classify_ruleset_create_error(err),
                    RulesetCreateKind::Refused
                ),
                "{err}"
            );
        }
    }

    #[test]
    fn is_ci_bootstrap_task_reads_the_body_marker() {
        assert!(is_ci_bootstrap_task(&bootstrap_body()));
        assert!(is_ci_bootstrap_task(&format!("{CI_BOOTSTRAP_MARKER}\n")));
        assert!(is_ci_bootstrap_task("goal\n<!-- orch-plan: CI-01 -->"));
        assert!(!is_ci_bootstrap_task(""));
        assert!(!is_ci_bootstrap_task("goal"));
    }

    #[test]
    fn checks_ready_for_ruleset_splits_pending_failed_and_empty() {
        assert_eq!(checks_ready_for_ruleset(&[], &[]), ChecksReady::Empty);
        assert_eq!(
            checks_ready_for_ruleset(&[run("test", "in_progress", None, None)], &[]),
            ChecksReady::Pending
        );
        assert_eq!(
            checks_ready_for_ruleset(&[run("test", "completed", Some("failure"), None)], &[]),
            ChecksReady::Failed(vec![FailedCheck {
                name: "test".to_string(),
                conclusion: "failure".to_string(),
            }])
        );
        assert_eq!(
            checks_ready_for_ruleset(&[run("test", "completed", Some("cancelled"), None)], &[]),
            ChecksReady::Failed(vec![FailedCheck {
                name: "test".to_string(),
                conclusion: "cancelled".to_string(),
            }])
        );
        assert_eq!(
            checks_ready_for_ruleset(&[], &[("ci/test".to_string(), "failure".to_string())]),
            ChecksReady::Failed(vec![FailedCheck {
                name: "ci/test".to_string(),
                conclusion: "failure".to_string(),
            }])
        );
        assert_eq!(
            checks_ready_for_ruleset(&[run("test", "completed", Some("skipped"), None)], &[]),
            ChecksReady::Empty
        );
        assert_eq!(
            checks_ready_for_ruleset(
                &[run("orch/review", "completed", Some("success"), None)],
                &[]
            ),
            ChecksReady::Empty
        );
        assert_eq!(
            checks_ready_for_ruleset(
                &[
                    run("test", "completed", Some("failure"), None),
                    run("lint", "in_progress", None, None),
                ],
                &[]
            ),
            ChecksReady::Failed(vec![FailedCheck {
                name: "test".to_string(),
                conclusion: "failure".to_string(),
            }])
        );
        let ChecksReady::Ready(ready) = checks_ready_for_ruleset(
            &[
                run("test", "completed", Some("success"), Some(15368)),
                run("review-gate", "completed", Some("success"), None),
            ],
            &[],
        ) else {
            panic!("expected ready checks");
        };
        assert_eq!(
            ready,
            vec![RequiredStatusCheck {
                context: "test".to_string(),
                integration_id: Some(15368),
            }]
        );
    }

    #[test]
    fn required_contexts_cover_matches_by_context_name() {
        let required = vec![RequiredStatusCheck {
            context: "cla/check".to_string(),
            integration_id: None,
        }];
        let needed = vec![RequiredStatusCheck {
            context: "test".to_string(),
            integration_id: Some(15368),
        }];
        assert!(!required_contexts_cover(&required, &needed));
        let required = vec![
            RequiredStatusCheck {
                context: "cla/check".to_string(),
                integration_id: None,
            },
            RequiredStatusCheck {
                context: "test".to_string(),
                integration_id: None,
            },
        ];
        assert!(required_contexts_cover(&required, &needed));
    }

    #[test]
    fn merge_missing_checks_appends_contexts_the_ruleset_lacks() {
        let existing = vec![RequiredStatusCheck {
            context: "cla/check".to_string(),
            integration_id: None,
        }];
        let needed = vec![
            RequiredStatusCheck {
                context: "cla/check".to_string(),
                integration_id: Some(1),
            },
            RequiredStatusCheck {
                context: "test".to_string(),
                integration_id: Some(15368),
            },
        ];
        assert_eq!(
            merge_missing_checks(existing, &needed),
            vec![
                RequiredStatusCheck {
                    context: "cla/check".to_string(),
                    integration_id: None,
                },
                RequiredStatusCheck {
                    context: "test".to_string(),
                    integration_id: Some(15368),
                },
            ]
        );
    }

    #[tokio::test]
    async fn hold_after_merge_records_pending_for_a_bootstrap_task() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task = ExternalTask {
            id: ExternalId("12".to_string()),
            title: "CI-01 Set up CI and tests".to_string(),
            body: bootstrap_body(),
            state: "open".to_string(),
            labels: vec![],
            author: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            url: String::new(),
        };
        assert!(hold_after_merge(&task, &store, "o/r").await);
        assert_eq!(
            store.kv_get(&kv_key("o/r", "12")).await.unwrap().as_deref(),
            Some(KV_PENDING)
        );
        store
            .kv_set(&kv_key("o/r", "12"), KV_APPLIED)
            .await
            .unwrap();
        assert!(!hold_after_merge(&task, &store, "o/r").await);
    }

    #[tokio::test]
    async fn hold_after_merge_skips_a_title_without_a_plan_marker() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task = ExternalTask {
            id: ExternalId("12".to_string()),
            title: "CI-01 Set up CI and tests".to_string(),
            body: "ordinary issue".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            url: String::new(),
        };
        assert!(!hold_after_merge(&task, &store, "o/r").await);
        assert!(store.kv_get(&kv_key("o/r", "12")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn apply_pending_dones_when_required_checks_already_cover_default_branch_checks() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        mock_classic_required(&server, "test", "deadbeef").await;
        mock_runs_and_empty_status(
            &server,
            "deadbeef",
            serde_json::json!([{
                "id": 1,
                "name": "test",
                "status": "completed",
                "conclusion": "success",
                "app": { "id": 15368 }
            }]),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 9
            })))
            .expect(0)
            .mount(&server)
            .await;

        let (store, task_manager, id) = pending_store().await;
        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Done);
        assert_eq!(
            store.kv_get(&kv_key("o/r", "12")).await.unwrap().as_deref(),
            Some(KV_APPLIED)
        );
    }

    #[tokio::test]
    async fn apply_pending_creates_a_ruleset_when_an_unrelated_required_check_exists() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        mock_classic_required(&server, "cla/check", "deadbeef").await;
        mock_runs_and_empty_status(
            &server,
            "deadbeef",
            serde_json::json!([{
                "id": 1,
                "name": "test",
                "status": "completed",
                "conclusion": "success",
                "app": { "id": 15368 }
            }]),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 9,
                "name": REQUIRED_CHECK_RULESET_NAME
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (store, task_manager, id) = pending_store().await;
        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Done);
        assert_eq!(
            store.kv_get(&kv_key("o/r", "12")).await.unwrap().as_deref(),
            Some(KV_APPLIED)
        );
    }

    #[tokio::test]
    async fn apply_pending_holds_when_an_unrelated_required_check_exists_and_checks_are_pending() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        mock_classic_required(&server, "cla/check", "deadbeef").await;
        mock_runs_and_empty_status(
            &server,
            "deadbeef",
            serde_json::json!([{
                "id": 1,
                "name": "test",
                "status": "in_progress",
                "conclusion": null,
                "app": { "id": 15368 }
            }]),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 9
            })))
            .expect(0)
            .mount(&server)
            .await;

        let (store, task_manager, id) = pending_store().await;
        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Approved);
        assert_eq!(
            store.kv_get(&kv_key("o/r", "12")).await.unwrap().as_deref(),
            Some(KV_PENDING)
        );
    }

    #[tokio::test]
    async fn apply_pending_dones_when_name_taken_and_orch_ruleset_lists_the_checks() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        mock_unprotected_main(&server, "deadbeef").await;
        mock_runs_and_empty_status(
            &server,
            "deadbeef",
            serde_json::json!([{
                "id": 1,
                "name": "test",
                "status": "completed",
                "conclusion": "success",
                "app": { "id": 15368 }
            }]),
        )
        .await;
        mock_name_taken_ruleset(
            &server,
            serde_json::json!([{ "context": "test", "integration_id": 15368 }]),
        )
        .await;
        Mock::given(method("PUT"))
            .and(path("/repos/o/r/rulesets/9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 9
            })))
            .expect(0)
            .mount(&server)
            .await;

        let (store, task_manager, id) = pending_store().await;
        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Done);
        assert_eq!(
            store.kv_get(&kv_key("o/r", "12")).await.unwrap().as_deref(),
            Some(KV_APPLIED)
        );
    }

    #[tokio::test]
    async fn apply_pending_updates_the_orch_ruleset_when_name_taken_and_checks_are_missing() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        mock_unprotected_main(&server, "deadbeef").await;
        mock_runs_and_empty_status(
            &server,
            "deadbeef",
            serde_json::json!([{
                "id": 1,
                "name": "test",
                "status": "completed",
                "conclusion": "success",
                "app": { "id": 15368 }
            }]),
        )
        .await;
        mock_name_taken_ruleset(&server, serde_json::json!([{ "context": "cla/check" }])).await;
        Mock::given(method("PUT"))
            .and(path("/repos/o/r/rulesets/9"))
            .and(body_partial_json(serde_json::json!({
                "name": REQUIRED_CHECK_RULESET_NAME,
                "rules": [{
                    "type": "required_status_checks",
                    "parameters": {
                        "required_status_checks": [
                            { "context": "cla/check" },
                            { "context": "test", "integration_id": 15368 }
                        ]
                    }
                }]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 9,
                "name": REQUIRED_CHECK_RULESET_NAME
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (store, task_manager, id) = pending_store().await;
        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Done);
        assert_eq!(
            store.kv_get(&kv_key("o/r", "12")).await.unwrap().as_deref(),
            Some(KV_APPLIED)
        );
    }

    #[tokio::test]
    async fn apply_pending_creates_a_ruleset_when_default_branch_checks_pass() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_branch": "main"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/branches/main/protection"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_string("{\"message\":\"Branch not protected\"}"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/rules/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commit": { "sha": "deadbeef" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/deadbeef/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 1,
                "check_runs": [{
                    "id": 1,
                    "name": "test",
                    "status": "completed",
                    "conclusion": "success",
                    "app": { "id": 15368 }
                }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/deadbeef/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "success",
                "total_count": 0,
                "statuses": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 9,
                "name": "orch required checks"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            Arc::clone(&store),
            "o/r".to_string(),
        ));
        let id = store
            .upsert_external(&UpsertExternal {
                repo: "o/r",
                ext_id: "12",
                title: "CI-01 Set up CI and tests",
                body: &bootstrap_body(),
                author: "u",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store.update_status(id, TaskStatus::Approved).await.unwrap();
        store
            .kv_set(&kv_key("o/r", "12"), KV_PENDING)
            .await
            .unwrap();

        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Done);
        assert_eq!(
            store.kv_get(&kv_key("o/r", "12")).await.unwrap().as_deref(),
            Some(KV_APPLIED)
        );
    }

    #[tokio::test]
    async fn apply_pending_comments_and_holds_when_github_refuses_the_ruleset() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_branch": "main"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/branches/main/protection"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_string("{\"message\":\"Branch not protected\"}"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/rules/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commit": { "sha": "deadbeef" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/deadbeef/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 1,
                "check_runs": [{
                    "id": 1,
                    "name": "test",
                    "status": "completed",
                    "conclusion": "success",
                    "app": { "id": 15368 }
                }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/deadbeef/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "success",
                "total_count": 0,
                "statuses": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "message": "Upgrade to GitHub Pro or make this repository public to enable this feature."
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/12/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 1
            })))
            .expect(1)
            .mount(&server)
            .await;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            Arc::clone(&store),
            "o/r".to_string(),
        ));
        let id = store
            .upsert_external(&UpsertExternal {
                repo: "o/r",
                ext_id: "12",
                title: "CI-01 Set up CI and tests",
                body: &bootstrap_body(),
                author: "u",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store.update_status(id, TaskStatus::Approved).await.unwrap();
        store
            .kv_set(&kv_key("o/r", "12"), KV_PENDING)
            .await
            .unwrap();

        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Approved);
        assert_eq!(
            store.kv_get(&kv_key("o/r", "12")).await.unwrap().as_deref(),
            Some(KV_REFUSED)
        );
    }

    #[tokio::test]
    async fn apply_pending_holds_when_ruleset_create_is_unrecognized_even_with_401_digits() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        mock_unprotected_main(&server, "deadbeef").await;
        mock_runs_and_empty_status(
            &server,
            "deadbeef",
            serde_json::json!([{
                "id": 1,
                "name": "test",
                "status": "completed",
                "conclusion": "success",
                "app": { "id": 15368 }
            }]),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
                "message": "Ruleset validation failed for rule 401"
            })))
            .mount(&server)
            .await;
        mock_issue_comment(&server, 0).await;

        let (store, task_manager, id) = pending_store().await;
        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Approved);
        assert_eq!(
            store.kv_get(&kv_key("o/r", "12")).await.unwrap().as_deref(),
            Some(KV_PENDING)
        );
    }

    #[tokio::test]
    async fn apply_pending_reopens_a_closed_issue_and_creates_the_ruleset() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_branch": "main"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/branches/main/protection"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_string("{\"message\":\"Branch not protected\"}"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/rules/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/12"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 12,
                "id": 120,
                "title": "CI-01 Set up CI and tests",
                "body": "",
                "state": "closed",
                "labels": [],
                "user": { "login": "u" },
                "created_at": "",
                "updated_at": "",
                "html_url": ""
            })))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/o/r/issues/12"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 12,
                "id": 120,
                "title": "CI-01 Set up CI and tests",
                "body": "",
                "state": "open",
                "labels": [],
                "user": { "login": "u" },
                "created_at": "",
                "updated_at": "",
                "html_url": ""
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commit": { "sha": "deadbeef" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/deadbeef/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 1,
                "check_runs": [{
                    "id": 1,
                    "name": "test",
                    "status": "completed",
                    "conclusion": "success",
                    "app": { "id": 15368 }
                }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/deadbeef/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "state": "success",
                "total_count": 0,
                "statuses": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 9,
                "name": "orch required checks"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            Arc::clone(&store),
            "o/r".to_string(),
        ));
        let id = store
            .upsert_external(&UpsertExternal {
                repo: "o/r",
                ext_id: "12",
                title: "CI-01 Set up CI and tests",
                body: &bootstrap_body(),
                author: "u",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store.update_status(id, TaskStatus::Done).await.unwrap();
        store
            .kv_set(&kv_key("o/r", "12"), KV_PENDING)
            .await
            .unwrap();

        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Done);
        assert_eq!(
            store.kv_get(&kv_key("o/r", "12")).await.unwrap().as_deref(),
            Some(KV_APPLIED)
        );
    }

    #[tokio::test]
    async fn apply_pending_logs_and_holds_while_default_branch_checks_run() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        mock_unprotected_main(&server, "deadbeef").await;
        mock_runs_and_empty_status(
            &server,
            "deadbeef",
            serde_json::json!([{
                "id": 1,
                "name": "test",
                "status": "in_progress",
                "conclusion": null,
                "app": { "id": 15368 }
            }]),
        )
        .await;
        mock_issue_comment(&server, 0).await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 9
            })))
            .expect(0)
            .mount(&server)
            .await;

        let (store, task_manager, id) = pending_store().await;
        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Approved);
        assert_eq!(
            store.kv_get(&kv_key("o/r", "12")).await.unwrap().as_deref(),
            Some(KV_PENDING)
        );
    }

    #[tokio::test]
    async fn apply_pending_comments_and_routes_when_default_branch_checks_fail() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        mock_unprotected_main(&server, "deadbeef").await;
        mock_runs_and_empty_status(
            &server,
            "deadbeef",
            serde_json::json!([{
                "id": 1,
                "name": "test",
                "status": "completed",
                "conclusion": "failure",
                "app": { "id": 15368 }
            }]),
        )
        .await;
        mock_issue_comment(&server, 1).await;

        let (store, task_manager, id) = pending_store().await;
        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Routed);
        assert_eq!(stored.ci_merge_failures, 1);
        assert!(stored.last_error.contains("test: failure"));
        assert_eq!(
            store.kv_get(&kv_key("o/r", "12")).await.unwrap().as_deref(),
            Some(KV_PENDING)
        );

        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();
        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Routed);
        assert_eq!(stored.ci_merge_failures, 1);
    }

    #[tokio::test]
    async fn apply_pending_blocks_after_max_failed_default_branch_heads() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        mock_unprotected_main(&server, "deadbeef").await;
        mock_runs_and_empty_status(
            &server,
            "deadbeef",
            serde_json::json!([{
                "id": 1,
                "name": "test",
                "status": "completed",
                "conclusion": "cancelled",
                "app": { "id": 15368 }
            }]),
        )
        .await;
        mock_issue_comment(&server, 1).await;

        let (store, task_manager, id) = pending_store().await;
        store.increment(id, "ci_merge_failures").await.unwrap();
        store.increment(id, "ci_merge_failures").await.unwrap();
        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Blocked);
        assert_eq!(
            stored.ci_merge_failures,
            i32::try_from(auto_merge::MAX_CI_MERGE_FAILURES).unwrap()
        );
        assert!(
            stored
                .block_reason
                .as_deref()
                .unwrap_or("")
                .contains("CI failure limit")
        );
    }

    #[tokio::test]
    async fn apply_pending_waits_before_commenting_when_no_check_starts() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        mock_unprotected_main(&server, "deadbeef").await;
        mock_runs_and_empty_status(&server, "deadbeef", serde_json::json!([])).await;
        mock_issue_comment(&server, 0).await;

        let (store, task_manager, id) = pending_store().await;
        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Approved);
        assert!(
            store
                .kv_get("ci_bootstrap_wait_since:o/r#12#deadbeef")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .kv_get(&no_checks_comment_key("o/r", "12", "deadbeef"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn apply_pending_comments_once_when_no_check_starts_after_the_wait() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        mock_unprotected_main(&server, "deadbeef").await;
        mock_runs_and_empty_status(&server, "deadbeef", serde_json::json!([])).await;
        mock_issue_comment(&server, 1).await;

        let (store, task_manager, id) = pending_store().await;
        let started = chrono::Utc::now().timestamp() - review_gate::NO_CHECKS_WAIT_SECS - 1;
        store
            .kv_set(
                "ci_bootstrap_wait_since:o/r#12#deadbeef",
                &started.to_string(),
            )
            .await
            .unwrap();
        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Approved);
        assert_eq!(
            store
                .kv_get(&no_checks_comment_key("o/r", "12", "deadbeef"))
                .await
                .unwrap()
                .as_deref(),
            Some("1")
        );

        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();
        assert_eq!(store.get(id).await.unwrap().status, TaskStatus::Approved);
    }

    #[tokio::test]
    async fn apply_pending_comments_once_when_github_says_the_check_never_ran() {
        let _token = EnvVarGuard::set("GH_TOKEN", "test_token");
        let server = MockServer::start().await;
        mock_unprotected_main(&server, "deadbeef").await;
        mock_runs_and_empty_status(
            &server,
            "deadbeef",
            serde_json::json!([{
                "id": 1,
                "name": "test",
                "status": "completed",
                "conclusion": "success",
                "app": { "id": 15368 }
            }]),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
                "message": "test was not a valid check"
            })))
            .mount(&server)
            .await;
        mock_issue_comment(&server, 1).await;

        let (store, task_manager, id) = pending_store().await;
        let gh = GhHttp::for_test(server.uri(), None);
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();
        apply_pending_with(&gh, &store, &task_manager, "o/r")
            .await
            .unwrap();

        let stored = store.get(id).await.unwrap();
        assert_eq!(stored.status, TaskStatus::Approved);
        assert_eq!(
            store.kv_get(&kv_key("o/r", "12")).await.unwrap().as_deref(),
            Some(KV_PENDING)
        );
        assert_eq!(
            store
                .kv_get(&never_run_comment_key("o/r", "12"))
                .await
                .unwrap()
                .as_deref(),
            Some("1")
        );
    }
}

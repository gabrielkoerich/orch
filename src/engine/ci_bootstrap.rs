//! After a CI bootstrap task merges, require its default-branch checks

use crate::backends::{ExternalId, ExternalTask, Status};
use crate::engine;
use crate::engine::review_gate;
use crate::engine::tasks::TaskManager;
use crate::github::http::{GhHttp, RulesetCreateKind, classify_ruleset_create_error};
use crate::github::types::{GitHubCheckRun, RequiredStatusCheck};
use crate::plan;
use crate::store;
use crate::store::TaskStore;
use std::sync::Arc;

const KV_PENDING: &str = "pending";
const KV_APPLIED: &str = "applied";
const KV_REFUSED: &str = "refused";

pub(crate) fn is_ci_bootstrap_task(title: &str, body: &str) -> bool {
    if plan::is_ci_bootstrap_body(body)
        || plan::marker_id(body).is_some_and(|id| plan::is_ci_bootstrap_id(&id))
    {
        return true;
    }
    title
        .split_whitespace()
        .next()
        .is_some_and(plan::is_ci_bootstrap_id)
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
    if !is_ci_bootstrap_task(&task.title, &task.body) {
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
) -> Option<Vec<RequiredStatusCheck>> {
    let mut ready: Vec<RequiredStatusCheck> = Vec::new();
    let mut pending = false;
    let mut failed = false;

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
                Some(_) => failed = true,
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
            "failure" | "error" => failed = true,
            _ => {}
        }
    }

    if pending || failed || ready.is_empty() {
        None
    } else {
        Some(ready)
    }
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
    if !required.is_empty() {
        return finish_applied(store, task_manager, task_id, repo).await;
    }

    // GitHub may have closed the issue via Closes on the PR, keep it open until the ruleset exists
    reopen_if_closed(gh, repo, task_id).await;

    if state == KV_REFUSED {
        return Ok(());
    }

    let sha = gh.get_branch_sha(repo, &branch).await?;
    let runs = gh.get_check_runs(repo, &sha).await?;
    let statuses = gh.get_commit_status_contexts(repo, &sha).await?;
    let Some(checks) = checks_ready_for_ruleset(&runs, &statuses) else {
        return Ok(());
    };

    match gh.create_required_check_ruleset(repo, &checks).await {
        Ok(()) => finish_applied(store, task_manager, task_id, repo).await,
        Err(e) => {
            let err = e.to_string();
            match classify_ruleset_create_error(&err) {
                RulesetCreateKind::NameTaken => {
                    finish_applied(store, task_manager, task_id, repo).await
                }
                RulesetCreateKind::CheckNeverRun | RulesetCreateKind::Retryable => Ok(()),
                RulesetCreateKind::Refused | RulesetCreateKind::Other => {
                    comment_refused(gh, repo, task_id, &err).await;
                    store.kv_set(&kv_key(repo, task_id), KV_REFUSED).await?;
                    Ok(())
                }
            }
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

async fn comment_refused(gh: &GhHttp, repo: &str, task_id: &str, err: &str) {
    let body = format!(
        "GitHub refused a required-checks ruleset on the default branch ({err}). Turn branch protection on for this repository. Later tasks stay waiting until that happens.{}",
        engine::orch_footer()
    );
    if let Err(e) = gh.add_comment(repo, task_id, &body).await {
        tracing::warn!(
            task_id,
            err = %e,
            "failed to comment on CI bootstrap ruleset refusal"
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
    use wiremock::matchers::{method, path};
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

    #[test]
    fn is_ci_bootstrap_task_reads_the_body_marker() {
        assert!(is_ci_bootstrap_task(
            "CI-01 Set up CI and tests",
            &bootstrap_body()
        ));
        assert!(is_ci_bootstrap_task("CI-01", ""));
        assert!(is_ci_bootstrap_task("ci-01 extra", ""));
        assert!(is_ci_bootstrap_task(
            "other",
            &format!("{CI_BOOTSTRAP_MARKER}\n")
        ));
        assert!(!is_ci_bootstrap_task("M1-01 Set up the workspace", "goal"));
    }

    #[test]
    fn checks_ready_for_ruleset_waits_until_real_checks_pass() {
        assert!(checks_ready_for_ruleset(&[], &[]).is_none());
        assert!(checks_ready_for_ruleset(&[run("test", "in_progress", None, None)], &[]).is_none());
        assert!(
            checks_ready_for_ruleset(&[run("test", "completed", Some("failure"), None)], &[])
                .is_none()
        );
        assert!(
            checks_ready_for_ruleset(
                &[run("orch/review", "completed", Some("success"), None)],
                &[]
            )
            .is_none()
        );
        let ready = checks_ready_for_ruleset(
            &[
                run("test", "completed", Some("success"), Some(15368)),
                run("review-gate", "completed", Some("success"), None),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(
            ready,
            vec![RequiredStatusCheck {
                context: "test".to_string(),
                integration_id: Some(15368),
            }]
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
    async fn apply_pending_dones_when_required_checks_already_exist() {
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
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "required_status_checks": {
                    "checks": [{ "context": "test", "app_id": 15368 }]
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/rules/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
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
}

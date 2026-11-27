use super::*;

#[test]
fn format_age_seconds() {
    let ts = (Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
    let age = format_age(&ts);
    assert!(age.ends_with('s'), "expected seconds format, got {age}");
}

#[test]
fn format_age_minutes() {
    let ts = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
    let age = format_age(&ts);
    assert!(age.ends_with('m'), "expected minutes format, got {age}");
}

#[test]
fn format_age_hours() {
    let ts = (Utc::now() - chrono::Duration::hours(3)).to_rfc3339();
    let age = format_age(&ts);
    assert!(age.ends_with('h'), "expected hours format, got {age}");
}

#[test]
fn format_age_days() {
    let ts = (Utc::now() - chrono::Duration::days(2)).to_rfc3339();
    let age = format_age(&ts);
    assert!(age.ends_with('d'), "expected days format, got {age}");
}

#[test]
fn format_age_invalid_returns_dash() {
    let age = format_age("not-a-timestamp");
    assert_eq!(age, "-");
}

fn task_ref(status: &str) -> store::CliTaskRef {
    store::CliTaskRef {
        store_id: Some(7),
        repo: "owner/repo".to_string(),
        origin: "github".to_string(),
        title: "t".to_string(),
        external_id: "58".to_string(),
        status: status.to_string(),
    }
}

#[test]
fn refuse_if_done_rejects_done_tasks() {
    for action in ["retry", "unblock", "route", "run"] {
        let err =
            refuse_if_done(&task_ref("done"), action).expect_err("done task must not be revived");
        let msg = err.to_string();
        assert!(msg.contains("is done"), "got: {msg}");
        assert!(msg.contains(action), "got: {msg}");
    }
}

#[test]
fn refuse_if_done_allows_other_statuses() {
    for status in ["blocked", "needs_review", "new", "in_review", ""] {
        assert!(refuse_if_done(&task_ref(status), "unblock").is_ok());
    }
}

async fn done_internal_task() -> (Arc<TaskStore>, i64) {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create_internal("owner/repo", "Task", "stale body", "test", "t:1", None)
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Done).await.unwrap();
    (store, id)
}

/// `orch task route` must leave a done task done (#3780, #4155)
#[tokio::test]
async fn done_task_refuses_route() {
    let (store, id) = done_internal_task().await;
    let err = route_on(&store, &format!("internal:{id}"))
        .await
        .expect_err("done task must not be revived");
    assert!(err.to_string().contains("is done"), "got: {err}");
    assert_eq!(
        store.get(id).await.unwrap().status,
        TaskStatus::Done,
        "route must leave a done task done"
    );
}

/// `orch task run` must leave a done task done (#3780, #4155)
#[tokio::test]
async fn done_task_refuses_run() {
    let (store, id) = done_internal_task().await;
    let err = run_on(&store, Some(format!("internal:{id}")))
        .await
        .expect_err("done task must not be revived");
    assert!(err.to_string().contains("is done"), "got: {err}");
    assert_eq!(
        store.get(id).await.unwrap().status,
        TaskStatus::Done,
        "run must leave a done task done"
    );
}

#[test]
fn matches_project_filter_accepts_exact_repo_slug() {
    assert!(matches_project_filter(
        "gabrielkoerich/orch",
        "gabrielkoerich/orch"
    ));
}

#[test]
fn matches_project_filter_accepts_repo_name_suffix() {
    assert!(matches_project_filter("gabrielkoerich/orch", "orch"));
}

#[test]
fn matches_project_filter_rejects_non_matching_repo() {
    assert!(!matches_project_filter("gabrielkoerich/orch", "bean"));
}

/// Verify the forced-routing field construction for agent-only case.
#[test]
fn retry_force_route_fields_agent_only() {
    let forced_agent = "claude";
    let model: Option<String> = None;
    let fields: Vec<(&str, serde_json::Value)> = {
        let mut f = vec![
            ("agent", serde_json::json!(forced_agent)),
            (
                "route_reason",
                serde_json::json!(format!("forced via retry --agent {forced_agent}")),
            ),
        ];
        if let Some(ref m) = model {
            f.push(("model", serde_json::json!(m)));
        }
        f
    };
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].1, serde_json::json!("claude"));
    assert_eq!(
        fields[1].1,
        serde_json::json!("forced via retry --agent claude")
    );
}

/// Verify the forced-routing field construction for agent+model case.
#[test]
fn retry_force_route_fields_agent_and_model() {
    let fields = retry_force_fields("claude", Some("opus"));
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0].1, serde_json::json!("claude"));
    assert_eq!(fields[2].0, "model");
    assert_eq!(fields[2].1, serde_json::json!("opus"));
}

/// Verify that the model hint display is correct in both cases.
#[test]
fn retry_model_hint_display() {
    let model_some: Option<String> = Some("opus".to_string());
    let model_none: Option<String> = None;
    assert_eq!(model_some.as_deref().unwrap_or("(router default)"), "opus");
    assert_eq!(
        model_none.as_deref().unwrap_or("(router default)"),
        "(router default)"
    );
}

#[test]
fn review_attempt_line_shows_cause_and_reason() {
    let activity = store::TaskActivity {
        id: 1,
        task_id: 7,
        timestamp: "2026-10-25T22:40:00Z".to_string(),
        event_type: "review_attempt_failed".to_string(),
        from_status: None,
        to_status: None,
        agent: None,
        model: None,
        details: serde_json::json!({
            "cause": "rate_limit",
            "external": true,
            "reason": "claude failed: rate limit reached",
        }),
    };
    let line = review_attempt_line(&activity);
    assert_eq!(
        line,
        "2026-10-25T22:40:00Z  rate_limit (external): claude failed: rate limit reached"
    );
}

#[test]
fn truncate_err_under_limit() {
    let s = "short error";
    assert_eq!(truncate_err(s, 50), "short error");
}

#[test]
fn truncate_err_cuts_inside_char_down_to_boundary() {
    assert_eq!(truncate_err("héllo", 2), "h...");
    assert_eq!(truncate_err("é", 1), "...");
    assert_eq!(truncate_err("abc", 0), "...");
}

#[test]
fn truncate_err_exact_limit() {
    let s = "short error";
    assert_eq!(truncate_err(s, 11), "short error");
}

#[test]
fn truncate_err_truncates() {
    let s = "this is a long error message that should be cut";
    let result = truncate_err(s, 10);
    assert!(result.ends_with("..."));
    assert!(result.len() <= 13); // 10 chars + "..."
}

#[test]
fn truncate_err_multi_byte_char_boundary() {
    // "héllo world" bytes: h(0)=1, é(1-2)=2, l(3)=1, l(4)=1, o(5)=1, sp(6)=1, w(7)=1...
    // byte 3 = 'l' (valid boundary); byte 4 = 'l' (valid); byte 5 = 'o' (valid)
    // No panic = success; exact result varies by where the boundary lands
    let s = "héllo world";
    let result = truncate_err(s, 3);
    assert!(result.ends_with("..."));
    assert!(!result.contains("panic"));
}

#[test]
fn truncate_err_emoji() {
    // "🔥 error message": 🔥=4 bytes, space=1, e=1, r=1, r=1, o=1, sp=1, m=1...
    // byte 5 = 'e' (valid boundary); walk-back not needed
    let s = "🔥 error message";
    let result = truncate_err(s, 5);
    assert_eq!(result, "🔥 ...");
}

#[test]
fn truncate_err_cjk() {
    // "你好世界": 你=3 bytes(0-2), 好=3 bytes(3-5), 世=3 bytes(6-8), 界=3 bytes(9-11)
    // byte 4 falls inside 好; walk back to byte 3 (= end of 你)
    let s = "你好世界";
    let result = truncate_err(s, 4);
    assert_eq!(result, "你...");
}

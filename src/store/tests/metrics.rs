use super::super::*;
use crate::store::control::parse_since_duration;
use chrono::TimeDelta;
use chrono::Utc;

#[tokio::test]
async fn cost_summary_aggregates_correctly() {
    let store = TaskStore::open_memory().await.unwrap();

    let id1 = store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Task 1".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let id2 = store
        .create(&NewTask {
            external_id: Some("2".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Task 2".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .store_tokens(id1, 1000, 500, Some(0.25))
        .await
        .unwrap();
    store
        .store_tokens(id2, 2000, 1000, Some(0.5))
        .await
        .unwrap();

    let (input, output, cost) = store.cost_summary("owner/repo").await.unwrap();
    assert_eq!(input, 3000);
    assert_eq!(output, 1500);
    assert_eq!(cost, 0.75);
}

#[tokio::test]
async fn status_counts_groups_correctly() {
    let store = TaskStore::open_memory().await.unwrap();

    for i in 0..3 {
        store
            .create(&NewTask {
                external_id: Some(format!("{i}")),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: format!("Task {i}"),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    // Move task 1 to routed, task 2 to done
    store.update_status(2, TaskStatus::Routed).await.unwrap();
    store.update_status(3, TaskStatus::Done).await.unwrap();

    let counts = store.status_counts("owner/repo").await.unwrap();
    assert_eq!(counts.get("new"), Some(&1));
    assert_eq!(counts.get("routed"), Some(&1));
    assert_eq!(counts.get("done"), Some(&1));
}

#[tokio::test]
async fn cost_summary_returns_zeros_for_empty_repo() {
    let store = TaskStore::open_memory().await.unwrap();
    let (input, output, cost) = store.cost_summary("empty/repo").await.unwrap();
    assert_eq!(input, 0);
    assert_eq!(output, 0);
    assert!((cost - 0.0).abs() < f64::EPSILON);
}

#[tokio::test]
async fn status_counts_returns_empty_map_for_empty_repo() {
    let store = TaskStore::open_memory().await.unwrap();
    let counts = store.status_counts("empty/repo").await.unwrap();
    assert!(counts.is_empty());
}

#[tokio::test]
async fn cost_summary_across_multiple_tasks() {
    let store = TaskStore::open_memory().await.unwrap();

    for i in 1..=3 {
        let id = store
            .create(&NewTask {
                external_id: Some(i.to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: format!("Task {i}"),
                body: String::new(),
                source: String::new(),
                source_id: String::new(),
                author: String::new(),
                url: String::new(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap();
        store
            .store_tokens(id, 1000 * i, 500 * i, None)
            .await
            .unwrap();
    }

    let (input, output, _cost) = store.cost_summary("owner/repo").await.unwrap();
    assert_eq!(input, 6000); // 1000 + 2000 + 3000
    assert_eq!(output, 3000); // 500 + 1000 + 1500
}

#[tokio::test]
async fn status_counts_with_mixed_statuses() {
    let store = TaskStore::open_memory().await.unwrap();

    for i in 1..=5 {
        let id = store
            .create(&NewTask {
                external_id: Some(i.to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: format!("Task {i}"),
                body: String::new(),
                source: String::new(),
                source_id: String::new(),
                author: String::new(),
                url: String::new(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap();
        if i <= 2 {
            store
                .update_status(id, TaskStatus::InProgress)
                .await
                .unwrap();
        } else if i == 3 {
            store.update_status(id, TaskStatus::Done).await.unwrap();
        }
        // Tasks 4-5 stay New
    }

    let counts = store.status_counts("owner/repo").await.unwrap();
    assert_eq!(counts.get("new").copied().unwrap_or(0), 2);
    assert_eq!(counts.get("in_progress").copied().unwrap_or(0), 2);
    assert_eq!(counts.get("done").copied().unwrap_or(0), 1);
}

#[tokio::test]
async fn insert_and_query_task_metric() {
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    let metric = InsertTaskMetric {
        repo: "",
        task_id: "42",
        agent: "claude",
        model: Some("opus"),
        complexity: Some("complex"),
        outcome: "success",
        duration_seconds: 120.5,
        started_at: &now,
        completed_at: &now,
        attempts: 1,
        files_changed: 3,
        error_type: None,
        input_tokens: Some(5000),
        output_tokens: Some(2000),
        total_cost_usd: Some(0.15),
    };

    let id = store.insert_task_metric(&metric).await.unwrap();
    assert!(id > 0);
}

#[tokio::test]
async fn metrics_summary_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let summary = store.get_metrics_summary_24h().await.unwrap();
    assert_eq!(summary.tasks_completed_24h, 0);
    assert_eq!(summary.tasks_failed_24h, 0);
    assert!(summary.agent_stats.is_empty());
}

#[tokio::test]
async fn metrics_summary_counts_success_and_failure() {
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    // Insert a success
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "",
            task_id: "1",
            agent: "claude",
            model: None,
            complexity: Some("simple"),
            outcome: "success",
            duration_seconds: 60.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: None,
            output_tokens: None,
            total_cost_usd: None,
        })
        .await
        .unwrap();

    // Insert a failure
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "",
            task_id: "2",
            agent: "codex",
            model: None,
            complexity: Some("medium"),
            outcome: "failed",
            duration_seconds: 30.0,
            started_at: &now,
            completed_at: &now,
            attempts: 2,
            files_changed: 0,
            error_type: Some("timeout"),
            input_tokens: None,
            output_tokens: None,
            total_cost_usd: None,
        })
        .await
        .unwrap();

    let summary = store.get_metrics_summary_24h().await.unwrap();
    assert_eq!(summary.tasks_completed_24h, 1);
    assert_eq!(summary.tasks_failed_24h, 1);
    assert_eq!(summary.agent_stats.len(), 2);
}

#[tokio::test]
async fn metrics_summary_excludes_rows_before_cutoff_on_same_calendar_day() {
    let store = TaskStore::open_memory().await.unwrap();

    // `completed_at` is stored as RFC3339 ('T' separator) but SQLite's
    // `datetime('now', '-24 hours')` produces a space-separated string. 'T' (0x54)
    // sorts after ' ' (0x20), so a plain string `>=` comparison used to treat any
    // row sharing the cutoff's calendar day as "within the window" regardless of
    // its actual time-of-day. Midnight of the cutoff's own day is unambiguously
    // >=24h in the past yet shares that day's date with the cutoff, so it
    // reproduces the bug deterministically without depending on wall-clock timing.
    let cutoff_str: String = sqlx::query_scalar("SELECT datetime('now', '-24 hours')")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    let cutoff_date = &cutoff_str[..10];

    let midnight = chrono::NaiveDateTime::parse_from_str(
        &format!("{cutoff_date} 00:00:01"),
        "%Y-%m-%d %H:%M:%S",
    )
    .unwrap()
    .and_utc();

    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "",
            task_id: "old",
            agent: "claude",
            model: None,
            complexity: None,
            outcome: "success",
            duration_seconds: 10.0,
            started_at: &midnight,
            completed_at: &midnight,
            attempts: 1,
            files_changed: 0,
            error_type: None,
            input_tokens: None,
            output_tokens: None,
            total_cost_usd: None,
        })
        .await
        .unwrap();

    let summary = store.get_metrics_summary(24).await.unwrap();
    assert_eq!(
        summary.tasks_completed_24h, 0,
        "a row from midnight of the cutoff's calendar day is >=24h old and must not be counted"
    );
}

#[tokio::test]
async fn cost_summary_returns_correct_periods() {
    let store = TaskStore::open_memory().await.unwrap();
    let summary = store.get_cost_summary().await.unwrap();
    assert_eq!(summary.periods.len(), 3);
    assert_eq!(summary.periods[0].label, "24h");
    assert_eq!(summary.periods[1].label, "7d");
    assert_eq!(summary.periods[2].label, "30d");
}

#[tokio::test]
async fn cost_by_agent_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let groups = store.get_cost_by_agent().await.unwrap();
    assert!(groups.is_empty());
}

#[tokio::test]
async fn cost_by_model_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let groups = store.get_cost_by_model().await.unwrap();
    assert!(groups.is_empty());
}

#[tokio::test]
async fn cost_by_agent_groups_correctly() {
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    for (agent, cost) in &[("claude", 0.10), ("codex", 0.05), ("claude", 0.20)] {
        store
            .insert_task_metric(&InsertTaskMetric {
                repo: "",
                task_id: "1",
                agent,
                model: None,
                complexity: None,
                outcome: "success",
                duration_seconds: 10.0,
                started_at: &now,
                completed_at: &now,
                attempts: 1,
                files_changed: 0,
                error_type: None,
                input_tokens: None,
                output_tokens: None,
                total_cost_usd: Some(*cost),
            })
            .await
            .unwrap();
    }

    let groups = store.get_cost_by_agent().await.unwrap();
    assert_eq!(groups.len(), 2);
    // claude should be first (higher total cost)
    assert_eq!(groups[0].name, "claude");
    assert_eq!(groups[0].task_count, 2);
    assert!((groups[0].total_cost_usd - 0.30).abs() < 0.001);
}

#[tokio::test]
async fn cost_by_model_groups_correctly() {
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    for (model, cost) in &[("sonnet", 0.05), ("opus", 0.15), ("sonnet", 0.10)] {
        store
            .insert_task_metric(&InsertTaskMetric {
                repo: "",
                task_id: "1",
                agent: "claude",
                model: Some(model),
                complexity: None,
                outcome: "success",
                duration_seconds: 10.0,
                started_at: &now,
                completed_at: &now,
                attempts: 1,
                files_changed: 0,
                error_type: None,
                input_tokens: None,
                output_tokens: None,
                total_cost_usd: Some(*cost),
            })
            .await
            .unwrap();
    }

    let groups = store.get_cost_by_model().await.unwrap();
    assert_eq!(groups.len(), 2);
    // sonnet should be first (higher total cost: 0.15)
    assert_eq!(groups[0].name, "sonnet");
    assert_eq!(groups[0].task_count, 2);
    assert!((groups[0].total_cost_usd - 0.15).abs() < 0.001);
    assert_eq!(groups[1].name, "opus");
    assert_eq!(groups[1].task_count, 1);
}

#[tokio::test]
async fn record_rate_limit_returns_id() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .record_rate_limit("claude", "rate", Some("42"))
        .await
        .unwrap();
    assert!(id > 0);
}

#[tokio::test]
async fn rate_limits_counted_in_metrics_summary() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .record_rate_limit("claude", "rate", None)
        .await
        .unwrap();
    store
        .record_rate_limit("codex", "auth", Some("5"))
        .await
        .unwrap();

    let summary = store.get_metrics_summary_24h().await.unwrap();
    assert_eq!(summary.rate_limits_24h, 2);
}

#[tokio::test]
async fn self_improvement_counter_starts_at_zero() {
    let store = TaskStore::open_memory().await.unwrap();
    let count = store.count_self_improvement_issues_7d().await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn self_improvement_counter_increments() {
    let store = TaskStore::open_memory().await.unwrap();
    store.increment_self_improvement_counter().await.unwrap();
    store.increment_self_improvement_counter().await.unwrap();
    let count = store.count_self_improvement_issues_7d().await.unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn slow_tasks_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let slow = store.get_slow_tasks_7d().await.unwrap();
    assert!(slow.is_empty());
}

#[tokio::test]
async fn error_distribution_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let errors = store.get_error_distribution(24 * 7).await.unwrap();
    assert!(errors.is_empty());
}

#[tokio::test]
async fn high_review_cycle_tasks_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let tasks = store.get_high_review_cycle_tasks_7d().await.unwrap();
    assert!(tasks.is_empty());
}

#[tokio::test]
async fn high_review_cycle_tasks_filters_by_threshold() {
    let store = TaskStore::open_memory().await.unwrap();

    // Create tasks with different review_cycles values
    let id_low = store
        .create_internal("owner/repo", "Low cycles", "", "manual", "", None)
        .await
        .unwrap();
    let id_high = store
        .create_internal("owner/repo", "High cycles", "", "manual", "", None)
        .await
        .unwrap();
    let id_boundary = store
        .create_internal("owner/repo", "At boundary", "", "manual", "", None)
        .await
        .unwrap();

    // Set review_cycles: 0, 2, 3
    store.increment(id_low, "review_cycles").await.unwrap();
    // id_low has review_cycles=1, should be excluded

    store.increment(id_high, "review_cycles").await.unwrap();
    store.increment(id_high, "review_cycles").await.unwrap();
    store.increment(id_high, "review_cycles").await.unwrap();
    // id_high has review_cycles=3, should be included

    store.increment(id_boundary, "review_cycles").await.unwrap();
    store.increment(id_boundary, "review_cycles").await.unwrap();
    // id_boundary has review_cycles=2, should be included (>=2)

    let tasks = store.get_high_review_cycle_tasks_7d().await.unwrap();
    assert_eq!(tasks.len(), 2);
    // Should be sorted by review_cycles DESC
    assert_eq!(tasks[0].title, "High cycles");
    assert_eq!(tasks[0].review_cycles, 3);
    assert_eq!(tasks[1].title, "At boundary");
    assert_eq!(tasks[1].review_cycles, 2);
}

#[tokio::test]
async fn high_review_cycle_tasks_excludes_old_tasks() {
    let store = TaskStore::open_memory().await.unwrap();

    let id_old = store
        .create_internal("owner/repo", "Old task", "", "manual", "", None)
        .await
        .unwrap();
    store.increment(id_old, "review_cycles").await.unwrap();
    store.increment(id_old, "review_cycles").await.unwrap();

    // Backdate to 10 days ago (outside 7d window)
    sqlx::query("UPDATE tasks SET updated_at = datetime('now', '-10 days') WHERE id = ?")
        .bind(id_old)
        .execute(&store.pool)
        .await
        .unwrap();

    let id_recent = store
        .create_internal("owner/repo", "Recent task", "", "manual", "", None)
        .await
        .unwrap();
    store.increment(id_recent, "review_cycles").await.unwrap();
    store.increment(id_recent, "review_cycles").await.unwrap();

    let tasks = store.get_high_review_cycle_tasks_7d().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "Recent task");
}

#[tokio::test]
async fn error_distribution_groups_by_type() {
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    for error_type in &["timeout", "timeout", "rate_limit"] {
        store
            .insert_task_metric(&InsertTaskMetric {
                repo: "",
                task_id: "1",
                agent: "claude",
                model: None,
                complexity: None,
                outcome: "failed",
                duration_seconds: 10.0,
                started_at: &now,
                completed_at: &now,
                attempts: 1,
                files_changed: 0,
                error_type: Some(error_type),
                input_tokens: None,
                output_tokens: None,
                total_cost_usd: None,
            })
            .await
            .unwrap();
    }

    let errors = store.get_error_distribution(24 * 7).await.unwrap();
    assert_eq!(errors.len(), 2);
    // timeout should be first (count=2)
    assert_eq!(errors[0].error_type.as_deref(), Some("timeout"));
    assert_eq!(errors[0].count, 2);
}

#[tokio::test]
async fn slow_tasks_returns_sorted_by_duration() {
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    // Insert three metrics with different durations
    for (task_id, duration) in &[("t1", 120.0), ("t2", 300.0), ("t3", 60.0)] {
        store
            .insert_task_metric(&InsertTaskMetric {
                repo: "",
                task_id,
                agent: "claude",
                model: Some("sonnet"),
                complexity: Some("medium"),
                outcome: "success",
                duration_seconds: *duration,
                started_at: &now,
                completed_at: &now,
                attempts: 1,
                files_changed: 1,
                error_type: None,
                input_tokens: None,
                output_tokens: None,
                total_cost_usd: None,
            })
            .await
            .unwrap();
    }

    let slow = store.get_slow_tasks(24 * 7).await.unwrap();
    assert_eq!(slow.len(), 3);
    // Should be sorted descending by duration
    assert_eq!(slow[0].task_id, "t2");
    assert!((slow[0].duration_seconds - 300.0).abs() < 0.01);
    assert_eq!(slow[1].task_id, "t1");
    assert_eq!(slow[2].task_id, "t3");
}

#[tokio::test]
async fn cost_summary_returns_three_periods() {
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "",
            task_id: "cost1",
            agent: "claude",
            model: Some("sonnet"),
            complexity: None,
            outcome: "success",
            duration_seconds: 10.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: Some(1000),
            output_tokens: Some(500),
            total_cost_usd: Some(0.0105),
        })
        .await
        .unwrap();

    let summary = store.get_cost_summary().await.unwrap();
    assert_eq!(summary.periods.len(), 3);
    assert_eq!(summary.periods[0].label, "24h");
    assert_eq!(summary.periods[1].label, "7d");
    assert_eq!(summary.periods[2].label, "30d");
    // All three windows should include our recent metric
    for period in &summary.periods {
        assert_eq!(period.task_count, 1);
        assert!(period.total_cost_usd > 0.0);
    }
}

#[tokio::test]
async fn metrics_summary_by_repo_filters_correctly() {
    let store = TaskStore::open_memory().await.unwrap();

    // Create tasks in different repos
    let id1 = store
        .create_internal("owner/orch", "Task A", "", "test", "", None)
        .await
        .unwrap();
    let id2 = store
        .create_internal("owner/bean", "Task B", "", "test", "", None)
        .await
        .unwrap();

    let now = chrono::Utc::now();
    let ago = now - chrono::Duration::minutes(30);

    // Insert metric for orch
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "owner/orch",
            task_id: &id1.to_string(),
            agent: "claude",
            model: Some("sonnet"),
            complexity: Some("simple"),
            outcome: "success",
            duration_seconds: 60.0,
            started_at: &ago,
            completed_at: &now,
            attempts: 1,
            files_changed: 2,
            error_type: None,
            input_tokens: None,
            output_tokens: None,
            total_cost_usd: None,
        })
        .await
        .unwrap();

    // Insert metric for bean
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "owner/bean",
            task_id: &id2.to_string(),
            agent: "codex",
            model: Some("gpt-5"),
            complexity: Some("medium"),
            outcome: "success",
            duration_seconds: 120.0,
            started_at: &ago,
            completed_at: &now,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: None,
            output_tokens: None,
            total_cost_usd: None,
        })
        .await
        .unwrap();

    // Query per-repo: orch
    let orch_stats = store
        .get_metrics_summary_by_repo("owner/orch", 24)
        .await
        .unwrap();
    assert_eq!(orch_stats.tasks_completed_24h, 1);
    assert_eq!(orch_stats.agent_stats.len(), 1);
    assert_eq!(orch_stats.agent_stats[0].agent, "claude");

    // Query per-repo: bean
    let bean_stats = store
        .get_metrics_summary_by_repo("owner/bean", 24)
        .await
        .unwrap();
    assert_eq!(bean_stats.tasks_completed_24h, 1);
    assert_eq!(bean_stats.agent_stats.len(), 1);
    assert_eq!(bean_stats.agent_stats[0].agent, "codex");

    // Global should show both
    let all_stats = store.get_metrics_summary_24h().await.unwrap();
    assert_eq!(all_stats.tasks_completed_24h, 2);
}

/// Regression test: when a metric's task_id matches both an internal id AND an
/// external_id of a DIFFERENT task in the same repo, the metric must NOT fan out
/// to both rows (which would double-count it).
///
/// Uses raw SQL to inspect base query output and detect fan-out directly.
#[tokio::test]
async fn metrics_summary_by_repo_no_duplication_on_id_collision() {
    let store = TaskStore::open_memory().await.unwrap();
    let now = chrono::Utc::now();

    // Create a task and capture its auto-increment id.
    let id1 = store
        .create_internal("owner/orch", "Task A", "", "test", "", None)
        .await
        .unwrap();

    // Set its external_id to "999" — a sentinel value that won't collide naturally.
    store.update_external_id(id1, "999").await.unwrap();

    // Create a second task. Its auto-increment id is unknown, but we need a
    // numeric-string collision. We'll look for it after creation.
    let id2 = store
        .create_internal("owner/orch", "Task B", "", "test", "", None)
        .await
        .unwrap();

    // If id2 happens to collide with id1's external_id, we've found our collision.
    let id2_str = id2.to_string();
    let collision = id2_str == "999";

    // Insert a metric for task A, referencing it by external_id.
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "",
            task_id: "999",
            agent: "claude",
            model: None,
            complexity: Some("simple"),
            outcome: "success",
            duration_seconds: 30.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: None,
            output_tokens: None,
            total_cost_usd: None,
        })
        .await
        .unwrap();

    // Also insert a metric for task B (non-colliding) so the comparison is meaningful.
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "",
            task_id: &id2_str,
            agent: "claude",
            model: None,
            complexity: Some("medium"),
            outcome: "success",
            duration_seconds: 20.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: None,
            output_tokens: None,
            total_cost_usd: None,
        })
        .await
        .unwrap();

    // Directly inspect the base CTE output to detect fan-out.
    // If the fix works, id2=999 produces 1 row; without the fix, it produces 2 rows
    // (one via id=999 match, one via external_id=999 match).
    let pool = store.pool();
    let base_row_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_metrics m
         LEFT JOIN tasks t ON m.task_id = CAST(t.id AS TEXT)
             OR (m.task_id = t.external_id AND NOT EXISTS (
                 SELECT 1 FROM tasks t2 WHERE t2.id = CAST(m.task_id AS INTEGER) AND t2.repo = t.repo
             ))
         WHERE m.task_id = ?",
    )
    .bind(&id2_str)
    .fetch_one(pool)
    .await
    .unwrap();

    // The metric for task B should map to exactly 1 task row, not fan out.
    assert_eq!(
        base_row_count, 1,
        "metric must not fan out to multiple task rows"
    );

    // Also check the full summary — we should have exactly 2 tasks (not 3 if fan-out occurred).
    let summary = store
        .get_metrics_summary_by_repo("owner/orch", 24)
        .await
        .unwrap();
    if collision {
        // When id2 coincidentally equals "999", the single metric for task A
        // must not be double-counted just because both task A (via external_id)
        // and task B (via id) match the same task_id value.
        assert_eq!(
            summary.tasks_completed_24h, 1,
            "metric must not be double-counted on id collision"
        );
    } else {
        // No collision occurred (expected in most test runs).
        // Verify the non-colliding path works: 2 tasks → 2 completed.
        assert_eq!(summary.tasks_completed_24h, 2);
    }
}

#[tokio::test]
async fn metrics_summary_rate_limits_are_repo_scoped() {
    let store = TaskStore::open_memory().await.unwrap();

    let id1 = store
        .create_internal("owner/orch", "Task A", "", "test", "", None)
        .await
        .unwrap();
    let id2 = store
        .create_internal("owner/bean", "Task B", "", "test", "", None)
        .await
        .unwrap();

    store
        .record_rate_limit("claude", "rate_limit", Some(&id1.to_string()))
        .await
        .unwrap();
    store
        .record_rate_limit("claude", "rate_limit", Some(&id1.to_string()))
        .await
        .unwrap();
    store
        .record_rate_limit("codex", "rate_limit", Some(&id2.to_string()))
        .await
        .unwrap();

    let orch_summary = store
        .get_metrics_summary_by_repo("owner/orch", 24)
        .await
        .unwrap();
    assert_eq!(
        orch_summary.rate_limits_24h, 2,
        "orch should only see its own 2 rate limits"
    );

    let bean_summary = store
        .get_metrics_summary_by_repo("owner/bean", 24)
        .await
        .unwrap();
    assert_eq!(
        bean_summary.rate_limits_24h, 1,
        "bean should only see its own 1 rate limit"
    );

    let all_summary = store.get_metrics_summary_24h().await.unwrap();
    assert_eq!(
        all_summary.rate_limits_24h, 3,
        "global should see all 3 rate limits"
    );
}

#[tokio::test]
async fn metrics_summary_rate_limits_no_duplication_on_id_external_id_collision() {
    let store = TaskStore::open_memory().await.unwrap();

    let id1 = store
        .create_internal("owner/orch", "Task A", "", "test", "", None)
        .await
        .unwrap();
    let id2 = store
        .create_internal("owner/orch", "Task B", "", "test", "", None)
        .await
        .unwrap();

    // Force an id/external_id collision across two different tasks.
    store
        .update_external_id(id2, &id1.to_string())
        .await
        .unwrap();

    // This event references task A by numeric id; without the NOT EXISTS guard
    // it would fan out to task A (id match) and task B (external_id match).
    store
        .record_rate_limit("claude", "rate_limit", Some(&id1.to_string()))
        .await
        .unwrap();

    let orch_summary = store
        .get_metrics_summary_by_repo("owner/orch", 24)
        .await
        .unwrap();
    assert_eq!(
        orch_summary.rate_limits_24h, 1,
        "rate limit event must not be double-counted on id/external_id collision"
    );
}

#[tokio::test]
async fn metrics_summary_rate_limits_cross_repo_numeric_collision() {
    let store = TaskStore::open_memory().await.unwrap();

    // Create an internal task in repo B which will get id = 1
    let id_b = store
        .create_internal("owner/bean", "Task B", "", "test", "", None)
        .await
        .unwrap();

    // Upsert an external task in repo A with external_id equal to id_b (numeric string)
    let _id_a = store
        .upsert_external(&UpsertExternal {
            repo: "owner/orch",
            ext_id: &id_b.to_string(),
            title: "External A",
            body: "",
            author: "user",
            url: "https://github.com/owner/orch/issues/1",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();

    // Record a rate limit event that references task_id = "<id_b>" — it should
    // resolve to the external task in owner/orch, not be suppressed by the
    // existence of an unrelated internal task with the same numeric id in owner/bean.
    store
        .record_rate_limit("claude", "rate_limit", Some(&id_b.to_string()))
        .await
        .unwrap();

    let orch_summary = store
        .get_metrics_summary_by_repo("owner/orch", 24)
        .await
        .unwrap();

    assert_eq!(
        orch_summary.rate_limits_24h, 1,
        "orch should see its rate limit event even when an unrelated repo has the same numeric id"
    );

    let bean_summary = store
        .get_metrics_summary_by_repo("owner/bean", 24)
        .await
        .unwrap();

    // bean should not see the orch event (it belongs to orch)
    assert_eq!(
        bean_summary.rate_limits_24h, 0,
        "bean must not see orch's rate limit event"
    );
}

#[tokio::test]
async fn cost_summary_by_repo_isolates_per_repo() {
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    // Insert a metric for owner/orch
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "owner/orch",
            task_id: "orch-1",
            agent: "claude",
            model: Some("sonnet"),
            complexity: None,
            outcome: "success",
            duration_seconds: 10.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: Some(1000),
            output_tokens: Some(500),
            total_cost_usd: Some(0.0105),
        })
        .await
        .unwrap();

    // Insert a metric for owner/bean
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "owner/bean",
            task_id: "bean-1",
            agent: "codex",
            model: Some("gpt-4"),
            complexity: None,
            outcome: "success",
            duration_seconds: 20.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 2,
            error_type: None,
            input_tokens: Some(2000),
            output_tokens: Some(1000),
            total_cost_usd: Some(0.021),
        })
        .await
        .unwrap();

    // owner/orch should only see its own cost
    let orch_cost = store
        .get_cost_summary_by_repo("owner/orch", 24)
        .await
        .unwrap();
    assert_eq!(orch_cost.periods.len(), 1);
    assert_eq!(orch_cost.periods[0].label, "24h");
    assert_eq!(orch_cost.periods[0].task_count, 1);
    assert!((orch_cost.periods[0].total_cost_usd - 0.0105).abs() < 1e-6);

    // owner/bean should only see its own cost
    let bean_cost = store
        .get_cost_summary_by_repo("owner/bean", 24)
        .await
        .unwrap();
    assert_eq!(bean_cost.periods[0].task_count, 1);
    assert!((bean_cost.periods[0].total_cost_usd - 0.021).abs() < 1e-6);

    // Unknown repo returns zeros
    let unknown = store
        .get_cost_summary_by_repo("unknown/repo", 24)
        .await
        .unwrap();
    assert_eq!(unknown.periods[0].task_count, 0);
    assert_eq!(unknown.periods[0].total_cost_usd, 0.0);
}

#[tokio::test]
async fn cost_summary_by_repo_falls_back_to_tasks_join() {
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    // Create a task with a known repo so the join can resolve it
    let task_id = store
        .create_internal("owner/orch", "task", "body", "manual", "", None)
        .await
        .unwrap();

    // Insert metric with empty repo — repo will be resolved via tasks join
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "",
            task_id: &task_id.to_string(),
            agent: "claude",
            model: None,
            complexity: None,
            outcome: "success",
            duration_seconds: 5.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 0,
            error_type: None,
            input_tokens: Some(500),
            output_tokens: Some(200),
            total_cost_usd: Some(0.0045),
        })
        .await
        .unwrap();

    let cost = store
        .get_cost_summary_by_repo("owner/orch", 24)
        .await
        .unwrap();
    assert_eq!(cost.periods[0].task_count, 1);
    assert!((cost.periods[0].total_cost_usd - 0.0045).abs() < 1e-6);
}

#[tokio::test]
async fn cost_summary_by_repo_uses_requested_window_label() {
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();
    let eight_days_ago = now - TimeDelta::days(8);

    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "owner/orch",
            task_id: "recent",
            agent: "claude",
            model: Some("sonnet"),
            complexity: None,
            outcome: "success",
            duration_seconds: 10.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_cost_usd: Some(0.003),
        })
        .await
        .unwrap();

    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "owner/orch",
            task_id: "older",
            agent: "claude",
            model: Some("sonnet"),
            complexity: None,
            outcome: "success",
            duration_seconds: 12.0,
            started_at: &eight_days_ago,
            completed_at: &eight_days_ago,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: Some(200),
            output_tokens: Some(100),
            total_cost_usd: Some(0.006),
        })
        .await
        .unwrap();

    let summary_7d = store
        .get_cost_summary_by_repo("owner/orch", 24 * 7)
        .await
        .unwrap();
    assert_eq!(summary_7d.periods[0].label, "7d");
    assert_eq!(summary_7d.periods[0].task_count, 1);
    assert!((summary_7d.periods[0].total_cost_usd - 0.003).abs() < 1e-6);

    let summary_30d = store
        .get_cost_summary_by_repo("owner/orch", 24 * 30)
        .await
        .unwrap();
    assert_eq!(summary_30d.periods[0].label, "30d");
    assert_eq!(summary_30d.periods[0].task_count, 2);
    assert!((summary_30d.periods[0].total_cost_usd - 0.009).abs() < 1e-6);
}

#[test]
fn parse_since_duration_days() {
    let ts = parse_since_duration("7d").unwrap();
    // The timestamp must be a well-formed RFC3339 string (e.g. 2026-03-18T09:56:02Z).
    assert_eq!(ts.len(), 20, "unexpected length: {ts:?}");
    // Must be in RFC3339 format: starts with year, contains 'T', ends with 'Z'.
    assert!(
        ts.starts_with("20") && ts.contains('T') && ts.ends_with('Z'),
        "expected RFC3339 format: {ts:?}"
    );
    // Must be in the past (less than now).
    let now_ts = parse_since_duration("0d");
    assert!(now_ts.is_err(), "0d should be rejected");
}

#[test]
fn parse_since_duration_hours() {
    let ts = parse_since_duration("24h").unwrap();
    assert_eq!(ts.len(), 20);
}

#[test]
fn parse_since_duration_minutes() {
    let ts = parse_since_duration("30m").unwrap();
    assert_eq!(ts.len(), 20);
}

#[test]
fn parse_since_duration_invalid() {
    assert!(parse_since_duration("abc").is_err());
    assert!(parse_since_duration("7").is_err());
    assert!(parse_since_duration("").is_err());
    assert!(parse_since_duration("0h").is_err());
}

#[tokio::test]
async fn recent_rate_limit_counts_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let counts = store.recent_rate_limit_counts(6).await.unwrap();
    assert!(counts.is_empty());
}

#[tokio::test]
async fn recent_rate_limit_counts_groups_by_agent() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .record_rate_limit("claude", "rate", None)
        .await
        .unwrap();
    store
        .record_rate_limit("claude", "error", None)
        .await
        .unwrap();
    store
        .record_rate_limit("codex", "rate", None)
        .await
        .unwrap();

    let counts = store.recent_rate_limit_counts(6).await.unwrap();
    assert_eq!(counts.get("claude"), Some(&1));
    assert_eq!(counts.get("codex"), Some(&1));
    assert_eq!(counts.get("opencode"), None);
}

#[tokio::test]
async fn recent_rate_limit_counts_skips_rows_before_cleared_at() {
    let store = TaskStore::open_memory().await.unwrap();
    let past = (chrono::Utc::now() - chrono::Duration::seconds(30))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let future = (chrono::Utc::now() + chrono::Duration::seconds(30))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    for _ in 0..3 {
        store
            .record_rate_limit_at("claude", "rate", None, &past)
            .await
            .unwrap();
    }
    store
        .kv_set(
            "rate_limits_cleared_at:claude",
            &chrono::Utc::now().timestamp().to_string(),
        )
        .await
        .unwrap();
    store
        .record_rate_limit_at("claude", "rate", None, &future)
        .await
        .unwrap();
    store
        .record_rate_limit_at("codex", "rate", None, &past)
        .await
        .unwrap();

    let counts = store.recent_rate_limit_counts(6).await.unwrap();
    assert_eq!(counts.get("claude"), Some(&1));
    assert_eq!(counts.get("codex"), Some(&1));
}

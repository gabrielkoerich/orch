use super::super::*;
use crate::store::control::parse_since_duration;

#[tokio::test]
async fn subscribe_and_list_channel_subscriptions() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/orch", None)
        .await
        .unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/bean", None)
        .await
        .unwrap();
    let subs = store
        .list_channel_subscriptions("telegram", "42")
        .await
        .unwrap();
    assert_eq!(subs.len(), 2);
    assert!(subs.contains(&"owner/orch".to_string()));
    assert!(subs.contains(&"owner/bean".to_string()));
}

#[tokio::test]
async fn unsubscribe_channel() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/orch", None)
        .await
        .unwrap();
    store
        .unsubscribe_channel("telegram", "42", "owner/orch")
        .await
        .unwrap();
    let subs = store
        .list_channel_subscriptions("telegram", "42")
        .await
        .unwrap();
    assert_eq!(subs.len(), 0);
}

#[tokio::test]
async fn subscribe_idempotent() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/orch", None)
        .await
        .unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/orch", None)
        .await
        .unwrap(); // duplicate
    let subs = store
        .list_channel_subscriptions("telegram", "42")
        .await
        .unwrap();
    assert_eq!(subs.len(), 1);
}

#[tokio::test]
async fn list_subscribers_for_repo() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/orch", None)
        .await
        .unwrap();
    store
        .subscribe_channel("discord", "99", "owner/orch", None)
        .await
        .unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/bean", None)
        .await
        .unwrap();
    let subs = store.list_subscribers_for_repo("owner/orch").await.unwrap();
    assert_eq!(subs.len(), 2);
}

#[tokio::test]
async fn subscription_round_trip_with_multiple_channels() {
    let store = TaskStore::open_memory().await.unwrap();

    // Subscribe telegram and discord to orch
    store
        .subscribe_channel("telegram", "42", "owner/orch", None)
        .await
        .unwrap();
    store
        .subscribe_channel("discord", "1111", "owner/orch", None)
        .await
        .unwrap();

    // Subscribe telegram to bean too
    store
        .subscribe_channel("telegram", "42", "owner/bean", None)
        .await
        .unwrap();

    // Check subscribers for orch
    let orch_subs = store.list_subscribers_for_repo("owner/orch").await.unwrap();
    assert_eq!(orch_subs.len(), 2);

    // Check what telegram:42 is subscribed to
    let tg_subs = store
        .list_channel_subscriptions("telegram", "42")
        .await
        .unwrap();
    assert_eq!(tg_subs.len(), 2);
    assert!(tg_subs.contains(&"owner/orch".to_string()));
    assert!(tg_subs.contains(&"owner/bean".to_string()));

    // Unsubscribe orch from telegram
    store
        .unsubscribe_channel("telegram", "42", "owner/orch")
        .await
        .unwrap();

    let tg_subs = store
        .list_channel_subscriptions("telegram", "42")
        .await
        .unwrap();
    assert_eq!(tg_subs.len(), 1);
    assert_eq!(tg_subs[0], "owner/bean");

    // orch should now only have discord subscriber
    let orch_subs = store.list_subscribers_for_repo("owner/orch").await.unwrap();
    assert_eq!(orch_subs.len(), 1);
    assert_eq!(
        orch_subs[0],
        ("discord".to_string(), "1111".to_string(), String::new())
    );
}

#[tokio::test]
async fn subscribe_with_topic_id_preserved() {
    let store = TaskStore::open_memory().await.unwrap();

    // Subscribe to orch with a topic_id (e.g., Telegram forum topic)
    store
        .subscribe_channel("telegram", "42", "owner/orch", Some("topic-123"))
        .await
        .unwrap();

    // Subscribe to bean without a topic (root thread)
    store
        .subscribe_channel("telegram", "42", "owner/bean", None)
        .await
        .unwrap();

    // List subscribers for orch - should include topic_id
    let orch_subs = store.list_subscribers_for_repo("owner/orch").await.unwrap();
    assert_eq!(orch_subs.len(), 1);
    assert_eq!(
        orch_subs[0],
        (
            "telegram".to_string(),
            "42".to_string(),
            "topic-123".to_string()
        )
    );

    // List subscribers for bean - should have empty topic_id
    let bean_subs = store.list_subscribers_for_repo("owner/bean").await.unwrap();
    assert_eq!(bean_subs.len(), 1);
    assert_eq!(
        bean_subs[0],
        ("telegram".to_string(), "42".to_string(), String::new())
    );
}

#[tokio::test]
async fn control_insert_and_list() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .insert_control_message(
            "default",
            "user",
            "cli",
            None,
            "what's running?",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    store
        .insert_control_message(
            "default",
            "assistant",
            "cli",
            None,
            "3 tasks active",
            Some("listed tasks"),
            Some("sonnet"),
            Some("claude"),
            Some(300),
            Some(200),
            Some(500),
            Some(0.01),
        )
        .await
        .unwrap();

    let messages = store
        .list_control_messages("default", None, 10)
        .await
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[1].summary.as_deref(), Some("listed tasks"));
}

#[tokio::test]
async fn control_search_messages() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .insert_control_message(
            "default",
            "user",
            "cli",
            None,
            "check bean auth issue",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    store
        .insert_control_message(
            "default",
            "user",
            "cli",
            None,
            "unblock trading tasks",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let results = store
        .search_control_messages("default", "bean", None, 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("bean"));
}

#[tokio::test]
async fn control_recent_summaries() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .insert_control_message(
            "default",
            "assistant",
            "cli",
            None,
            "long response",
            Some("did X"),
            Some("sonnet"),
            Some("claude"),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    store
        .insert_control_message(
            "default",
            "assistant",
            "cli",
            None,
            "another response",
            Some("did Y"),
            Some("sonnet"),
            Some("claude"),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let summaries = store.control_recent_summaries("default", 5).await.unwrap();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0], "did X");
    assert_eq!(summaries[1], "did Y");
}

#[tokio::test]
async fn control_recent_summaries_propagates_decode_errors() {
    // Regression: rows with non-string summary (BLOB) must not be silently
    // defaulted to empty string — the error must propagate.
    let store = TaskStore::open_memory().await.unwrap();

    // Insert a valid summary row first.
    store
        .insert_control_message(
            "default",
            "assistant",
            "cli",
            None,
            "valid",
            Some("summary that is a real string"),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    // Insert a row with a BLOB value in the summary column (not valid UTF-8).
    // SQLite stores this as BLOB; reading it as String via try_get fails.
    sqlx::query(
        "INSERT INTO control_messages
         (session_id, role, channel, content, summary, created_at)
         VALUES ('default', 'assistant', 'cli', 'msg', X'01020304', datetime('now'))",
    )
    .execute(store.pool())
    .await
    .unwrap();

    // The decode error on the BLOB row must propagate, NOT produce an
    // empty-string entry or silently succeed.
    let result = store.control_recent_summaries("default", 10).await;
    assert!(
        result.is_err(),
        "expected decode error for BLOB summary row, got Ok: {result:?}"
    );
}

#[tokio::test]
async fn control_sessions_are_isolated() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .insert_control_message(
            "session-a",
            "user",
            "cli",
            None,
            "message in A",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    store
        .insert_control_message(
            "session-b",
            "user",
            "cli",
            None,
            "message in B",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let a_msgs = store
        .list_control_messages("session-a", None, 10)
        .await
        .unwrap();
    let b_msgs = store
        .list_control_messages("session-b", None, 10)
        .await
        .unwrap();
    assert_eq!(a_msgs.len(), 1);
    assert_eq!(b_msgs.len(), 1);
    assert!(a_msgs[0].content.contains("in A"));
    assert!(b_msgs[0].content.contains("in B"));
}

#[tokio::test]
async fn control_session_cost_summary_aggregates_tokens_cost_and_model_breakdown() {
    let store = TaskStore::open_memory().await.unwrap();

    store
        .insert_control_message(
            "default", "user", "cli", None, "hello", None, None, None, None, None, None, None,
        )
        .await
        .unwrap();

    store
        .insert_control_message(
            "default",
            "assistant",
            "cli",
            None,
            "hi",
            Some("responded"),
            Some("sonnet"),
            Some("claude"),
            Some(100),
            Some(40),
            Some(140),
            Some(0.010),
        )
        .await
        .unwrap();

    store
        .insert_control_message(
            "default",
            "assistant",
            "cli",
            None,
            "done",
            Some("completed"),
            Some("sonnet"),
            Some("claude"),
            Some(50),
            Some(25),
            Some(75),
            Some(0.005),
        )
        .await
        .unwrap();

    let summary = store.get_session_cost_summary("default").await.unwrap();
    assert_eq!(summary.total_messages, 3);
    assert_eq!(summary.assistant_messages, 2);
    assert_eq!(summary.total_input_tokens, 150);
    assert_eq!(summary.total_output_tokens, 65);
    assert_eq!(summary.total_tokens, 215);
    assert!((summary.total_cost_usd - 0.015).abs() < 1e-9);
    assert_eq!(summary.primary_model.as_deref(), Some("sonnet"));
    assert_eq!(summary.primary_agent.as_deref(), Some("claude"));
    assert_eq!(summary.by_model.len(), 1);
    assert_eq!(summary.by_model[0].0, "sonnet");
    assert_eq!(summary.by_model[0].1, 2);
    assert_eq!(summary.by_model[0].2, 215);
}

#[tokio::test]
async fn control_list_messages_since_filters_old() {
    let store = TaskStore::open_memory().await.unwrap();

    // Insert a message with a timestamp far in the past.
    sqlx::query(
        "INSERT INTO control_messages
         (session_id, role, channel, content, created_at)
         VALUES ('default', 'user', 'cli', 'old message', '2020-01-01 00:00:00')",
    )
    .execute(store.pool())
    .await
    .unwrap();

    // Insert a recent message using the normal API (created_at defaults to NOW).
    store
        .insert_control_message(
            "default",
            "user",
            "cli",
            None,
            "recent message",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    // Without a since filter: both messages returned.
    let all = store
        .list_control_messages("default", None, 10)
        .await
        .unwrap();
    assert_eq!(all.len(), 2, "expected 2 messages without filter");

    // With a since filter (7 days ago): only the recent one should appear.
    let since_ts = parse_since_duration("7d").unwrap();
    let filtered = store
        .list_control_messages("default", Some(&since_ts), 10)
        .await
        .unwrap();
    assert_eq!(
        filtered.len(),
        1,
        "expected 1 message after filtering, got: {filtered:?}"
    );
    assert!(filtered[0].content.contains("recent"));
}

#[tokio::test]
async fn control_search_messages_since_filters_old() {
    let store = TaskStore::open_memory().await.unwrap();

    // Insert an old matching message.
    sqlx::query(
        "INSERT INTO control_messages
         (session_id, role, channel, content, created_at)
         VALUES ('default', 'user', 'cli', 'bean auth old', '2020-01-01 00:00:00')",
    )
    .execute(store.pool())
    .await
    .unwrap();

    // Insert a recent matching message.
    store
        .insert_control_message(
            "default",
            "user",
            "cli",
            None,
            "bean auth recent",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    // Insert a recent non-matching message.
    store
        .insert_control_message(
            "default",
            "user",
            "cli",
            None,
            "unrelated recent",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    // Search without since: both bean messages returned.
    let all_bean = store
        .search_control_messages("default", "bean", None, 10)
        .await
        .unwrap();
    assert_eq!(all_bean.len(), 2, "expected 2 bean matches without filter");

    // Search with since: only the recent bean message returned.
    let since_ts = parse_since_duration("7d").unwrap();
    let recent_bean = store
        .search_control_messages("default", "bean", Some(&since_ts), 10)
        .await
        .unwrap();
    assert_eq!(
        recent_bean.len(),
        1,
        "expected 1 bean match after since filter, got: {recent_bean:?}"
    );
    assert!(recent_bean[0].content.contains("recent"));
}

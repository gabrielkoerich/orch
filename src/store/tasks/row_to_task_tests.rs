use super::*;
use crate::store;
use crate::store::TaskStatus;

async fn store_with_task() -> (store::TaskStore, i64) {
    let store = store::TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test task".to_string(),
            body: "Test body".to_string(),
            source: "cron".to_string(),
            source_id: "test-1".to_string(),
            author: "tester".to_string(),
            url: "https://example.com/1".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    (store, id)
}

// Each test selects all previously-required columns so that exactly the
// column under test is absent and triggers the Err path.
macro_rules! missing_column_test {
    ($name:ident, $col:literal, $select:literal) => {
        #[tokio::test]
        async fn $name() {
            let (store, id) = store_with_task().await;
            let sql = format!("{} WHERE id = {id}", $select);
            let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .fetch_one(store.pool())
                .await
                .unwrap();
            let err = TaskStore::row_to_task(&row).unwrap_err();
            assert!(
                err.to_string().contains($col),
                "expected '{}' in error message, got: {err}",
                $col
            );
        }
    };
}

missing_column_test!(
    row_to_task_fails_on_missing_repo,
    "repo",
    "SELECT id, status FROM tasks"
);
missing_column_test!(
    row_to_task_fails_on_missing_origin,
    "origin",
    "SELECT id, repo, status FROM tasks"
);
missing_column_test!(
    row_to_task_fails_on_missing_title,
    "title",
    "SELECT id, repo, origin, status FROM tasks"
);
missing_column_test!(
    row_to_task_fails_on_missing_body,
    "body",
    "SELECT id, repo, origin, title, status FROM tasks"
);
missing_column_test!(
    row_to_task_fails_on_missing_source,
    "source",
    "SELECT id, repo, origin, title, body, status FROM tasks"
);
missing_column_test!(
    row_to_task_fails_on_missing_source_id,
    "source_id",
    "SELECT id, repo, origin, title, body, source, status FROM tasks"
);
missing_column_test!(
    row_to_task_fails_on_missing_author,
    "author",
    "SELECT id, repo, origin, title, body, source, source_id, status FROM tasks"
);
missing_column_test!(
    row_to_task_fails_on_missing_url,
    "url",
    "SELECT id, repo, origin, title, body, source, source_id, author, status FROM tasks"
);

// Decode-error test: inject a BLOB with invalid UTF-8 bytes for the `repo`
// column. SQLite's type coercion converts NULL and integers to strings, so
// they don't produce a decode error. A raw BLOB that is not valid UTF-8
// (X'DEADBEEF') does cause `try_get::<String>` to fail with a ColumnDecode
// error, exercising the decode-error-propagation path that is distinct from
// the missing-column (ColumnNotFound) path tested above.
#[tokio::test]
async fn row_to_task_fails_on_decode_error_invalid_utf8_repo() {
    let (store, id) = store_with_task().await;
    // X'DEADBEEF' is a 4-byte BLOB; bytes 0xDE 0xAD 0xBE 0xEF are not
    // valid UTF-8, so try_get::<String>("repo") will fail with a decode error.
    let sql = format!(
        "SELECT id, X'DEADBEEF' as repo, origin, title, body, status, \
             source, source_id, author, url FROM tasks WHERE id = {id}"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .fetch_one(store.pool())
        .await
        .unwrap();
    let err = TaskStore::row_to_task(&row).unwrap_err();
    assert!(
        err.to_string().contains("repo"),
        "expected 'repo' in error message, got: {err}"
    );
}

// ── distinct_repos decode-error tests ─────────────────────────────────

#[tokio::test]
async fn distinct_repos_fails_on_decode_error() {
    let store = store::TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test task".to_string(),
            body: "Test body".to_string(),
            source: "cron".to_string(),
            source_id: "test-1".to_string(),
            author: "tester".to_string(),
            url: "https://example.com/1".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Inject an invalid UTF-8 BLOB as repo — try_get::<String> will fail.
    let sql = format!("UPDATE tasks SET repo = X'DEADBEEF' WHERE id = {id}");
    sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .execute(store.pool())
        .await
        .unwrap();

    let err = store.distinct_repos().await.unwrap_err();
    assert!(
        err.to_string().contains("distinct_repos") && err.to_string().contains("repo"),
        "expected 'distinct_repos' and 'repo' in error message, got: {err}"
    );
}

// ── row_to_run decode-error tests ────────────────────────────────────────

async fn store_with_run() -> (store::TaskStore, i64, i64) {
    let store = store::TaskStore::open_memory().await.unwrap();
    let task_id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test task".to_string(),
            body: "Test body".to_string(),
            source: "cron".to_string(),
            source_id: "test-1".to_string(),
            author: "tester".to_string(),
            url: "https://example.com/1".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    let run_id = store
        .start_run(&crate::store::StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude -p ...",
            prompt: "system prompt",
        })
        .await
        .unwrap();
    (store, task_id, run_id)
}

#[tokio::test]
async fn start_run_upsert_clears_stale_completion_fields() {
    let (store, task_id, run_id) = store_with_run().await;
    store
        .complete_run(&CompleteRun {
            run_id,
            exit_code: Some(0),
            stdout: "",
            stderr: "",
            parsed: "",
            outcome: "success",
            error: "boom",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    // Re-dispatch the same (task_id, attempt, run_type) key.
    let same_run_id = store
        .start_run(&crate::store::StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude -p ...",
            prompt: "system prompt",
        })
        .await
        .unwrap();
    assert_eq!(run_id, same_run_id, "upsert must reuse the same row");

    let row =
        sqlx::query("SELECT outcome, completed_at, exit_code, error FROM task_runs WHERE id = ?")
            .bind(run_id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    let outcome: Option<String> = row.try_get("outcome").unwrap();
    let completed_at: Option<String> = row.try_get("completed_at").unwrap();
    let exit_code: Option<i64> = row.try_get("exit_code").unwrap();
    let error: Option<String> = row.try_get("error").unwrap();
    assert_eq!(outcome, None);
    assert_eq!(
        completed_at, None,
        "stale completion timestamp must be cleared"
    );
    assert_eq!(exit_code, None);
    assert_eq!(error, None);
}

// NOTE: Unlike the task table (where all required columns are TEXT and NULL
// propagates a ColumnNotFound error), the task_runs table has INTEGER and REAL
// columns (id, task_id, attempt, started_at). SQLite coerces NULL/INTEGER to
// String when try_get::<String> is called, so we cannot use the missing-column
// pattern here. Instead, we use BLOB injection to trigger actual decode errors
// that propagate through map_err.
macro_rules! run_blob_decode_test {
    ($name:ident, $col:literal, $select:literal) => {
        #[tokio::test]
        async fn $name() {
            let (store, _task_id, run_id) = store_with_run().await;
            let sql = format!($select, run_id = run_id);
            let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .fetch_one(store.pool())
                .await
                .unwrap();
            let err = TaskStore::row_to_run(&row).unwrap_err();
            assert!(
                err.to_string().contains($col),
                "expected '{0}' in error message, got: {err}",
                $col
            );
        }
    };
}

run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_id,
    "id",
    "SELECT X'DEADBEEF' as id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_task_id,
    "task_id",
    "SELECT id, X'DEADBEEF' as task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_started_at,
    "started_at",
    "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, X'DEADBEEF' as started_at, completed_at FROM task_runs WHERE id = {run_id}"
);

// ── row_to_activity decode-error tests ───────────────────────────────────

async fn store_with_activity() -> (store::TaskStore, i64, i64) {
    let store = store::TaskStore::open_memory().await.unwrap();
    let task_id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test task".to_string(),
            body: "Test body".to_string(),
            source: "cron".to_string(),
            source_id: "test-1".to_string(),
            author: "tester".to_string(),
            url: "https://example.com/1".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store
        .append_activity(task_id, "created", None, Some("new"), None, None, None)
        .await
        .unwrap();
    (store, task_id, 1)
}

// Each test drops exactly one column from the full TASK_ACTIVITY_COLS so that
// the first missing-column try_get (id) propagates the error.
#[tokio::test]
async fn row_to_activity_fails_on_decode_error_invalid_utf8_id() {
    let (store, _task_id, activity_id) = store_with_activity().await;
    let sql = format!(
        "SELECT X'DEADBEEF' as id, task_id, timestamp, event_type, from_status, to_status, agent, model, details FROM task_activity WHERE id = {activity_id}"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .fetch_one(store.pool())
        .await
        .unwrap();
    let err = TaskStore::row_to_activity(&row).unwrap_err();
    assert!(
        err.to_string().contains("id"),
        "expected 'id' in error message, got: {err}"
    );
}

#[tokio::test]
async fn row_to_activity_fails_on_decode_error_invalid_utf8_task_id() {
    let (store, _task_id, activity_id) = store_with_activity().await;
    let sql = format!(
        "SELECT id, X'DEADBEEF' as task_id, timestamp, event_type, from_status, to_status, agent, model, details FROM task_activity WHERE id = {activity_id}"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .fetch_one(store.pool())
        .await
        .unwrap();
    let err = TaskStore::row_to_activity(&row).unwrap_err();
    assert!(
        err.to_string().contains("task_id"),
        "expected 'task_id' in error message, got: {err}"
    );
}

#[tokio::test]
async fn row_to_activity_fails_on_decode_error_invalid_utf8_timestamp() {
    let (store, _task_id, activity_id) = store_with_activity().await;
    let sql = format!(
        "SELECT id, task_id, X'DEADBEEF' as timestamp, event_type, from_status, to_status, agent, model, details FROM task_activity WHERE id = {activity_id}"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .fetch_one(store.pool())
        .await
        .unwrap();
    let err = TaskStore::row_to_activity(&row).unwrap_err();
    assert!(
        err.to_string().contains("timestamp"),
        "expected 'timestamp' in error message, got: {err}"
    );
}

#[tokio::test]
async fn row_to_activity_fails_on_decode_error_invalid_utf8_event_type() {
    let (store, _task_id, activity_id) = store_with_activity().await;
    let sql = format!(
        "SELECT id, task_id, timestamp, X'DEADBEEF' as event_type, from_status, to_status, agent, model, details FROM task_activity WHERE id = {activity_id}"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .fetch_one(store.pool())
        .await
        .unwrap();
    let err = TaskStore::row_to_activity(&row).unwrap_err();
    assert!(
        err.to_string().contains("event_type"),
        "expected 'event_type' in error message, got: {err}"
    );
}

#[tokio::test]
async fn row_to_activity_fails_on_corrupt_details_json() {
    let (store, task_id, _activity_id) = store_with_activity().await;
    // Insert a row with invalid JSON in details
    sqlx::query("INSERT INTO task_activity (task_id, event_type, details) VALUES (?, ?, ?)")
        .bind(task_id)
        .bind("test")
        .bind("not valid json {{{")
        .execute(store.pool())
        .await
        .unwrap();
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {TASK_ACTIVITY_COLS} FROM task_activity WHERE task_id = {task_id} ORDER BY id DESC LIMIT 1"
    )))
    .fetch_one(store.pool())
    .await
    .unwrap();
    let err = TaskStore::row_to_activity(&row).unwrap_err();
    assert!(
        err.to_string().contains("details")
            || err.to_string().contains("json")
            || err.to_string().contains("expected"),
        "expected 'details', 'json', or 'expected' in error message, got: {err}"
    );
}

// ── row_to_run: remaining fields with map_err ──────────────────────────────

run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_model,
    "model",
    "SELECT id, task_id, attempt, run_type, agent, X'DeadbEEF' as model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_command,
    "command",
    "SELECT id, task_id, attempt, run_type, agent, model, X'DeadbEEF' as command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_prompt,
    "prompt",
    "SELECT id, task_id, attempt, run_type, agent, model, command, X'DeadbEEF' as prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_env_vars,
    "env_vars",
    "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, X'DeadbEEF' as env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_stdout,
    "stdout",
    "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, X'DeadbEEF' as stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_stderr,
    "stderr",
    "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, X'DeadbEEF' as stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_parsed_response,
    "parsed_response",
    "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, X'DeadbEEF' as parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_outcome,
    "outcome",
    "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, X'DeadbEEF' as outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_error,
    "error",
    "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, X'DeadbEEF' as error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_input_tokens,
    "input_tokens",
    "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, X'DeadbEEF' as input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_output_tokens,
    "output_tokens",
    "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, X'DeadbEEF' as output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_total_cost_usd,
    "total_cost_usd",
    "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, X'DeadbEEF' as total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_duration_secs,
    "duration_secs",
    "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, X'DeadbEEF' as duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
);
run_blob_decode_test!(
    row_to_run_fails_on_decode_error_invalid_utf8_exit_code,
    "exit_code",
    "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, X'DeadbEEF' as exit_code, completed_at FROM task_runs WHERE id = {run_id}"
);

// ── row_to_activity: nullable columns with map_err ────────────────────────

// from_status, to_status, agent, model are nullable (Option<String>) but their
// decode errors should still propagate via map_err, not be masked as None.
// Each test injects BLOB into exactly one column and NULL into the rest so
// the error always references the intended column (the first failing map_err).
#[tokio::test]
async fn row_to_activity_fails_on_decode_error_invalid_utf8_from_status() {
    let (store, _task_id, activity_id) = store_with_activity().await;
    let sql = format!(
        "SELECT id, task_id, timestamp, event_type, \
             X'DeadbEEF' as from_status, NULL as to_status, \
             NULL as agent, NULL as model, details \
             FROM task_activity WHERE id = {activity_id}"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .fetch_one(store.pool())
        .await
        .unwrap();
    let err = TaskStore::row_to_activity(&row).unwrap_err();
    assert!(
        err.to_string().contains("from_status"),
        "expected 'from_status' in error message, got: {err}"
    );
}

#[tokio::test]
async fn row_to_activity_fails_on_decode_error_invalid_utf8_to_status() {
    let (store, _task_id, activity_id) = store_with_activity().await;
    let sql = format!(
        "SELECT id, task_id, timestamp, event_type, \
             NULL as from_status, X'DeadbEEF' as to_status, \
             NULL as agent, NULL as model, details \
             FROM task_activity WHERE id = {activity_id}"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .fetch_one(store.pool())
        .await
        .unwrap();
    let err = TaskStore::row_to_activity(&row).unwrap_err();
    assert!(
        err.to_string().contains("to_status"),
        "expected 'to_status' in error message, got: {err}"
    );
}

#[tokio::test]
async fn row_to_activity_fails_on_decode_error_invalid_utf8_agent() {
    let (store, _task_id, activity_id) = store_with_activity().await;
    let sql = format!(
        "SELECT id, task_id, timestamp, event_type, \
             NULL as from_status, NULL as to_status, \
             X'DeadbEEF' as agent, NULL as model, details \
             FROM task_activity WHERE id = {activity_id}"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .fetch_one(store.pool())
        .await
        .unwrap();
    let err = TaskStore::row_to_activity(&row).unwrap_err();
    assert!(
        err.to_string().contains("agent"),
        "expected 'agent' in error message, got: {err}"
    );
}

#[tokio::test]
async fn row_to_activity_fails_on_decode_error_invalid_utf8_model() {
    let (store, _task_id, activity_id) = store_with_activity().await;
    let sql = format!(
        "SELECT id, task_id, timestamp, event_type, \
             NULL as from_status, NULL as to_status, \
             NULL as agent, X'DeadbEEF' as model, details \
             FROM task_activity WHERE id = {activity_id}"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .fetch_one(store.pool())
        .await
        .unwrap();
    let err = TaskStore::row_to_activity(&row).unwrap_err();
    assert!(
        err.to_string().contains("model"),
        "expected 'model' in error message, got: {err}"
    );
}

// ── get_statuses_by_external_ids: decode-failure rows are skipped ───────────

#[tokio::test]
async fn get_statuses_by_external_ids_skips_rows_with_decode_error() {
    // Insert a task row with a BLOB value in the external_id column (valid
    // SQLite but not valid UTF-8 TEXT), so row.try_get("external_id") fails
    // with a decode error. The method should skip this row (not panic) and
    // continue processing the remaining rows.
    let store = store::TaskStore::open_memory().await.unwrap();
    sqlx::query("INSERT INTO tasks (repo, external_id, status, title, origin) VALUES ('owner/repo', X'DeadbEEF', 'new', '', 'internal')")
        .execute(store.pool())
        .await
        .unwrap();
    // A second row with a valid external_id should be returned.
    sqlx::query("INSERT INTO tasks (repo, external_id, status, title, origin) VALUES ('owner/repo', 'valid-id', 'done', '', 'internal')")
        .execute(store.pool())
        .await
        .unwrap();

    let result = store
        .get_statuses_by_external_ids("owner/repo", &["valid-id"])
        .await
        .unwrap();
    assert_eq!(result.len(), 1, "expected exactly one result for valid-id");
    assert_eq!(result.get("valid-id"), Some(&TaskStatus::Done));
}

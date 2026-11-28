#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::fs::Permissions;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::time::Instant;

use super::*;
use crate::engine::agents::fixture;

fn runner() -> OpenCodeRunner {
    OpenCodeRunner::new()
}

#[test]
fn parse_opencode_ndjson_success() {
    let raw = r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start","snapshot":"..."}}
{"type":"text","timestamp":1001,"part":{"type":"text","text":"{\"status\":\"done\",\"summary\":\"hello\",\"accomplished\":[],\"remaining\":[],\"files\":[]}"}}
{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"stop","cost":0,"tokens":{"total":17512,"input":17509,"output":3}}}"#;

    let parsed = runner().parse_response(raw).unwrap();
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.response.summary, "hello");
    assert_eq!(parsed.input_tokens, Some(17509));
    assert_eq!(parsed.output_tokens, Some(3));
}

/// Newer opencode versions emit text directly in the event (no "part" wrapper).
/// The extract_text method must handle both formats.
#[test]
fn parse_opencode_ndjson_direct_text_format() {
    let raw = r#"{"type":"step_start","timestamp":1000,"sessionID":"ses_abc"}
{"type":"text","timestamp":1001,"sessionID":"ses_abc","text":"{\"status\":\"done\",\"summary\":\"fixed\",\"accomplished\":[],\"remaining\":[],\"files\":[]}"}
{"type":"step_finish","timestamp":1002,"sessionID":"ses_abc","part":{"type":"step-finish","reason":"stop","tokens":{"input":200,"output":10}}}"#;

    let parsed = runner().parse_response(raw).unwrap();
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.response.summary, "fixed");
    assert_eq!(parsed.input_tokens, Some(200));
    assert_eq!(parsed.output_tokens, Some(10));
}

#[test]
fn parse_opencode_concatenated_text() {
    let raw = r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}
{"type":"text","timestamp":1001,"part":{"type":"text","text":"Working on "}}
{"type":"text","timestamp":1002,"part":{"type":"text","text":"the task. "}}
{"type":"text","timestamp":1003,"part":{"type":"text","text":"{\"status\":\"done\",\"summary\":\"finished\",\"accomplished\":[],\"remaining\":[],\"files\":[]}"}}
{"type":"step_finish","timestamp":1004,"part":{"type":"step-finish","reason":"stop","tokens":{"input":100,"output":50}}}"#;

    let parsed = runner().parse_response(raw).unwrap();
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.input_tokens, Some(100));
    assert_eq!(parsed.output_tokens, Some(50));
}

#[test]
fn extract_router_text_prefers_json_payload() {
    let raw = r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"Thinking..."}}
{"type":"text","timestamp":1002,"part":{"type":"text","text":"{\"executor\":\"claude\",\"complexity\":\"medium\",\"reason\":\"fit\"}"}}"#;

    let text = extract_router_text(raw).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["executor"], "claude");
}

#[test]
fn parse_opencode_embedded_json_fragment_without_executor_is_ignored() {
    // NDJSON with a text event that contains a JSON-like fragment which does
    // NOT include routing keys (executor/status). This should NOT be
    // interpreted as a routing payload; parse_response should fail with
    // InvalidResponse rather than accidentally accepting the fragment.
    let raw = r#"{"type":"step_start","timestamp":1}
{"type":"text","timestamp":2,"part":{"type":"text","text":"Here is a debug trace: {\"trace\":{\"duration\":123,\"id\":\"abc\"}}"}}
{"type":"step_finish","timestamp":3,"part":{"type":"step-finish","reason":"stop"}}"#;

    let err = runner().parse_response(raw).unwrap_err();
    match err {
        AgentError::InvalidResponse { raw: _ } => {
            // expected
        }
        other => panic!("expected InvalidResponse, got: {other:?}"),
    }
}

#[test]
fn parse_opencode_error_event() {
    let raw = r#"{"type":"error","message":"rate limit exceeded for this model"}"#;

    let err = runner().parse_response(raw).unwrap_err();
    assert!(matches!(err, AgentError::RateLimit { .. }));
}

/// Regression: opencode runs that emit only tool_use / step_start / step_finish
/// events (no `text` events) must synthesize a clean needs_review response from
/// the event stream instead of returning InvalidResponse with the raw NDJSON
/// blob, which would otherwise be passed to synthesize_response_from_text and
/// leak as 500 bytes of raw JSON into the notification summary.
#[test]
fn parse_opencode_tool_only_stream_synthesizes_needs_review() {
    let raw = r#"{"type":"step_start","timestamp":1,"part":{"type":"step-start"}}
{"type":"tool_use","timestamp":2,"name":"bash","input":{"cmd":"ls"}}
{"type":"tool_use","timestamp":3,"name":"read","input":{"path":"src/main.rs"}}
{"type":"tool_use","timestamp":4,"name":"bash","input":{"cmd":"cargo check"}}
{"type":"step_finish","timestamp":5,"part":{"type":"step-finish","reason":"stop","tokens":{"input":500,"output":0}}}"#;

    let parsed = runner()
        .parse_response(raw)
        .expect("tool-only NDJSON must synthesize a response, not error");
    assert_eq!(parsed.response.status, "needs_review");
    assert!(
        !parsed.response.summary.contains("\"type\":\"step_start\""),
        "summary must not contain raw NDJSON; got: {}",
        parsed.response.summary
    );
    assert!(
        parsed.response.summary.contains("bash") && parsed.response.summary.contains("read"),
        "summary should mention tools used; got: {}",
        parsed.response.summary
    );
    assert!(
        parsed.response.summary.contains("stop"),
        "summary should mention step_finish reason; got: {}",
        parsed.response.summary
    );
    // Tokens from step_finish must still be propagated.
    assert_eq!(parsed.input_tokens, Some(500));
    assert_eq!(parsed.output_tokens, Some(0));
}

/// Regression test for issue #3467: when opencode's `text` event contains
/// a plain-prose completion summary (not JSON), parser::parse fails but
/// synthesize_response_from_text must rescue it instead of returning
/// InvalidResponse, mirroring the fallback claude.rs already has.
#[test]
fn parse_opencode_plain_prose_completion_synthesizes_response() {
    let raw = r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}
{"type":"text","timestamp":1001,"part":{"type":"text","text":"Fixed the typo and corrected the deadline reference in the review."}}
{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"stop","tokens":{"input":300,"output":40}}}"#;

    let parsed = runner()
        .parse_response(raw)
        .expect("plain-prose completion text must synthesize successfully");
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.input_tokens, Some(300));
    assert_eq!(parsed.output_tokens, Some(40));
}

#[test]
fn parse_opencode_step_finish_error() {
    let raw = r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}
{"type":"step_finish","timestamp":1001,"part":{"type":"step-finish","reason":"error","error":"context length exceeded"}}"#;

    let err = runner().parse_response(raw).unwrap_err();
    assert!(matches!(err, AgentError::ContextOverflow { .. }));
}

#[test]
fn parse_opencode_empty_returns_silent_failure() {
    let err = runner().parse_response("").unwrap_err();
    // Empty output with exit 0 is treated as silent failure so fallback.rs
    // can apply model cooldown and retry with free models
    assert!(matches!(err, AgentError::Unknown { exit_code: 0, .. }));
}

#[test]
fn extract_tokens_from_step_finish() {
    let events: Vec<serde_json::Value> = vec![
        serde_json::json!({"type":"step_finish","part":{"type":"step-finish","tokens":{"input":1000,"output":500,"total":1500}}}),
    ];
    let (input, output) = OpenCodeRunner::extract_tokens(&events);
    assert_eq!(input, Some(1000));
    assert_eq!(output, Some(500));
}

#[test]
fn classify_opencode_rate_limit() {
    let err = classify_opencode_message("429 Too Many Requests");
    assert!(matches!(err, AgentError::RateLimit { .. }));
}

#[test]
fn classify_opencode_model_not_found() {
    let err = classify_opencode_message("model not found: gpt-5");
    assert!(matches!(err, AgentError::ModelUnavailable { .. }));
}

#[test]
fn classify_opencode_model_not_found_with_suggestion() {
    // Issue #1934: opencode returns "Model not found: X. Did you mean: Y?"
    // The parser must extract "X", not "X. Did you mean"
    let err = classify_opencode_message(
        "Model not found: github-copilot/gemini-3.1-pro. Did you mean: gemini-3.1-pro-preview?",
    );
    match err {
        AgentError::ModelUnavailable { model, message } => {
            assert_eq!(model, "github-copilot/gemini-3.1-pro");
            assert!(message.contains("Did you mean"));
        }
        other => panic!("expected ModelUnavailable, got: {other:?}"),
    }
}

#[test]
fn classify_opencode_model_deprecated() {
    // Issue #2228: "The free model has been deprecated." should be ModelUnavailable
    // with the model extracted from "Transition to X" so the cooldown key is non-empty.
    let err = classify_opencode_message(
        "The free model has been deprecated. Transition to qwen/qwen3.6-plus for continued paid access.",
    );
    match err {
        AgentError::ModelUnavailable { model, .. } => {
            assert_eq!(
                model, "qwen/qwen3.6-plus",
                "expected model extracted from 'Transition to X', got: {model:?}"
            );
        }
        other => panic!("expected ModelUnavailable, got: {other:?}"),
    }
}

#[test]
fn classify_opencode_model_unavailable_for_free() {
    // Issue #3475: "This model is unavailable for free ... use this slug instead: X"
    // must classify as ModelUnavailable, not fall through to the transient
    // network-error path via the "Upstream request failed" substring.
    let err = classify_opencode_message(
        "network error: Error from provider (Console): Upstream request failed: [404] This model is unavailable for free. The paid version is available now - use this slug instead: inclusionai/ling-3.0-flash",
    );
    match err {
        AgentError::ModelUnavailable { model, .. } => {
            assert_eq!(
                model, "inclusionai/ling-3.0-flash",
                "expected model extracted from 'use this slug instead: X', got: {model:?}"
            );
        }
        other => panic!("expected ModelUnavailable, got: {other:?}"),
    }
}

#[test]
fn classify_opencode_model_not_available_in_country() {
    // Issue #3535: "This model is not available in your country." must classify as
    // ModelUnavailable (persistent per-model cooldown), not fall through to the
    // generic AgentFailed bucket which applies a weaker cooldown plus an
    // unwarranted agent-wide penalty.
    let err =
        classify_opencode_message("agent failed: This model is not available in your country.");
    match err {
        AgentError::ModelUnavailable { .. } => {}
        other => panic!("expected ModelUnavailable, got: {other:?}"),
    }
}

#[test]
fn classify_opencode_no_endpoints_found() {
    // Issue #2228: "No endpoints found for X." should be ModelUnavailable with model extracted
    let err = classify_opencode_message("No endpoints found for qwen/qwen3.6-plus:free.");
    match err {
        AgentError::ModelUnavailable { model, .. } => {
            assert_eq!(model, "qwen/qwen3.6-plus:free");
        }
        other => panic!("expected ModelUnavailable, got: {other:?}"),
    }
}

#[test]
fn build_command_opencode() {
    let r = runner();
    let perms = PermissionRules::default();
    let cmd = r.build_command(
        Some("anthropic/claude-sonnet-4-20250514"),
        "timeout 1800",
        "/tmp/sys.txt",
        "/tmp/msg.txt",
        &perms,
    );
    assert!(cmd.contains("opencode run"));
    assert!(cmd.contains("--model 'anthropic/claude-sonnet-4-20250514'"));
    assert!(cmd.contains("--format json"));
    assert!(
        cmd.starts_with(
            r#"OPENCODE_CONFIG_CONTENT='{"snapshot":false}' timeout 1800 opencode run"#
        ),
        "snapshots must be off for orch runs, got: {cmd}"
    );
    // Autonomous mode passes --dangerously-skip-permissions instead of
    // writing a config file or overriding XDG_CONFIG_HOME.
    assert!(
        cmd.contains("--dangerously-skip-permissions"),
        "expected --dangerously-skip-permissions, got: {cmd}"
    );
    assert!(
        !cmd.contains("opencode.json"),
        "should not write a config file, got: {cmd}"
    );
    assert!(
        !cmd.contains("XDG_CONFIG_HOME"),
        "should not override XDG_CONFIG_HOME, got: {cmd}"
    );
    assert!(
        !cmd.contains(".orch-opencode"),
        "should not reference .orch-opencode, got: {cmd}"
    );
    // Regression: the command must not start with a `cat sys msg |` pipe
    // stage. `build_runner_script` captures the agent's exit status via
    // `PIPESTATUS[0]` of `{agent_cmd} | tee ...` — a leading `cat |` would
    // make PIPESTATUS[0] always report cat's (always-zero) exit code
    // instead of opencode's real exit code. sys/msg files must be fed via
    // input redirection instead.
    assert!(
        !cmd.trim_start().starts_with("cat "),
        "opencode command must not start with a `cat |` pipe stage, got: {cmd}"
    );
    assert!(
        cmd.contains("< <(cat \"/tmp/sys.txt\" \"/tmp/msg.txt\")"),
        "sys/msg files must be fed via process substitution, got: {cmd}"
    );
}

#[test]
fn build_command_opencode_keeps_its_server_inside_the_os_sandbox() {
    let r = runner();
    let perms = PermissionRules {
        sandbox: SandboxLevel::Os,
        ..PermissionRules::default()
    };
    let cmd = r.build_command(None, "timeout 1800", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
    assert!(cmd.contains("opencode run --standalone "), "got: {cmd}");
    assert!(
        cmd.starts_with(r#"mkdir -p "$TMPDIR/pty" && OPENCODE_PTY_SOCKET_DIR="$TMPDIR/pty" "#),
        "got: {cmd}"
    );

    let cmd = r.build_command(
        None,
        "timeout 1800",
        "/tmp/sys.txt",
        "/tmp/msg.txt",
        &PermissionRules::default(),
    );
    assert!(!cmd.contains("--standalone"), "got: {cmd}");
    assert!(!cmd.contains("OPENCODE_PTY_SOCKET_DIR"), "got: {cmd}");
}

#[test]
fn build_command_opencode_supervised_no_skip_flag() {
    let r = runner();
    let perms = PermissionRules {
        autonomous: false,
        ..PermissionRules::default()
    };
    let cmd = r.build_command(
        Some("anthropic/claude-sonnet-4-20250514"),
        "timeout 1800",
        "/tmp/sys.txt",
        "/tmp/msg.txt",
        &perms,
    );
    assert!(
        !cmd.contains("--dangerously-skip-permissions"),
        "supervised mode must not pass --dangerously-skip-permissions, got: {cmd}"
    );
}

#[test]
fn split_model_variant_no_at() {
    let (base, variant) = OpenCodeRunner::split_model_variant("openai/gpt-5.5");
    assert_eq!(base, "openai/gpt-5.5");
    assert!(variant.is_none());
}

#[test]
fn split_model_variant_with_variant() {
    let (base, variant) = OpenCodeRunner::split_model_variant("openai/gpt-5.5@xhigh");
    assert_eq!(base, "openai/gpt-5.5");
    assert_eq!(variant, Some("xhigh"));
}

#[test]
fn split_model_variant_trailing_at_empty() {
    let (base, variant) = OpenCodeRunner::split_model_variant("openai/gpt-5.5@");
    assert_eq!(base, "openai/gpt-5.5");
    assert_eq!(variant, Some(""));
}

#[test]
fn split_model_variant_multiple_ats_last_wins() {
    let (base, variant) = OpenCodeRunner::split_model_variant("a@b@c@v");
    assert_eq!(base, "a@b@c");
    assert_eq!(variant, Some("v"));
}

#[test]
fn build_command_opencode_with_variant() {
    let r = runner();
    let perms = PermissionRules::default();
    let cmd = r.build_command(
        Some("openai/gpt-5.5@high"),
        "timeout 1800",
        "/tmp/sys.txt",
        "/tmp/msg.txt",
        &perms,
    );
    assert!(cmd.contains("--model 'openai/gpt-5.5'"));
    assert!(cmd.contains("--variant 'high'"));
}

#[test]
fn build_command_opencode_effort_overrides_variant_suffix() {
    let perms = PermissionRules::default();
    let cmd = runner().build_command_with_effort(
        Some("openai/gpt-5.5@low"),
        Some("high"),
        "",
        "/tmp/sys.txt",
        "/tmp/msg.txt",
        &perms,
    );
    assert!(cmd.contains("--model 'openai/gpt-5.5'"), "{cmd}");
    assert!(cmd.contains("--variant 'high'"), "{cmd}");
    assert!(!cmd.contains("--variant 'low'"), "{cmd}");
}

#[test]
fn classify_opencode_permission_denied() {
    let err = classify_opencode_message("user rejected permission for tool bash: cargo test");
    assert!(
        matches!(err, AgentError::PermissionDenied { .. }),
        "expected PermissionDenied, got: {err:?}"
    );
}

#[test]
fn classify_opencode_provider_returned_error() {
    // Issue #2478: "Provider returned error" is an opaque upstream failure from the
    // opencode CLI. Classify as RateLimit so fallback records a rate-limit signal
    // and applies cooldown behavior tuned for transient provider instability.
    let err = classify_opencode_message("Provider returned error");
    assert!(
        matches!(err, AgentError::RateLimit { .. }),
        "expected RateLimit, got: {err:?}"
    );
    let err_with_detail =
        classify_opencode_message("Provider returned error: upstream connection timed out");
    assert!(
        matches!(err_with_detail, AgentError::RateLimit { .. }),
        "expected RateLimit, got: {err_with_detail:?}"
    );
}

#[test]
fn classify_opencode_upstream_idle_timeout() {
    // Issue #3301: "Upstream idle timeout exceeded" must produce NetworkError so
    // fallback.rs applies a model-level cooldown instead of retrying immediately.
    let err = classify_opencode_message("Upstream idle timeout exceeded");
    assert!(
        matches!(err, AgentError::NetworkError { .. }),
        "expected NetworkError, got: {err:?}"
    );
}

#[test]
fn free_models_discovery_returns_vec() {
    // discover_free_opencode_models returns empty in test/debug builds
    let models = discover_free_opencode_models();
    assert!(models.is_empty(), "{models:?}");
}

#[test]
fn live_opencode_discovery_is_skipped_in_test_builds() {
    assert!(skip_live_opencode_discovery());
    let models = discover_opencode_models();
    assert!(models.is_empty(), "{models:?}");
    let models = discover_free_opencode_models();
    assert!(models.is_empty(), "{models:?}");
    prime_free_model_cache();
    let models = discover_opencode_models();
    assert!(models.is_empty(), "{models:?}");
}

/// The stub stands in for opencode 2.2.1, which starts `opencode serve --service`
/// as a child that outlives it
#[cfg(unix)]
#[tokio::test]
async fn models_cli_reaps_orphaned_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let sleeper_pid_path = dir.path().join("sleeper.pid");
    let script = dir.path().join("opencode");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n/bin/sleep 60 </dev/null >/dev/null 2>&1 &\necho $! > '{}'\necho test-model-free\n",
            sleeper_pid_path.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, Permissions::from_mode(0o755)).unwrap();

    // The production discovery timeout, so a loaded machine does not turn this into
    // a timeout run, which reaps the group too and would assert nothing
    let (models, reason) =
        run_opencode_models_cli_inner(script.to_str().unwrap(), Duration::from_secs(30)).await;
    assert_eq!(reason, "ok");
    assert_eq!(models, vec!["test-model-free".to_string()]);

    let sleeper: u32 = fs::read_to_string(&sleeper_pid_path)
        .unwrap()
        .trim()
        .parse()
        .expect("sleeper pid");
    let deadline = Instant::now() + Duration::from_secs(10);
    while pid_is_alive(sleeper) && Instant::now() < deadline {
        time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !pid_is_alive(sleeper),
        "sleeper {sleeper} must die with the process group when opencode models returns"
    );
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    // Signal 0 is the POSIX existence probe
    i32::try_from(pid).is_ok_and(|pid| cmd::raw_kill(pid, 0) == 0)
}

#[test]
fn empty_free_discovery_preserves_existing_cache_contents() {
    reset_model_caches_for_test();
    let cache = FREE_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));
    {
        let mut guard = cache.lock().unwrap_or_else(PoisonError::into_inner);
        *guard = (123, vec!["opencode/model-free".to_string()]);
    }

    update_discovered_models_cache(cache, Vec::new(), "free", "empty_output");

    let guard = cache.lock().unwrap_or_else(PoisonError::into_inner);
    // Timestamp must advance on a failed refresh so the TTL provides a
    // real backoff window instead of retrying on every subsequent call.
    assert!(guard.0 > 123);
    assert_eq!(guard.1, vec!["opencode/model-free".to_string()]);
}

#[test]
fn empty_all_models_discovery_preserves_existing_cache_contents() {
    reset_model_caches_for_test();
    let cache = ALL_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));
    {
        let mut guard = cache.lock().unwrap_or_else(PoisonError::into_inner);
        *guard = (
            456,
            vec![
                "opencode/model-free".to_string(),
                "opencode/model-paid".to_string(),
            ],
        );
    }

    update_discovered_models_cache(cache, Vec::new(), "all", "exit_status_failure");

    let guard = cache.lock().unwrap_or_else(PoisonError::into_inner);
    assert!(guard.0 > 456);
    assert_eq!(
        guard.1,
        vec![
            "opencode/model-free".to_string(),
            "opencode/model-paid".to_string(),
        ]
    );
}

#[test]
fn empty_discovery_populates_empty_cache() {
    reset_model_caches_for_test();
    let cache = FREE_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));

    update_discovered_models_cache(cache, Vec::new(), "free", "command_not_found");

    let guard = cache.lock().unwrap_or_else(PoisonError::into_inner);
    assert!(guard.0 > 0);
    assert!(guard.1.is_empty(), "{:?}", guard.1);
}

// ── Fixture-based tests ─────────────────────────────────────

/// Real failure: OpenCode rejects a tool permission in non-interactive mode.
/// The error has `{"error":{"name":"PermissionError","data":{"message":"user rejected permission..."}}}`.
/// Parser must extract the nested message and classify as PermissionDenied.
#[test]
fn fixture_opencode_permission_denied() {
    let raw = fixture::load(include_str!(
        "../../../../tests/fixtures/opencode_permission_denied.jsonl"
    ));
    let err = runner().parse_response(&raw.stdout).unwrap_err();
    assert!(
        matches!(err, AgentError::PermissionDenied { .. }),
        "expected PermissionDenied, got: {err:?}"
    );
}

/// Real failure: OpenCode returns nested error for unknown model.
/// The error object is `{"name":"UnknownError","data":{"message":"Model not found: X"}}`.
/// Parser must extract the nested message and classify as ModelUnavailable.
#[test]
fn fixture_opencode_model_not_found() {
    let raw = fixture::load(include_str!(
        "../../../../tests/fixtures/opencode_model_not_found.jsonl"
    ));
    let err = runner().parse_response(&raw.stdout).unwrap_err();
    assert!(
        matches!(err, AgentError::ModelUnavailable { .. }),
        "expected ModelUnavailable, got: {err:?}"
    );
    if let AgentError::ModelUnavailable { model, .. } = &err {
        assert_eq!(model, "anthropic/claude-sonnet-4-6");
    }
}

/// Real failure: OpenCode returns "Provider returned error" as an opaque upstream
/// provider failure. Classified as RateLimit so degraded/rate-limit tracking applies.
#[test]
fn fixture_opencode_provider_returned_error() {
    let raw = fixture::load(include_str!(
        "../../../../tests/fixtures/opencode_provider_returned_error.jsonl"
    ));
    let err = runner().parse_response(&raw.stdout).unwrap_err();
    assert!(
        matches!(err, AgentError::RateLimit { .. }),
        "expected RateLimit, got: {err:?}"
    );
}

// ── extract_text ─────────────────────────────────────────────

/// OpenCode: extract_text concatenates text events for review parsing.
#[test]
fn extract_text_returns_text_events() {
    let raw = concat!(
        r#"{"type":"step_start","timestamp":1000,"sessionID":"ses_abc"}"#,
        "\n",
        r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"test_results\":\"pass\",\"issues\":[]}"}}"#,
        "\n",
        r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"stop","cost":0,"tokens":{"total":100,"input":90,"output":10}}}"#,
    );
    let text = runner().extract_text(raw).unwrap();
    assert_eq!(
        text,
        r#"{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#
    );
}

/// OpenCode: extract_text with newer direct-text format.
#[test]
fn extract_text_direct_text_event_format() {
    let raw = concat!(
        r#"{"type":"step_start","timestamp":1000,"sessionID":"ses_abc"}"#,
        "\n",
        r#"{"type":"text","timestamp":1001,"sessionID":"ses_abc","text":"Here is my review:\n\n```json\n{\"decision\":\"request_changes\",\"notes\":\"Fix the bug\",\"test_results\":\"fail\",\"issues\":[]}\n```\n"}"#,
        "\n",
        r#"{"type":"step_finish","timestamp":1002,"sessionID":"ses_abc"}"#,
    );
    let text = runner().extract_text(raw).unwrap();
    assert!(
        text.contains("request_changes"),
        "expected review JSON, got: {text}"
    );
}

/// OpenCode: extract_text propagates RateLimit for terminal errors.
#[test]
fn extract_text_rate_limit_propagates() {
    let raw = concat!(
        r#"{"type":"step_start","timestamp":1000}"#,
        "\n",
        r#"{"type":"error","message":"rate limit exceeded: 429 Too Many Requests"}"#,
    );
    let err = runner().extract_text(raw).unwrap_err();
    assert!(
        matches!(err, AgentError::RateLimit { .. }),
        "expected RateLimit, got: {err:?}"
    );
}

/// OpenCode: extract_text propagates Auth error.
#[test]
fn extract_text_auth_error_propagates() {
    let raw = concat!(
        r#"{"type":"step_start","timestamp":1000}"#,
        "\n",
        r#"{"type":"error","message":"401 Unauthorized: invalid api key"}"#,
    );
    let err = runner().extract_text(raw).unwrap_err();
    assert!(
        matches!(err, AgentError::Auth { .. }),
        "expected Auth, got: {err:?}"
    );
}

/// OpenCode: extract_text on empty input returns empty string (not an error).
#[test]
fn extract_text_empty_input_returns_empty_ok() {
    let text = runner().extract_text("").unwrap();
    assert_eq!(text, "");
}

// ── find_opencode_result ────────────────────────────────────

#[test]
fn find_opencode_result_success_with_tokens() {
    let ndjson = concat!(
        r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}"#,
        "\n",
        r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"{\"decision\":\"approve\",\"notes\":\"LGTM\"}"}}"#,
        "\n",
        r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"stop","cost":0.05,"tokens":{"input":5000,"output":200}}}"#,
    );
    let result = find_opencode_result(ndjson).expect("should find result");
    assert!(!result.is_error);
    assert!(result.result_text.contains("approve"));
    assert_eq!(result.input_tokens, Some(5000));
    assert_eq!(result.output_tokens, Some(200));
    assert_eq!(result.cost_usd, Some(0.05));
    assert!(!result.truncated_by_length);
}

#[test]
fn find_opencode_result_error_event() {
    let ndjson = r#"{"type":"error","message":"rate limit exceeded: 429"}"#;
    let result = find_opencode_result(ndjson).expect("should find error result");
    assert!(result.is_error);
    assert!(result.result_text.contains("rate limit"));
    assert!(!result.truncated_by_length);
}

/// step_finish reason=length signals the model was cut off by its
/// output/reasoning token budget, not malformed output — the flag must
/// be set so callers avoid the persistent-format-failure cooldown.
#[test]
fn find_opencode_result_length_truncation() {
    let ndjson = concat!(
        r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}"#,
        "\n",
        r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"{\"decision\":"}}"#,
        "\n",
        r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"length","tokens":{"input":5485,"output":759,"reasoning":31241}}}"#,
    );
    let result = find_opencode_result(ndjson).expect("should find result");
    assert!(result.truncated_by_length);
    assert!(!result.is_error, "length truncation is not a hard error");
}

/// A reasoning model can burn its entire budget on hidden reasoning and
/// emit zero text before step_finish reason=length. The old early return
/// on empty result_text bailed out with None before truncated_by_length
/// was ever computed, so this got misclassified as parse_error and hit
/// the 4h-7d persistent-model cooldown instead of standard backoff.
#[test]
fn find_opencode_result_zero_output_length_truncation() {
    let ndjson = concat!(
        r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}"#,
        "\n",
        r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"length","tokens":{"total":71482,"input":39482,"output":0,"reasoning":40000}}}"#,
    );
    let result = find_opencode_result(ndjson).expect("should find result, not None");
    assert!(result.truncated_by_length);
    assert!(!result.is_error);
    assert_eq!(result.result_text, "");
}

/// opencode also emits reason="unknown" with all-zero tokens for the same
/// zero-output failure mode — must be treated as an equivalent signal.
#[test]
fn find_opencode_result_zero_output_unknown_reason_truncation() {
    let ndjson = concat!(
        r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}"#,
        "\n",
        r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"unknown","tokens":{"input":0,"output":0,"reasoning":0}}}"#,
    );
    let result = find_opencode_result(ndjson).expect("should find result, not None");
    assert!(result.truncated_by_length);
    assert!(!result.is_error);
    assert_eq!(result.result_text, "");
}

/// reason="unknown" with nonzero output tokens means the model did
/// produce something and the step just wasn't cleanly terminated — not
/// the same "wrote nothing" signal, so it must not be flagged.
#[test]
fn find_opencode_result_unknown_reason_with_output_not_truncated() {
    let ndjson = concat!(
        r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}"#,
        "\n",
        r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"partial answer"}}"#,
        "\n",
        r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"unknown","tokens":{"input":100,"output":50,"reasoning":0}}}"#,
    );
    let result = find_opencode_result(ndjson).expect("should find result");
    assert!(!result.truncated_by_length);
    assert!(!result.is_error);
}

#[test]
fn find_opencode_result_empty_returns_none() {
    assert!(find_opencode_result("").is_none());
    assert!(find_opencode_result("   ").is_none());
}

#[test]
fn find_opencode_result_direct_text_format() {
    let ndjson = concat!(
        r#"{"type":"step_start","timestamp":1000}"#,
        "\n",
        r#"{"type":"text","timestamp":1001,"text":"Review complete. All tests passed."}"#,
        "\n",
        r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"stop","tokens":{"input":100,"output":10}}}"#,
    );
    let result = find_opencode_result(ndjson).expect("should find result");
    assert!(!result.is_error);
    assert!(result.result_text.contains("All tests passed"));
}

#[test]
fn find_opencode_result_plain_text_returns_none() {
    assert!(find_opencode_result("just some plain text").is_none());
}

use tracing::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt;

use std::io;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;

use super::*;

#[test]
fn get_runner_returns_correct_types() {
    assert_eq!(get_runner("claude").name(), "claude");
    assert_eq!(get_runner("kimi").name(), "kimi");
    assert_eq!(get_runner("minimax").name(), "minimax");
    assert_eq!(get_runner("codex").name(), "codex");
    assert_eq!(get_runner("opencode").name(), "opencode");
    assert_eq!(get_runner("grok").name(), "grok");
    assert_eq!(
        get_runner("claude").router_transcript(),
        RouterTranscript::Messages
    );
    assert_eq!(
        get_runner("kimi").router_transcript(),
        RouterTranscript::Messages
    );
    assert_eq!(
        get_runner("minimax").router_transcript(),
        RouterTranscript::Messages
    );
    assert_eq!(
        get_runner("grok").router_transcript(),
        RouterTranscript::Messages
    );
    assert_eq!(
        get_runner("glm").router_transcript(),
        RouterTranscript::Messages
    );
    assert_eq!(
        get_runner("olm").router_transcript(),
        RouterTranscript::Messages
    );
    assert_eq!(
        get_runner("opencode").router_transcript(),
        RouterTranscript::OpenCode
    );
    assert_eq!(
        get_runner("codex").router_transcript(),
        RouterTranscript::Raw
    );
    assert!(supports_session("grok"));
    assert!(!supports_session("codex"));
    // olm and glm are claude-compatible and should return correct name
    assert_eq!(get_runner("olm").name(), "olm");
    assert_eq!(get_runner("glm").name(), "glm");
    // Unknown falls back to claude-compatible
    assert_eq!(get_runner("unknown-agent").name(), "unknown-agent");
}

/// Regression test: known claude-compatible agents (including glm) must not
/// emit "unknown agent" warnings when get_runner() is called.
#[test]
fn get_runner_no_warning_for_claude_compatible_agents() {
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            CaptureWriter(Arc::clone(&self.0))
        }
    }

    let output: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let output_clone = Arc::clone(&output);

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::WARN.into()))
        .with_writer(CaptureWriter(output_clone))
        .with_ansi(false)
        .finish()
        .init();

    for agent in &["claude", "kimi", "minimax", "olm", "glm", "grok"] {
        get_runner(agent);
    }

    let captured = {
        let guard = output.lock().unwrap();
        String::from_utf8_lossy(&guard).to_string()
    };
    assert!(
        !captured.contains("unknown agent"),
        "known claude-compatible agents must not emit 'unknown agent' warnings; got: {captured}"
    );
}

#[test]
fn agent_error_display() {
    let e = AgentError::RateLimit {
        message: "429 Too Many Requests".to_string(),
    };
    assert!(e.to_string().contains("rate limit"));

    let e = AgentError::Timeout {
        elapsed: Duration::from_mins(30),
    };
    assert!(e.to_string().contains("1800"));
}

#[test]
fn pattern_detect_rate_limit() {
    assert!(patterns::detect_rate_limit("Error: rate limit exceeded").is_some());
    assert!(patterns::detect_rate_limit("HTTP 429 Too Many Requests").is_some());
    assert!(patterns::detect_rate_limit("You've hit your usage limit").is_some());
    assert!(
        patterns::detect_rate_limit(
            "You've hit your session limit · resets 10:30am (America/Sao_Paulo)"
        )
        .is_some()
    );
    assert!(patterns::detect_rate_limit(
        "Upstream error from Alibaba: Request rate increased too quickly. \
             To ensure system stability, please adjust your client logic to scale requests more smoothly over time."
    ).is_some());
    assert!(patterns::detect_rate_limit("all good").is_none());
}

#[test]
fn classify_from_text_detects_weekly_limit_as_rate_limit() {
    // Regression test for issue #3283: Claude "weekly limit" errors were
    // classified as `failed` instead of `rate_limit` because "weekly limit"
    // was not in the detect_rate_limit pattern list.
    let system_event = r#"{"type":"system","subtype":"error","error":{"type":"weekly_limit","message":"You've hit your weekly limit · resets Jun 9 at 1am (America/Sao_Paulo)"}}"#;
    let work_product = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Working on the task...\"}]}}\n"
        .repeat(200);
    let text = format!("{system_event}\n{work_product}");
    assert!(
        text.len() > 3000,
        "test output must exceed tail window to be meaningful"
    );
    let err = patterns::classify_from_text(1, &text);
    assert!(
        matches!(err, AgentError::RateLimit { .. }),
        "weekly limit as early system event must be detected via head scan, got: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("weekly limit"),
        "error message should reference weekly limit, got: {msg}"
    );
}

#[test]
fn detect_rate_limit_github_copilot_monthly_quota() {
    // Regression test for issue #3302: "You have exceeded your monthly quota"
    // was not matched because detect_rate_limit only checked "quota exceeded"
    // (reversed word order), causing 4 immediate retries with no cooldown.
    let err = patterns::detect_rate_limit("You have exceeded your monthly quota");
    assert!(
        matches!(err, Some(AgentError::RateLimit { .. })),
        "monthly quota exhaustion must be detected as rate limit, got: {err:?}"
    );
}

#[test]
fn detect_rate_limit_nvidia_resourceexhausted() {
    // Regression test for issue #3361: Nvidia "ResourceExhausted: Worker local total
    // request limit reached" was classified as generic `failed` instead of `rate_limit`,
    // bypassing the cooldown path and weakening routing health signals.
    let opencode_error = r#"opencode agent error: agent failed: "Upstream error from Nvidia: ResourceExhausted: Worker local total request limit reached (42/32)""#;
    let err = patterns::detect_rate_limit(opencode_error);
    assert!(
        matches!(err, Some(AgentError::RateLimit { .. })),
        "Nvidia ResourceExhausted must be detected as rate limit, got: {err:?}"
    );

    // Both sub-patterns must match independently.
    assert!(
        patterns::detect_rate_limit("ResourceExhausted: some capacity error").is_some(),
        "bare ResourceExhausted must match"
    );
    assert!(
        patterns::detect_rate_limit("Worker local total request limit reached (10/8)").is_some(),
        "worker local total request limit reached must match"
    );

    // classify_from_text must also route this to RateLimit (exercises the tail-scan path).
    let err = patterns::classify_from_text(1, opencode_error);
    assert!(
        matches!(err, AgentError::RateLimit { .. }),
        "classify_from_text must produce RateLimit for Nvidia ResourceExhausted, got: {err:?}"
    );
}

#[test]
fn detect_rate_limit_message_contains_context_not_tail() {
    // Simulate a long claude NDJSON output where the rate limit message is in
    // the middle and usage stats JSON is at the tail.
    let padding = "x".repeat(5000);
    let text = format!(
        "{padding}you've reached your usage limit for this billing cycle{padding}\
             {{\"web_fetch_requests\":0,\"service_tier\":\"standard\"}}"
    );
    let err = patterns::detect_rate_limit(&text).expect("should detect");
    let msg = err.to_string();
    assert!(
        msg.contains("usage limit"),
        "message should contain the actual error, got: {msg}"
    );
    assert!(
        !msg.contains("web_fetch_requests"),
        "message should not be the tail JSON, got: {msg}"
    );
}

#[test]
fn pattern_detect_rate_limit_529_not_bare() {
    // Bare "529" in various contexts should NOT trigger rate limit detection.
    // These are false positive cases we want to avoid.
    assert!(
        patterns::detect_rate_limit("error on line 529").is_none(),
        "bare line number 529 should not match"
    );
    assert!(
        patterns::detect_rate_limit("file size 529 bytes").is_none(),
        "bare file size 529 should not match"
    );
    assert!(
        patterns::detect_rate_limit("port :5290 in config").is_none(),
        "port number containing 529 should not match"
    );
    assert!(
        patterns::detect_rate_limit("timeout after 5290ms").is_none(),
        "duration containing 529 should not match"
    );
    // But HTTP 529 status codes SHOULD match.
    assert!(
        patterns::detect_rate_limit("HTTP 529 Service Unavailable").is_some(),
        "HTTP 529 status should match"
    );
    assert!(
        patterns::detect_rate_limit("http 529").is_some(),
        "http 529 should match"
    );
    assert!(
        patterns::detect_rate_limit("529 service unavailable").is_some(),
        "529 service unavailable should match"
    );
    assert!(
        patterns::detect_rate_limit("status: 529").is_some(),
        "status: 529 should match"
    );
}

#[test]
fn pattern_detect_auth() {
    assert!(patterns::detect_auth_error("401 Unauthorized").is_some());
    assert!(patterns::detect_auth_error("HTTP 401").is_some());
    assert!(patterns::detect_auth_error("error: 401").is_some());
    assert!(patterns::detect_auth_error("403 Forbidden").is_some());
    assert!(patterns::detect_auth_error("HTTP 403").is_some());
    assert!(patterns::detect_auth_error("HTTP 407 Authentication Required").is_some());
    assert!(patterns::detect_auth_error("407 Proxy Authentication Required").is_some());
    assert!(patterns::detect_auth_error("invalid api key").is_some());
    assert!(patterns::detect_auth_error("billing expired").is_some());
    assert!(patterns::detect_auth_error("task done").is_none());
    // Bare numbers in JSON must NOT trigger auth classification.
    assert!(patterns::detect_auth_error("duration_api_ms 4010292").is_none());
    assert!(
        patterns::detect_auth_error(
            "API Error: Unable to connect to API (ConnectionRefused) duration_api_ms 4010292"
        )
        .is_none()
    );
    // JSON field with colon: "duration_api_ms": 4010292 — contains ": 401" as substring.
    assert!(patterns::detect_auth_error(r#""duration_api_ms": 4010292"#).is_none());
    assert!(
        patterns::detect_auth_error(r#"{"error":"ConnectionRefused","duration_api_ms": 4010292}"#)
            .is_none()
    );
    // ": 401" at end-of-string or followed by non-digit must still match.
    assert!(patterns::detect_auth_error("status: 401").is_some());
    assert!(patterns::detect_auth_error("error: 403").is_some());
}

#[test]
fn pattern_detect_auth_insufficient_balance() {
    // Regression test for issue #3642: "insufficient balance" (and by extension
    // "402 Payment Required: insufficient balance") must be detected as an auth
    // error so it routes through the credit exhaustion path, not the generic 5-min
    // backoff. This ensures the #3642 cooldown behavior is properly triggered.
    assert!(
        patterns::detect_auth_error("402 Payment Required: insufficient balance").is_some(),
        "insufficient balance must be detected as auth error"
    );
    assert!(
        patterns::detect_auth_error("insufficient balance").is_some(),
        "bare insufficient balance must be detected as auth error"
    );
    // The stored message should contain the matched pattern.
    let err = patterns::detect_auth_error("402 Payment Required: insufficient balance")
        .expect("should detect auth error");
    let AgentError::Auth { message } = err else {
        panic!("expected Auth, got {err:?}");
    };
    assert!(
        message.to_lowercase().contains("insufficient balance"),
        "stored message should contain the matched pattern, got: {message}"
    );
    // The message should be the line containing the auth error, not the tail.
    assert!(
        message.lines().count() <= 2,
        "message should be the auth line, not the full tail: {}",
        message.len()
    );
}

#[test]
fn auth_error_message_contains_match_not_tail() {
    // Simulate real agent output: auth error appears early, NDJSON session
    // metadata at the end. Stored message must be the error line, not the
    // trailing JSON that agents emit at exit.
    let mut text = "some output\nunauthorized: your api key has expired\nx".repeat(400);
    text.push_str(r#","outputTokens":744,"cacheReadInputTokens":344785,"uuid":"86d13b1f-..."#);
    let err = patterns::detect_auth_error(&text).expect("should detect auth error");
    let AgentError::Auth { message } = err else {
        panic!("expected Auth, got {err:?}");
    };
    assert!(
        message.to_lowercase().contains("unauthorized"),
        "stored message should contain the matched pattern, got: {message}"
    );
    assert!(
        !message.contains("outputTokens"),
        "stored message must not be the trailing NDJSON, got: {message}"
    );
    // The message should be the line containing the auth error, not the tail.
    assert!(
        message.lines().count() <= 2,
        "message should be the auth line, not the full tail: {}",
        message.len()
    );
}

#[test]
fn pattern_detect_network_error() {
    assert!(patterns::detect_network_error("connection refused").is_some());
    assert!(patterns::detect_network_error("ConnectionRefused").is_some());
    assert!(patterns::detect_network_error("Unable to connect to API").is_some());
    assert!(patterns::detect_network_error("ECONNREFUSED").is_some());
    assert!(patterns::detect_network_error("network unreachable").is_some());
    assert!(patterns::detect_network_error("Upstream idle timeout exceeded").is_some());
    assert!(patterns::detect_network_error("upstream idle timeout").is_some());
    assert!(patterns::detect_network_error("Streaming response failed").is_some());
    assert!(patterns::detect_network_error("Upstream request failed").is_some());
    assert!(patterns::detect_network_error("all systems operational").is_none());
}

#[test]
fn network_error_message_contains_match_not_tail() {
    // Simulate real agent output: error appears early, unrelated stats JSON at the end.
    let prefix = "connection refused connecting to api.anthropic.com:443 ";
    let mut suffix = "x".repeat(2000);
    suffix.push_str(r#"":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":0}"#);
    let text = format!("{prefix}{suffix}");

    let err = patterns::detect_network_error(&text).expect("should detect network error");
    let AgentError::NetworkError { message } = err else {
        panic!("expected NetworkError, got {err:?}");
    };
    assert!(
        message.to_lowercase().contains("connection refused"),
        "stored message should contain the matched pattern, got: {message}"
    );
    assert!(
        !message.contains("web_fetch_requests"),
        "stored message must not be the unrelated stats tail, got: {message}"
    );
}

#[test]
fn pattern_detect_context_overflow() {
    assert!(patterns::detect_context_overflow("context_length_exceeded").is_some());
    assert!(patterns::detect_context_overflow("too many tokens in prompt").is_some());
    assert!(patterns::detect_context_overflow("success").is_none());
}

#[test]
fn pattern_detect_worktree_missing() {
    assert!(
        patterns::detect_worktree_missing("worktree directory does not exist: /tmp/wt").is_some()
    );
    assert!(patterns::detect_worktree_missing("all good").is_none());
}

#[test]
fn pattern_detect_missing_tool() {
    assert!(patterns::detect_missing_tool("bun: command not found").is_some());
    assert!(patterns::detect_missing_tool("env: anchor: no such file").is_some());
    assert!(patterns::detect_missing_tool("everything works").is_none());
}

#[test]
fn pattern_detect_permission_denied() {
    assert!(patterns::detect_permission_denied("permission denied: /etc/hosts").is_some());
    assert!(patterns::detect_permission_denied("sandbox violation detected").is_some());
    assert!(patterns::detect_permission_denied("disallowed tool: Bash(rm *)").is_some());
    assert!(patterns::detect_permission_denied("task completed").is_none());
}

#[test]
fn pattern_detect_waiting_for_input() {
    assert!(patterns::detect_waiting_for_input("Enter passphrase for key").is_some());
    assert!(patterns::detect_waiting_for_input("1Password CLI required").is_some());
    assert!(
        patterns::detect_waiting_for_input("SSH authentication required for deploy key").is_some()
    );
    assert!(patterns::detect_waiting_for_input("HTTP 407 Proxy Authentication Required").is_none());
    assert!(patterns::detect_waiting_for_input("done").is_none());
}

#[test]
fn pattern_detect_stale_session() {
    // Exact error message from Claude
    let err = patterns::detect_stale_session(
        "No conversation found with session ID: 2be572c4-57bb-4f86-bc03-b593c329177c",
    );
    assert!(err.is_some(), "should detect stale session");
    if let Some(AgentError::StaleSession { session_id }) = err {
        assert_eq!(session_id, "2be572c4-57bb-4f86-bc03-b593c329177c");
    }

    // Case-insensitive
    let err = patterns::detect_stale_session("no conversation found with session id: abc-123");
    assert!(err.is_some(), "should detect lowercase variant");

    // Non-matching text
    assert!(patterns::detect_stale_session("all good").is_none());
    assert!(patterns::detect_stale_session("conversation started").is_none());

    // Multi-byte UTF-8 characters before the match must not panic.
    // German ß lowercases to "ss" (1 byte → 2 bytes), so using a byte
    // offset from `lower` directly into `text` would be wrong.
    let multibyte = "Straße: No conversation found with session ID: dead-beef";
    let err = patterns::detect_stale_session(multibyte);
    assert!(
        err.is_some(),
        "should detect stale session after multi-byte chars"
    );
    if let Some(AgentError::StaleSession { session_id }) = err {
        assert_eq!(session_id, "dead-beef");
    }
}

#[test]
fn stale_session_display() {
    let e = AgentError::StaleSession {
        session_id: "abc-123".to_string(),
    };
    assert!(e.to_string().contains("stale session"));
    assert!(e.to_string().contains("abc-123"));
}

#[test]
fn classify_from_text_timeout() {
    let err = patterns::classify_from_text(124, "");
    assert!(matches!(err, AgentError::Timeout { .. }));
}

#[test]
fn classify_from_text_missing_tool_before_rate_limit() {
    // Missing tool takes priority over rate limit patterns
    let err = patterns::classify_from_text(1, "bun: command not found rate limit");
    assert!(matches!(err, AgentError::MissingTool { .. }));
}

#[test]
fn classify_from_text_worktree_missing() {
    let err = patterns::classify_from_text(1, "worktree directory does not exist: /tmp/wt");
    assert!(matches!(err, AgentError::AgentFailed { .. }));
}

#[test]
fn classify_from_text_model_not_found_backtick() {
    // Issue #3604: plain-text "model not found" errors (no structured
    // NDJSON event) must classify as ModelUnavailable, not fall through to
    // Unknown, so the agent-wide cooldown is not bumped for a bad model name.
    let text = "codex failed: model unavailable (gpt-5.5): Reconnecting... 2/5 \
            (unexpected status 404 Not Found: The model `gpt-5.5` does not exist \
            or you do not have access to it.)";
    let err = patterns::classify_from_text(1, text);
    match err {
        AgentError::ModelUnavailable { model, .. } => assert_eq!(model, "gpt-5.5"),
        other => panic!("expected ModelUnavailable, got: {other:?}"),
    }
}

#[test]
fn classify_from_text_model_metadata_not_found() {
    let text = "Model metadata for `gpt-5.4` not found. Defaulting to fallback \
            metadata; this can degrade performance and cause issues.";
    let err = patterns::classify_from_text(1, text);
    match err {
        AgentError::ModelUnavailable { model, .. } => assert_eq!(model, "gpt-5.4"),
        other => panic!("expected ModelUnavailable, got: {other:?}"),
    }
}

#[test]
fn classify_from_text_no_false_positive_from_work_product() {
    // Regression test for issue #1292: agent output containing rate-limit
    // keywords in code/diffs must not be misclassified as a rate limit error.
    let work_product = "Added `is_copilot_model(model) -> bool` helper\n\
             - Added copilot quota patterns to `parse_retry_at`\n\
             fn detect_rate_limit(text: &str) -> Option<AgentError> {\n\
             quota exceeded handling for billing cycle\n"
        .repeat(50); // ~200 lines of work product — well over 3000 chars
    let actual_error = "\nerror: command not found: cargo\n";
    let text = format!("{work_product}{actual_error}");
    let err = patterns::classify_from_text(1, &text);
    assert!(
        !matches!(err, AgentError::RateLimit { .. }),
        "work product keywords must not trigger rate limit, got: {err:?}"
    );
}

#[test]
fn classify_from_text_still_detects_real_rate_limit_at_tail() {
    // A real rate-limit error printed at the end of the output must still
    // be detected, even if work product precedes it.
    let padding = "normal agent work output ".repeat(100);
    let text = format!("{padding}\nError: rate limit exceeded (429 Too Many Requests)\n");
    let err = patterns::classify_from_text(1, &text);
    assert!(
        matches!(err, AgentError::RateLimit { .. }),
        "real rate limit at tail must be detected, got: {err:?}"
    );
}

#[test]
fn classify_from_text_ignores_markdown_rate_limit_table_at_tail() {
    let padding = "normal agent work output ".repeat(250);
    let text = format!(
        "{padding}\n+| agent | model | outcome | count |\n+| codex | gpt-5.4 | failed | 1 |\n+| kimi | opus | rate_limit | 1 |\n"
    );
    let err = patterns::classify_from_text(1, &text);
    assert!(
        !matches!(err, AgentError::RateLimit { .. }),
        "markdown table cell must not trigger rate limit, got: {err:?}"
    );
}

#[test]
fn classify_from_text_rate_limit_non_ascii_prefix_does_not_panic() {
    // Regression test: non-ASCII characters before `rate_limit` that change
    // byte length when lowercased (e.g. İ 2→3 bytes, ﬂ 3→2 bytes) must not
    // cause is_word_boundary to index text at an invalid byte offset.
    let text = "Aﬂαβrate_limit error: too many requests";
    let err = patterns::classify_from_text(1, text);
    assert!(
        matches!(err, AgentError::RateLimit { .. }),
        "real rate_limit should still be detected with non-ASCII prefix, got: {err:?}"
    );
}

#[test]
fn classify_from_text_detects_session_limit_as_early_system_event() {
    // Regression test for issue #3272: Claude session limit errors appear as
    // `type:system` NDJSON events early in the stream, not as `type:result`
    // lines. When the agent produces significant output after the error, the
    // system event is pushed out of the tail window and missed by tail-only
    // scanning. The head scan must catch it.
    let system_event = r#"{"type":"system","subtype":"error","error":{"type":"session_limit","message":"You've hit your session limit · resets 4:10am (America/Sao_Paulo)"}}"#;
    // Generate enough work product (>3000 bytes) to push the system event
    // out of the tail window.
    let work_product = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Working on the task...\"}]}}\n"
        .repeat(200);
    let text = format!("{system_event}\n{work_product}");
    // Verify the output is large enough that the event is not in the tail.
    assert!(
        text.len() > 3000,
        "test output must exceed tail window to be meaningful"
    );
    let err = patterns::classify_from_text(1, &text);
    assert!(
        matches!(err, AgentError::RateLimit { .. }),
        "session limit as early system event must be detected via head scan, got: {err:?}"
    );
    // The message should contain the actual error, not unrelated tail content.
    let msg = err.to_string();
    assert!(
        msg.contains("session limit"),
        "error message should contain the rate limit reason, got: {msg}"
    );
}

#[test]
fn classify_from_text_no_false_positive_auth_from_work_product() {
    // Regression test for issue #2126: auth keywords in agent work product
    // must not trigger auth classification when the real failure is
    // unrelated and appears at the end of the output.
    let work_product = "if response.status == 401 { return Err(\"unauthorized\") }\n\
             // handle 403 forbidden errors\n\
             // billing checks happen elsewhere\n"
        .repeat(80);
    let actual_error = "\nerror: command not found: cargo\n";
    let text = format!("{work_product}{actual_error}");
    let err = patterns::classify_from_text(1, &text);
    assert!(
        !matches!(err, AgentError::Auth { .. }),
        "work product auth keywords must not trigger auth classification, got: {err:?}"
    );
    assert!(
        matches!(err, AgentError::MissingTool { .. }),
        "the real tail error should still win, got: {err:?}"
    );
}

#[test]
fn classify_from_text_still_detects_real_auth_at_tail() {
    let padding = "normal agent work output ".repeat(100);
    let text = format!("{padding}\nHTTP 401 Unauthorized\n");
    let err = patterns::classify_from_text(1, &text);
    assert!(
        matches!(err, AgentError::Auth { .. }),
        "real auth error at tail must be detected, got: {err:?}"
    );
}

#[test]
fn classify_from_text_no_false_positive_context_overflow_from_work_product() {
    // Regression test for issue #2205: context overflow keywords in agent
    // work product must not trigger ContextOverflow when the real failure is
    // unrelated and appears at the end of the output.
    let work_product = "const MAX_TOKEN_LIMIT = 10000;\n\
             // Ensure we don't exceed the token limit per request\n\
             fn check_token_limit(tokens: usize) { ... }\n\
             if total_tokens > MAX_TOKEN_LIMIT { ... }\n\
             // Note: too many tokens in the prompt causes context_length_exceeded\n"
        .repeat(80);
    let actual_error = "\nerror: command not found: cargo\n";
    let text = format!("{work_product}{actual_error}");
    let err = patterns::classify_from_text(1, &text);
    assert!(
        !matches!(err, AgentError::ContextOverflow { .. }),
        "work product context keywords must not trigger ContextOverflow, got: {err:?}"
    );
    assert!(
        matches!(err, AgentError::MissingTool { .. }),
        "the real tail error should still win, got: {err:?}"
    );
}

#[test]
fn classify_from_text_still_detects_real_context_overflow_at_tail() {
    let padding = "normal agent work output ".repeat(100);
    let text = format!("{padding}\nError: context_length_exceeded\n");
    let err = patterns::classify_from_text(1, &text);
    assert!(
        matches!(err, AgentError::ContextOverflow { .. }),
        "real context overflow at tail must be detected, got: {err:?}"
    );
}

#[test]
fn classify_from_text_no_false_positive_permission_denied_from_work_product() {
    // Regression test for issue #2205: permission-related keywords in agent
    // work product must not trigger PermissionDenied when the real failure
    // is unrelated and appears at the end of the output.
    let work_product = "match fs::metadata(path) {\n\
             Err(ref e) if e.kind() == ErrorKind::PermissionDenied => {\n\
                 bail!(\"access denied: {path}\");\n\
             }\n\
             // Check if directory is writable\n\
             if !path.is_writable() { ... }\n\
             }\n"
    .repeat(80);
    let actual_error = "\nerror: command not found: cargo\n";
    let text = format!("{work_product}{actual_error}");
    let err = patterns::classify_from_text(1, &text);
    assert!(
        !matches!(err, AgentError::PermissionDenied { .. }),
        "work product permission keywords must not trigger PermissionDenied, got: {err:?}"
    );
    assert!(
        matches!(err, AgentError::MissingTool { .. }),
        "the real tail error should still win, got: {err:?}"
    );
}

#[test]
fn classify_from_text_still_detects_real_permission_denied_at_tail() {
    let padding = "normal agent work output ".repeat(100);
    let text = format!("{padding}\npermission denied: /etc/hosts\n");
    let err = patterns::classify_from_text(1, &text);
    assert!(
        matches!(err, AgentError::PermissionDenied { .. }),
        "real permission denied at tail must be detected, got: {err:?}"
    );
}

#[test]
fn classify_from_text_no_false_positive_waiting_for_input_from_work_product() {
    // Regression test for issue #2205: input-request keywords in agent work
    // product must not trigger WaitingForInput when the real failure is
    // unrelated and appears at the end of the output.
    let work_product = "// Handle password prompts gracefully\n\
             if prompt.contains(\"password:\") {\n\
                 log::warn!(\"Password prompt detected — skipping interactive step\");\n\
             }\n\
             // SSH agent will ask for the deploy key passphrase\n\
             fn handle_ssh_passphrase() { ... }\n\
             )\n"
    .repeat(80);
    let actual_error = "\nerror: command not found: cargo\n";
    let text = format!("{work_product}{actual_error}");
    let err = patterns::classify_from_text(1, &text);
    assert!(
        !matches!(err, AgentError::WaitingForInput { .. }),
        "work product input keywords must not trigger WaitingForInput, got: {err:?}"
    );
    assert!(
        matches!(err, AgentError::MissingTool { .. }),
        "the real tail error should still win, got: {err:?}"
    );
}

#[test]
fn classify_from_text_still_detects_real_waiting_for_input_at_tail() {
    let padding = "normal agent work output ".repeat(100);
    let text = format!("{padding}\nEnter passphrase for key:\n");
    let err = patterns::classify_from_text(1, &text);
    assert!(
        matches!(err, AgentError::WaitingForInput { .. }),
        "real waiting for input at tail must be detected, got: {err:?}"
    );
}

#[test]
fn classify_from_text_prefers_auth_over_permission_denied() {
    let err = patterns::classify_from_text(1, "HTTP 403 access denied: invalid token");
    assert!(
        matches!(err, AgentError::Auth { .. }),
        "auth errors must outrank permission denied, got: {err:?}"
    );
}

#[test]
fn classify_from_text_prefers_auth_over_waiting_for_input() {
    let err = patterns::classify_from_text(1, "HTTP 407 Proxy Authentication Required");
    assert!(
        matches!(err, AgentError::Auth { .. }),
        "HTTP auth failures must not be treated as waiting for input, got: {err:?}"
    );
}

#[test]
fn classify_from_text_detects_stale_session() {
    // Regression test for issue #1800: classify_from_text must call
    // detect_stale_session to catch "No conversation found with session ID"
    // errors. Without this, the error would be misclassified as Unknown and
    // the control.rs recovery code would not trigger.
    let text = "No conversation found with session ID: 2be572c4-57bb-4f86-bc03-b593c329177c";
    let err = patterns::classify_from_text(1, text);
    assert!(
        matches!(err, AgentError::StaleSession { .. }),
        "stale session must be detected via classify_from_text, got: {err:?}"
    );
    // Verify the Display format matches what control.rs expects
    assert!(
        err.to_string().contains("stale session"),
        "error display must contain 'stale session' for recovery code to trigger"
    );
}

#[test]
fn classify_from_text_detects_socket_closed() {
    // Regression test for issue #3045: "socket connection was closed" must be
    // classified as NetworkError, not Unknown.
    let text = "API Error: The socket connection was closed unexpectedly. \
             For more information, pass `verbose: true` in the second argument to fetch()";
    let err = patterns::classify_from_text(1, text);
    assert!(
        matches!(err, AgentError::NetworkError { .. }),
        "socket closed must be NetworkError, got: {err:?}"
    );
}

#[test]
fn classify_from_text_detects_socket_hang_up() {
    let err = patterns::classify_from_text(1, "Error: socket hang up");
    assert!(
        matches!(err, AgentError::NetworkError { .. }),
        "socket hang up must be NetworkError, got: {err:?}"
    );
}

#[test]
fn classify_from_text_detects_fetch_failed() {
    let err = patterns::classify_from_text(1, "TypeError: fetch failed");
    assert!(
        matches!(err, AgentError::NetworkError { .. }),
        "fetch failed must be NetworkError, got: {err:?}"
    );
}

#[test]
fn classify_from_text_detects_econnreset() {
    let err = patterns::classify_from_text(1, "read ECONNRESET");
    assert!(
        matches!(err, AgentError::NetworkError { .. }),
        "ECONNRESET must be NetworkError, got: {err:?}"
    );
}

#[test]
fn classify_from_text_detects_stalled_mid_stream() {
    // Regression test for issue #3541: kimi's "Response stalled mid-stream" must be
    // classified as NetworkError, not Unknown/AgentFailed.
    let err = patterns::classify_from_text(
        1,
        "API Error: Response stalled mid-stream. The response above may be incomplete.",
    );
    assert!(
        matches!(err, AgentError::NetworkError { .. }),
        "stalled mid-stream must be NetworkError, got: {err:?}"
    );
}

#[test]
fn classify_from_text_detects_connection_closed_mid_response() {
    // Regression test for issue #3541: claude's "Connection closed mid-response" must be
    // classified as NetworkError, not Unknown/AgentFailed.
    let err = patterns::classify_from_text(
        1,
        "API Error: Connection closed mid-response. The response above may be incomplete.",
    );
    assert!(
        matches!(err, AgentError::NetworkError { .. }),
        "connection closed mid-response must be NetworkError, got: {err:?}"
    );
}

#[test]
fn synthesize_response_marks_done_for_plain_text_no_op() {
    let response = synthesize_response_from_text(
        "No open positions, no trade executions, no conditions change. Full cash, holding.",
    )
    .unwrap();

    assert_eq!(response.status, "done");
    assert_eq!(
        response.summary,
        "No open positions, no trade executions, no conditions change. Full cash, holding."
    );
    assert!(response.error.is_none());
}

#[test]
fn synthesize_response_marks_needs_review_for_plain_text_error() {
    let response =
        synthesize_response_from_text("Failed to update branch: permission denied").unwrap();

    assert_eq!(response.status, "needs_review");
    assert_eq!(
        response.error.as_deref(),
        Some("agent returned plain text instead of JSON")
    );
}

#[test]
fn synthesize_response_rejects_empty_text() {
    assert!(synthesize_response_from_text("   \n\t  ").is_none());
}

/// Regression: raw NDJSON event streams (opencode/claude/codex stdout)
/// must be rejected by the synthesizer. Without the guard, the
/// "completed" substring inside tool state JSON triggers a false "done"
/// classification and 500 bytes of raw NDJSON leak into the
/// notification summary.
#[test]
fn synthesize_response_rejects_ndjson_event_stream() {
    let ndjson = r#"{"type":"step_start","timestamp":1,"part":{"type":"step-start"}}
{"type":"tool_use","timestamp":2,"name":"bash","input":{"cmd":"echo completed"}}
{"type":"step_finish","timestamp":3,"part":{"type":"step-finish","reason":"stop"}}"#;
    assert!(
        synthesize_response_from_text(ndjson).is_none(),
        "raw NDJSON must not be interpreted as agent prose"
    );

    let single = r#"{"type":"step_finish","reason":"stop","completed":true}"#;
    assert!(
        synthesize_response_from_text(single).is_none(),
        "single-line NDJSON event must also be rejected"
    );
}

#[test]
fn synthesize_response_accepts_json_without_event_type() {
    // A JSON-like blob that does NOT match a known NDJSON event type
    // should still be allowed through to the regular synthesis path
    // (so the guard does not over-reach).
    let text = "The task is complete. Filed issue #1234 successfully.";
    let response = synthesize_response_from_text(text).expect("plain prose should synthesize");
    assert_eq!(response.status, "done");
}

// Regression: opencode/nemotron-3-super-free and similar models sometimes exit
// with code 0 after emitting only exploratory or startup text (e.g. "Let's go!",
// planning prose) without completing any work.  The synthesis must return None for
// such ambiguous text so the runner treats the run as an invalid response and
// reroutes the task instead of falsely advancing it to needs_review / in_review.
// (Issues #2404, #2653, #2666)
#[test]
fn synthesize_response_returns_none_for_ambiguous_startup_text() {
    // Short greeting — not an error, not done, not an explicit negation.
    assert!(
        synthesize_response_from_text("Let's go!").is_none(),
        "startup greeting must not synthesize as needs_review"
    );
    // Exploratory planning prose — agent is thinking out loud, not reporting completion.
    assert!(
        synthesize_response_from_text(
            "I need to find where the stream command is implemented. \
                 Looking at the CLI structure, it seems like the stream functionality \
                 might be in the task module or another file. Let me check the CLI \
                 command structure. First, let me look at the task module since stream \
                 is related to tasks."
        )
        .is_none(),
        "exploratory prose must not synthesize as needs_review"
    );
}

#[test]
fn synthesize_response_does_not_mark_done_for_incomplete() {
    let response = synthesize_response_from_text("The task is incomplete").unwrap();
    assert_eq!(
        response.status, "needs_review",
        "\"incomplete\" should not match \"complete\""
    );
}

#[test]
fn synthesize_response_does_not_mark_done_for_not_done() {
    let response = synthesize_response_from_text("Changes are not done yet").unwrap();
    assert_eq!(
        response.status, "needs_review",
        "\"not done\" should not match bare \"done\""
    );
}

#[test]
fn synthesize_response_does_not_mark_done_for_not_complete() {
    let response = synthesize_response_from_text("Still in progress, not complete").unwrap();
    assert_eq!(
        response.status, "needs_review",
        "\"not complete\" should not match \"complete\""
    );
}

#[test]
fn synthesize_response_does_not_mark_done_for_not_yet_completed() {
    let response = synthesize_response_from_text("The task is not yet completed").unwrap();
    assert_eq!(
        response.status, "needs_review",
        "\"not yet completed\" should not match \"completed\""
    );
}

#[test]
fn synthesize_response_marks_done_for_completed() {
    let response = synthesize_response_from_text("Task has been completed successfully").unwrap();
    assert_eq!(response.status, "done");
}

#[test]
fn synthesize_response_marks_done_for_word_boundary_done() {
    let response = synthesize_response_from_text("All changes are done.").unwrap();
    assert_eq!(response.status, "done");
}

#[test]
fn synthesize_response_marks_done_for_issue_creation_text() {
    let response =
        synthesize_response_from_text("Filed 3 high-value GitHub issues: #1037, #1038, #1039")
            .unwrap();

    assert_eq!(response.status, "done");
    assert!(response.error.is_none());
    assert!(
        response
            .summary
            .contains("Filed 3 high-value GitHub issues")
    );
}

#[test]
fn synthesize_response_marks_done_for_committed() {
    let response =
        synthesize_response_from_text("I've committed the changes to the repository").unwrap();
    assert_eq!(response.status, "done");
}

#[test]
fn synthesize_response_rejects_exploratory_prose() {
    // Regression test: ensure exploratory analysis that mentions "let me check"
    // or planning language is NOT synthesized into a "done" completion.
    // With the ambiguous-text fix, this text correctly returns None (treated as
    // an invalid response / reroute signal) rather than synthesizing needs_review.
    // Crucially, it must NOT be marked "done".
    let exploratory = "I explored the codebase and found several .expect()/.unwrap() uses.\n\nLet me check each occurrence directly and run the tests to be sure. I may commit fixes afterwards.";
    match synthesize_response_from_text(exploratory) {
        None => {
            // Ambiguous exploratory prose correctly returns None — not marked done.
        }
        Some(resp) => {
            assert_ne!(
                resp.status, "done",
                "Exploratory prose must not be auto-marked done"
            );
        }
    }
}

#[test]
fn synthesize_response_marks_done_for_commit_created() {
    let response = synthesize_response_from_text(
        "Commit created locally. The push is being blocked by permissions.",
    )
    .unwrap();
    assert_eq!(response.status, "done");
}

#[test]
fn synthesize_response_does_not_mark_done_for_not_committed() {
    let response = synthesize_response_from_text("The changes are not committed yet").unwrap();
    assert_eq!(
        response.status, "needs_review",
        "\"not committed\" should not match \"committed\""
    );
}

// ── Regression: #1362/#1363 — agent text misclassified as needs_review ──

#[test]
fn synthesize_response_marks_done_for_fix_is_complete() {
    // Exact agent output from task 30203 (issue #1363).
    // "The fix is complete" was misclassified as needs_review because
    // the pattern list had "completed" but not "complete".
    let response = synthesize_response_from_text(
        "All 73 cooldown tests pass. The fix is complete.\n\n\
             **Summary of changes:**\n\n\
             1. **`src/store/kv.rs`** — Added `kv_increment()` method that uses a single \
             `INSERT … ON CONFLICT … DO UPDATE … RETURNING` SQL statement.",
    )
    .unwrap();
    assert_eq!(
        response.status, "done",
        "\"The fix is complete\" must be classified as done, not needs_review"
    );
}

#[test]
fn synthesize_response_marks_done_for_tests_pass_fix_working() {
    // Exact agent output from task 30204 (issue #1362).
    // "All tests pass. The fix is clean and working." was misclassified
    // as needs_review because none of the looks_done patterns matched.
    let response = synthesize_response_from_text(
        "All tests pass. The fix is clean and working.\n\n\
             **Summary of changes** to `src/engine/runner/response.rs:37`:\n\n\
             - **Before:** `capped + jitter` → range `[capped, 1.6*capped]`\n\
             - **After:** `capped.saturating_sub(jitter_range) + jitter` → range `[0.7*capped, 1.3*capped]`",
    )
    .unwrap();
    assert_eq!(
        response.status, "done",
        "\"All tests pass. The fix is clean and working.\" must be classified as done"
    );
}

// ── parse_ndjson edge cases ───────────────────────────────────

#[test]
fn parse_ndjson_handles_standard_lines() {
    let raw = r#"{"type":"text","text":"hello"}
{"type":"text","text":"world"}"#;
    let events = parse_ndjson(raw);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].get("type").unwrap().as_str(), Some("text"));
    assert_eq!(events[1].get("type").unwrap().as_str(), Some("text"));
}

#[test]
fn parse_ndjson_skips_empty_lines() {
    let raw = r#"{"type":"text"}

{"type":"text"}
"#;
    let events = parse_ndjson(raw);
    assert_eq!(events.len(), 2);
}

#[test]
fn parse_ndjson_skips_malformed_lines() {
    let raw = r#"{"type":"valid"}
not json at all
{"type":"also_valid"}"#;
    let events = parse_ndjson(raw);
    assert_eq!(events.len(), 2);
}

#[test]
fn parse_ndjson_fallback_extracts_embedded_json() {
    // Edge case: JSON with closing fence inside string field
    let raw = r#"text: {"text": "here is the result: ```json\n{\"status\":\"done\"}\n```"} more"#;
    let events = parse_ndjson(raw);
    // Should have extracted the embedded JSON object
    assert!(
        !events.is_empty(),
        "should extract JSON objects via fallback"
    );
}

#[test]
fn parse_ndjson_fallback_handles_fragmented_objects() {
    // Edge case: JSON object spread across multiple lines (fragmented NDJSON)
    let raw = r#"{"type": "text
", "content": "hello
world"}"#;
    let events = parse_ndjson(raw);
    // Should still extract the JSON via fallback
    assert!(!events.is_empty(), "should handle fragmented JSON");
}

// ── PermissionRules defaults ────────────────────────────────

#[test]
fn permission_rules_default_is_autonomous() {
    let rules = PermissionRules::default();
    assert!(rules.autonomous);
}

#[test]
fn permission_rules_default_sandbox_is_workspace_write() {
    let rules = PermissionRules::default();
    assert_eq!(rules.sandbox, SandboxLevel::WorkspaceWrite);
}

#[test]
fn permission_rules_default_disallows_rm() {
    let rules = PermissionRules::default();
    assert!(rules.disallowed_tools.contains(&"Bash(rm *)".to_string()));
    assert!(rules.disallowed_tools.contains(&"Bash(rm -*)".to_string()));
}

#[test]
fn permission_rules_default_no_allowed_edit_paths() {
    let rules = PermissionRules::default();
    assert!(
        rules.allowed_edit_paths.is_empty(),
        "{:?}",
        rules.allowed_edit_paths
    );
}

// ── Permission translation across all agents ────────────────

/// Test that all agents handle the default permission rules consistently.
#[test]
fn all_agents_handle_default_permissions() {
    let perms = PermissionRules::default();
    let sys = "/tmp/sys.md";
    let msg = "/tmp/msg.md";

    // Claude: should have bypassPermissions and rm disallowed
    let claude = get_runner("claude");
    let cmd = claude.build_command(None, "", sys, msg, &perms);
    assert!(
        cmd.contains("--permission-mode bypassPermissions"),
        "claude default: expected bypassPermissions, got: {cmd}"
    );
    assert!(
        cmd.contains("Bash(rm *)"),
        "claude default: expected rm disallowed, got: {cmd}"
    );

    // Codex: autonomous + workspace-write → --sandbox workspace-write -c 'approval_policy="never"'
    let codex = get_runner("codex");
    let cmd = codex.build_command(None, "", sys, msg, &perms);
    assert!(
        cmd.contains("--sandbox workspace-write"),
        "codex default: expected --sandbox workspace-write, got: {cmd}"
    );
    assert!(
        cmd.contains(r#"-c 'approval_policy="never"'"#),
        "codex default: expected -c approval_policy=never, got: {cmd}"
    );
    assert!(
        !cmd.contains("--ask-for-approval"),
        "codex default: --ask-for-approval was removed in 0.133.0 and must not appear, got: {cmd}"
    );
    assert!(
        !cmd.contains("--full-auto"),
        "codex default: --full-auto is deprecated and must not appear, got: {cmd}"
    );

    // OpenCode: autonomous dispatch passes --dangerously-skip-permissions
    // and writes no config file (relies on opencode's built-in flag).
    let opencode = get_runner("opencode");
    let cmd = opencode.build_command(None, "", sys, msg, &perms);
    assert!(
        cmd.contains("--dangerously-skip-permissions"),
        "opencode default: should pass --dangerously-skip-permissions, got: {cmd}"
    );
    assert!(
        !cmd.contains("opencode.json"),
        "opencode default: should not write a config file, got: {cmd}"
    );
    assert!(
        !cmd.contains("XDG_CONFIG_HOME"),
        "opencode default: should not override XDG_CONFIG_HOME, got: {cmd}"
    );
    assert!(
        !cmd.contains(".orch-opencode"),
        "opencode default: should not reference .orch-opencode, got: {cmd}"
    );
}

/// Test supervised mode translates correctly for each agent.
#[test]
fn all_agents_handle_supervised_mode() {
    let perms = PermissionRules {
        autonomous: false,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec![],
        allowed_tools: vec![],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let sys = "/tmp/sys.md";
    let msg = "/tmp/msg.md";

    // Claude: supervised → acceptEdits
    let claude = get_runner("claude");
    let cmd = claude.build_command(None, "", sys, msg, &perms);
    assert!(
        cmd.contains("--permission-mode acceptEdits"),
        "claude supervised: expected acceptEdits, got: {cmd}"
    );

    // Codex: supervised → on-request
    let codex = get_runner("codex");
    let cmd = codex.build_command(None, "", sys, msg, &perms);
    assert!(
        cmd.contains(r#"-c 'approval_policy="on-request"'"#),
        "codex supervised: expected -c approval_policy=on-request, got: {cmd}"
    );
    assert!(
        !cmd.contains("--ask-for-approval"),
        "codex supervised: --ask-for-approval was removed in 0.133.0 and must not appear, got: {cmd}"
    );
}

/// Test full access sandbox translates correctly for each agent.
#[test]
fn all_agents_handle_full_access_sandbox() {
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::FullAccess,
        disallowed_tools: vec![],
        allowed_tools: vec![],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let sys = "/tmp/sys.md";
    let msg = "/tmp/msg.md";

    // Claude: ignores sandbox level (no --sandbox flag)
    let claude = get_runner("claude");
    let cmd = claude.build_command(None, "", sys, msg, &perms);
    assert!(
        !cmd.contains("--sandbox"),
        "claude full_access: should have no sandbox flag, got: {cmd}"
    );

    // Codex: full access → --dangerously-bypass-approvals-and-sandbox
    let codex = get_runner("codex");
    let cmd = codex.build_command(None, "", sys, msg, &perms);
    assert!(
        cmd.contains("--dangerously-bypass-approvals-and-sandbox"),
        "codex full_access: expected dangerously-bypass, got: {cmd}"
    );
}

/// SandboxLevel::None with autonomous uses --sandbox workspace-write (same as WorkspaceWrite).
#[test]
fn codex_sandbox_none_uses_workspace_write() {
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::None,
        disallowed_tools: vec![],
        allowed_tools: vec![],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let codex = get_runner("codex");
    let cmd = codex.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
    assert!(
        cmd.contains("--sandbox workspace-write"),
        "codex sandbox::none + autonomous: should use --sandbox workspace-write, got: {cmd}"
    );
    assert!(
        cmd.contains(r#"-c 'approval_policy="never"'"#),
        "codex sandbox::none + autonomous: should use -c approval_policy=never, got: {cmd}"
    );
    assert!(
        !cmd.contains("--ask-for-approval"),
        "codex sandbox::none: --ask-for-approval was removed in 0.133.0, got: {cmd}"
    );
    assert!(
        !cmd.contains("--full-auto"),
        "codex sandbox::none: --full-auto is deprecated and must not appear, got: {cmd}"
    );
}

/// Test that allowed_edit_paths scopes Edit/Write when used with allowed_tools.
#[test]
fn claude_allowed_edit_paths_scope_edit_write() {
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec![],
        allowed_tools: vec![
            "Edit".to_string(),
            "Write".to_string(),
            "Read".to_string(),
            "Bash".to_string(),
        ],
        allowed_edit_paths: vec![PathBuf::from("/home/user/worktree")],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let claude = get_runner("claude");
    let cmd = claude.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);

    // Edit/Write should be scoped to worktree path
    assert!(
        cmd.contains("Edit(/home/user/worktree/*)"),
        "missing scoped Edit, got: {cmd}"
    );
    assert!(
        cmd.contains("Write(/home/user/worktree/*)"),
        "missing scoped Write, got: {cmd}"
    );
    // Read should be unrestricted
    assert!(cmd.contains("Read"), "missing Read");
}

/// Test that blocked paths don't affect Codex (it uses sandbox isolation).
#[test]
fn codex_ignores_allowed_edit_paths() {
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec![],
        allowed_tools: vec![],
        allowed_edit_paths: vec![PathBuf::from("/home/user/project")],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let codex = get_runner("codex");
    let cmd = codex.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
    assert!(
        !cmd.contains("/home/user/project"),
        "codex: should not reference blocked paths, got: {cmd}"
    );
}

/// Test that blocked paths don't affect OpenCode.
#[test]
fn opencode_ignores_allowed_edit_paths() {
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec![],
        allowed_tools: vec![],
        allowed_edit_paths: vec![PathBuf::from("/home/user/project")],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let opencode = get_runner("opencode");
    let cmd = opencode.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
    assert!(
        !cmd.contains("/home/user/project"),
        "opencode: should not reference blocked paths, got: {cmd}"
    );
}

/// Test that disallowed tools merge correctly for Claude (defaults + custom).
#[test]
fn claude_merges_disallowed_tools() {
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec![
            "Bash(rm *)".to_string(),
            "Bash(sudo *)".to_string(),
            "WebFetch".to_string(),
        ],
        allowed_tools: vec![],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let claude = get_runner("claude");
    let cmd = claude.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
    assert!(cmd.contains("Bash(rm *)"), "missing rm disallow");
    assert!(cmd.contains("Bash(sudo *)"), "missing sudo disallow");
    assert!(cmd.contains("WebFetch"), "missing WebFetch disallow");
}

/// Test that Codex ignores disallowed tools (has no such flag).
#[test]
fn codex_ignores_disallowed_tools() {
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec!["Bash(rm *)".to_string(), "WebFetch".to_string()],
        allowed_tools: vec![],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let codex = get_runner("codex");
    let cmd = codex.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
    assert!(
        !cmd.contains("disallow"),
        "codex: should not have disallow flags, got: {cmd}"
    );
}

/// Test Claude with empty disallowed tools produces no --disallowedTools flag.
#[test]
fn claude_no_disallowed_flag_when_empty() {
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec![],
        allowed_tools: vec![],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let claude = get_runner("claude");
    let cmd = claude.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
    assert!(
        !cmd.contains("--disallowedTools"),
        "claude: should have no disallowed flag when empty, got: {cmd}"
    );
}

/// Test Kimi/MiniMax aliases inherit Claude permission translation.
#[test]
fn kimi_minimax_use_claude_permissions() {
    let perms = PermissionRules {
        autonomous: false,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec!["Bash(rm *)".to_string()],
        allowed_tools: vec![],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let sys = "/tmp/sys.md";
    let msg = "/tmp/msg.md";

    for agent in &["kimi", "minimax"] {
        let runner = get_runner(agent);
        let cmd = runner.build_command(None, "", sys, msg, &perms);
        assert!(
            cmd.contains("--permission-mode acceptEdits"),
            "{agent}: expected acceptEdits"
        );
        assert!(
            cmd.contains("Bash(rm *)"),
            "{agent}: expected rm disallowed"
        );
    }
}

/// Test kimi/minimax get --allowedTools when allowed_tools is set.
#[test]
fn kimi_minimax_use_allowed_tools() {
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec![],
        allowed_tools: vec!["Edit".to_string(), "Bash".to_string(), "git".to_string()],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };

    for agent in &["kimi", "minimax"] {
        let runner = get_runner(agent);
        let cmd = runner.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
        assert!(
            cmd.contains("--allowedTools"),
            "{agent}: expected --allowedTools, got: {cmd}"
        );
        assert!(
            cmd.contains("Bash(git *)"),
            "{agent}: expected Bash(git *) pattern, got: {cmd}"
        );
    }
}

// ── Allowed tools across agents ─────────────────────────────

/// Test that allowed_tools generates --allowedTools for Claude.
#[test]
fn claude_allowed_tools_generates_allowedtools_flag() {
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec!["Bash(rm *)".to_string()],
        allowed_tools: vec![
            "Edit".to_string(),
            "Write".to_string(),
            "Read".to_string(),
            "Bash".to_string(),
            "git".to_string(),
        ],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };

    let claude = get_runner("claude");
    let cmd = claude.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
    assert!(
        cmd.contains("--allowedTools"),
        "expected --allowedTools, got: {cmd}"
    );
    assert!(
        !cmd.contains("--disallowedTools"),
        "should not have --disallowedTools when allowed_tools is set"
    );
}

/// Test that codex/opencode ignore allowed_tools (no equivalent flag).
#[test]
fn codex_opencode_ignore_allowed_tools() {
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec![],
        allowed_tools: vec!["Edit".to_string(), "git".to_string()],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };

    let codex = get_runner("codex");
    let cmd = codex.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
    assert!(
        !cmd.contains("--allowedTools"),
        "codex should not have --allowedTools, got: {cmd}"
    );

    let opencode = get_runner("opencode");
    let cmd = opencode.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
    assert!(
        !cmd.contains("--allowedTools"),
        "opencode should not have --allowedTools, got: {cmd}"
    );
}

// ── Integration: from_config() → agent translation ──────────

/// Integration test: verify from_config() loads real config and agents
/// translate it correctly. Requires ~/.orch/config.yml to exist.
#[test]
#[ignore = "reads the operator config"]
fn integration_from_config_to_agent_commands() {
    let perms = PermissionRules::from_config();

    // Verify defaults are reasonable
    assert!(
        perms.autonomous,
        "from_config: expected autonomous=true by default"
    );

    // Verify each agent can build a command from the config-loaded rules
    for agent_name in &["claude", "codex", "opencode", "kimi", "minimax"] {
        let runner = get_runner(agent_name);
        let cmd = runner.build_command(None, "timeout 1800", "/tmp/sys.md", "/tmp/msg.md", &perms);
        assert!(
            !cmd.is_empty(),
            "{agent_name}: build_command returned empty string"
        );
        assert!(
            cmd.contains(agent_name) || *agent_name == "kimi" || *agent_name == "minimax",
            "{agent_name}: command should reference agent binary, got: {cmd}"
        );
    }
}

/// Integration test: verify from_config() handles supervised mode.
/// Requires config with workflow.permissions.mode = "supervised".
#[test]
#[ignore = "reads the operator config"]
fn integration_supervised_config_translates_correctly() {
    // This test requires config to have:
    // workflow:
    //   permissions:
    //     mode: supervised
    let perms = PermissionRules::from_config();

    let claude = get_runner("claude");
    let cmd = claude.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);

    if perms.autonomous {
        assert!(
            cmd.contains("bypassPermissions"),
            "autonomous config → bypassPermissions"
        );
    } else {
        assert!(
            cmd.contains("acceptEdits"),
            "supervised config → acceptEdits"
        );
    }

    let codex = get_runner("codex");
    let cmd = codex.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);

    if perms.autonomous {
        // autonomous codex uses --sandbox workspace-write -c 'approval_policy="never"'
        assert!(
            cmd.contains("--sandbox workspace-write"),
            "autonomous config → --sandbox workspace-write, got: {cmd}"
        );
        assert!(
            cmd.contains(r#"-c 'approval_policy="never"'"#),
            "autonomous config → -c approval_policy=never, got: {cmd}"
        );
    } else {
        assert!(
            cmd.contains(r#"-c 'approval_policy="on-request"'"#),
            "supervised config → -c approval_policy=on-request, got: {cmd}"
        );
    }
    assert!(
        !cmd.contains("--ask-for-approval"),
        "--ask-for-approval was removed in codex 0.133.0 and must not appear, got: {cmd}"
    );
}

// ── deny_read_only preset ───────────────────────────────────

/// Test that deny_read_only() sets the right disallowed tools for control sessions.
#[test]
fn deny_read_only_blocks_reading_tools() {
    let perms = PermissionRules::deny_read_only();

    let read_tools = [
        "Bash(ls *)",
        "Bash(ls)",
        "Bash(find *)",
        "Bash(cat *)",
        "Bash(head *)",
        "Bash(tail *)",
        "Bash(fzf *)",
        "Bash(less *)",
        "Bash(more *)",
        "Read",
        "Glob",
        "Grep",
    ];

    for tool in read_tools {
        assert!(
            perms.disallowed_tools.contains(&tool.to_string()),
            "deny_read_only should block {tool}, got: {:?}",
            perms.disallowed_tools
        );
    }
}

/// Test that deny_read_only() produces correct Claude command for control session.
#[test]
fn deny_read_only_claude_command() {
    let perms = PermissionRules::deny_read_only();
    let claude = get_runner("claude");
    let cmd = claude.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);

    // Should contain Read, Glob, Grep disallow patterns
    assert!(
        cmd.contains("Read"),
        "should block Read for control session"
    );
    assert!(
        cmd.contains("Glob"),
        "should block Glob for control session"
    );
    assert!(
        cmd.contains("Grep"),
        "should block Grep for control session"
    );
    // Should block ls
    assert!(
        cmd.contains("Bash(ls") || cmd.contains("Bash(ls *)"),
        "should block ls"
    );
    // Should still have bypassPermissions (autonomous)
    assert!(
        cmd.contains("--permission-mode bypassPermissions"),
        "should be autonomous (bypassPermissions)"
    );
}

// --- synthesize_response_from_text structured extraction ---

#[test]
fn synthesize_extracts_file_paths() {
    let text =
        "Fixed the issue by modifying src/engine/runner/agents/mod.rs and tests/integration.rs.";
    let resp = synthesize_response_from_text(text).unwrap();
    assert!(
        resp.files
            .contains(&"src/engine/runner/agents/mod.rs".to_string()),
        "expected src/engine/runner/agents/mod.rs in files, got: {:?}",
        resp.files
    );
    assert!(
        resp.files.contains(&"tests/integration.rs".to_string()),
        "expected tests/integration.rs in files, got: {:?}",
        resp.files
    );
}

#[test]
fn synthesize_extracts_accomplished_bullets() {
    let text = "All tests pass.\n- Added logging to src/foo.rs\n- Updated migration file";
    let resp = synthesize_response_from_text(text).unwrap();
    assert_eq!(resp.status, "done");
    assert!(
        resp.accomplished
            .iter()
            .any(|a| a.contains("Added logging")),
        "expected accomplished bullet, got: {:?}",
        resp.accomplished
    );
    assert!(
        resp.accomplished
            .iter()
            .any(|a| a.contains("Updated migration")),
        "expected accomplished bullet, got: {:?}",
        resp.accomplished
    );
}

#[test]
fn synthesize_extracts_remaining_section() {
    let text = "Completed the main fix.\n- Patched src/lib.rs\n\nRemaining:\n- Write unit tests\n- Update docs";
    let resp = synthesize_response_from_text(text).unwrap();
    assert!(
        resp.remaining
            .iter()
            .any(|r| r.contains("Write unit tests")),
        "expected remaining item, got: {:?}",
        resp.remaining
    );
    assert!(
        resp.remaining.iter().any(|r| r.contains("Update docs")),
        "expected remaining item, got: {:?}",
        resp.remaining
    );
    assert!(
        resp.accomplished.iter().any(|a| a.contains("Patched")),
        "expected accomplished item, got: {:?}",
        resp.accomplished
    );
}

#[test]
fn synthesize_fixed_phrase_is_done() {
    let text = "Fixed. The bug no longer reproduces.";
    let resp = synthesize_response_from_text(text).unwrap();
    assert_eq!(resp.status, "done", "expected done for 'Fixed.' text");
}

// ── detect_rate_limit false-positive guard ────────────────────────────
// Regression: NDJSON telemetry blobs can contain "429" as a token count
// (e.g. "outputTokens":429). `detect_rate_limit` should still flag those
// because it cannot distinguish context without the terminal_reason guard.
// The guard lives in review.rs, but here we verify the raw detector
// behaviour so callers know they must apply the guard themselves.
#[test]
fn detect_rate_limit_fires_on_bare_429_in_token_count() {
    // This SHOULD return Some — the generic detector has no context.
    // The caller (review.rs) is responsible for suppressing it when
    // terminal_reason:completed is present.
    let payload =
        r#"{"type":"result","terminal_reason":"completed","outputTokens":429,"is_error":false}"#;
    assert!(
        patterns::detect_rate_limit(payload).is_some(),
        "detect_rate_limit must still fire on bare 429 so callers apply the guard"
    );
}

#[test]
fn detect_rate_limit_ignores_429_inside_longer_number() {
    let uuid = r#"{"type":"hook","uuid":"a1b2c3d4-4295-4e5f-8a9b-0c1d2e3f4a5b"}"#;
    assert!(
        patterns::detect_rate_limit(uuid).is_none(),
        "429 inside a UUID digit run must not match"
    );
}

#[test]
fn detect_rate_limit_ignores_429_in_uuid_hex_group() {
    let hook = r#"{"type":"system","subtype":"hook_response","hook_id":"18db61b6-7b72-429c-8279-fae3089bf565"}"#;
    assert!(patterns::detect_rate_limit(hook).is_none());
}

#[test]
fn detect_rate_limit_429_message_comes_from_matched_line() {
    let filler = "unrelated agent prose that is long enough to fill the window. ".repeat(20);
    let text = format!("HTTP 429 Too Many Requests\n{filler}");
    match patterns::detect_rate_limit(&text) {
        Some(AgentError::RateLimit { message }) => assert!(message.contains("429")),
        other => panic!("expected RateLimit, got {other:?}"),
    }
}

// Verify the guard logic that review.rs uses: when raw NDJSON has
// terminal_reason:completed without is_error:true, the result of
// detect_rate_limit should be suppressed.
#[test]
fn terminal_reason_completed_guard_suppresses_false_rate_limit() {
    let raw_output =
        r#"{"type":"result","terminal_reason":"completed","outputTokens":429,"is_error":false}"#;
    let ndjson_completed = raw_output.contains("\"terminal_reason\":\"completed\"")
        && !raw_output.contains("\"is_error\":true");
    // Apply the same guard that review.rs now uses.
    let detected = (!ndjson_completed)
        .then(|| patterns::detect_rate_limit(raw_output))
        .flatten();
    assert!(
        detected.is_none(),
        "guard must suppress rate-limit false positive from token-count 429 when terminal_reason:completed"
    );
}

// Verify the guard does NOT suppress a real rate-limit error.
#[test]
fn terminal_reason_completed_guard_passes_real_rate_limit() {
    // Real rate-limit output has no terminal_reason:completed.
    let raw_output = "Error: 429 Too Many Requests — you have exceeded your quota";
    let ndjson_completed = raw_output.contains("\"terminal_reason\":\"completed\"")
        && !raw_output.contains("\"is_error\":true");
    let detected = (!ndjson_completed)
        .then(|| patterns::detect_rate_limit(raw_output))
        .flatten();
    assert!(
        detected.is_some(),
        "guard must NOT suppress a real rate-limit error"
    );
}

#[test]
fn synthesize_no_false_positive_file_paths() {
    // URLs and version strings should not appear in files list.
    // The text is ambiguous (no error, no done signal, no explicit negation) so
    // synthesize_response_from_text returns None — no file extraction happens.
    let text = "See https://example.com/foo.rs for details. Version 1.2.3 is fine.";
    match synthesize_response_from_text(text) {
        None => {} // Ambiguous text correctly returns None — no files extracted.
        Some(resp) => {
            assert!(
                resp.files.is_empty(),
                "expected no file paths extracted from URL text, got: {:?}",
                resp.files
            );
        }
    }
}

// Regression: issue #3197 — CLI arg-parser diagnostics were being synthesized as
// agent responses. When a process (e.g. codex) fails with an invalid CLI flag, it
// exits non-zero with a clap/help diagnostic on stderr. This should be rejected
// (return None) rather than synthesized as a "needs_review" response.
#[test]
fn synthesize_rejects_cli_parser_diagnostics_unexpected_argument() {
    // Real clap error from the bug report (codex exit 141)
    let text = "error: unexpected argument '--ask-for-approval' found\n  \
                    tip: to pass '--ask-for-approval' as a value, use '-- --ask-for-approval'\n\
                    Usage: codex exec [OPTIONS] [PROMPT]";
    assert!(
        synthesize_response_from_text(text).is_none(),
        "CLI parser diagnostic must not synthesize as agent response"
    );
}

#[test]
fn synthesize_rejects_cli_parser_diagnostics_usage_line() {
    let text = "Usage: mycommand [OPTIONS] <arg>";
    assert!(
        synthesize_response_from_text(text).is_none(),
        "Usage line must not synthesize as agent response"
    );
}

#[test]
fn synthesize_rejects_cli_parser_diagnostics_help_hint() {
    let text = "Error: unknown flag --foo\nFor more information, try '--help'";
    assert!(
        synthesize_response_from_text(text).is_none(),
        "Help hint must not synthesize as agent response"
    );
}

#[test]
fn synthesize_rejects_cli_parser_diagnostics_tip() {
    let text = "error: bad value\ntip: to pass '--flag' as a value, use '-- --flag'";
    assert!(
        synthesize_response_from_text(text).is_none(),
        "Clap tip must not synthesize as agent response"
    );
}

#[test]
fn synthesize_accepts_agent_error_messages_that_mention_error() {
    // Agent returning plain text that happens to contain "error:" but is an actual
    // agent message (e.g. summarizing what went wrong) should still synthesize.
    let text = "I tried to run the command but got an error: permission denied on that file.";
    let response =
        synthesize_response_from_text(text).expect("agent error message should synthesize");
    assert_eq!(response.status, "needs_review");
    assert!(response.error.is_some());
}

// Regression: Issue #3197 — exact clap error from bug report.
// When codex fails with an invalid CLI flag (e.g., --ask-for-approval with old codex versions),
// it should return None, not synthesize as "needs_review". Without this fix, the run
// would be marked as success and advance to in_review, wasting resources.
#[test]
fn synthesize_rejects_exact_codex_cli_error() {
    let stderr_from_codex = "error: unexpected argument '--ask-for-approval' found\n  \
                                 tip: to pass '--ask-for-approval' as a value, use '-- --ask-for-approval'\n\
                                 Usage: codex exec [OPTIONS] [PROMPT]";
    assert!(
        synthesize_response_from_text(stderr_from_codex).is_none(),
        "exact codex CLI error from #3197 must not synthesize as agent response"
    );
}

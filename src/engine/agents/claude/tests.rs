use super::*;
use crate::cooldown;
use crate::engine::agents::fixture;

fn runner() -> ClaudeRunner {
    ClaudeRunner::new("claude")
}

#[test]
fn error_envelope_uses_errors_array_when_result_missing() {
    let raw = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["API error (status 402 Payment Required): usage balance exhausted"]}"#;
    match runner().parse_response(raw) {
        Err(AgentError::AgentFailed { message } | AgentError::Auth { message }) => {
            assert!(cooldown::detect_credit_exhaustion(&message).is_some());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn error_envelope_falls_back_to_subtype() {
    let raw = r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#;
    match runner().parse_response(raw) {
        Err(AgentError::AgentFailed { message }) => {
            assert_eq!(message, "error_during_execution");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn parse_claude_envelope_success() {
    let raw = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 2006,
            "result": "{\"status\":\"done\",\"summary\":\"hello\",\"accomplished\":[],\"remaining\":[],\"files\":[]}",
            "usage": {"input_tokens": 10, "output_tokens": 78},
            "permission_denials": []
        }"#;

    let parsed = runner().parse_response(raw).unwrap();
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.response.summary, "hello");
    assert_eq!(parsed.input_tokens, Some(10));
    assert_eq!(parsed.output_tokens, Some(78));
    assert_eq!(parsed.duration_ms, Some(2006));
}

#[test]
fn parse_claude_envelope_with_markdown_result() {
    let raw = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 5000,
            "result": "```json\n{\"status\": \"done\", \"summary\": \"fixed bug\"}\n```",
            "usage": {"input_tokens": 100, "output_tokens": 50},
            "permission_denials": ["Bash(rm *)"]
        }"#;

    let parsed = runner().parse_response(raw).unwrap();
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.response.summary, "fixed bug");
}

#[test]
fn parse_claude_envelope_error() {
    let raw = r#"{
            "type": "result",
            "subtype": "error",
            "is_error": true,
            "result": "credit balance too low",
            "usage": {},
            "permission_denials": []
        }"#;

    let err = runner().parse_response(raw).unwrap_err();
    assert!(matches!(err, AgentError::Auth { .. }));
}

#[test]
fn parse_claude_envelope_rate_limit_error() {
    let raw = r#"{
            "type": "result",
            "subtype": "error",
            "is_error": true,
            "result": "rate limit exceeded, please try again later",
            "usage": {},
            "permission_denials": []
        }"#;

    let err = runner().parse_response(raw).unwrap_err();
    assert!(matches!(err, AgentError::RateLimit { .. }));
}

#[test]
fn parse_envelope_no_false_positive_from_work_product_result() {
    // Regression test for issue #1292: when is_error=false but the result
    // text contains rate-limit keywords (agent describing work it did),
    // parse_response must NOT return a RateLimit error.
    //
    // Fix for issue #1377: when parser::parse fails on plain-text output,
    // synthesize_response_from_text is tried and tokens are preserved.
    let raw = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1000,"result":"Added detect_rate_limit and quota exceeded patterns","usage":{"input_tokens":10,"output_tokens":5}}"#.to_string();
    // Plain-text result with rate-limit keywords: parser::parse fails, but
    // synthesis succeeds — result must be Ok (tokens preserved) not RateLimit.
    let parsed = runner()
        .parse_response(&raw)
        .expect("plain-text work product must synthesize successfully");
    // Tokens must be preserved from the envelope (issue #1377)
    assert_eq!(
        parsed.input_tokens,
        Some(10),
        "input_tokens must be preserved"
    );
    assert_eq!(
        parsed.output_tokens,
        Some(5),
        "output_tokens must be preserved"
    );
}

#[test]
fn parse_direct_json_no_envelope() {
    let raw = r#"{"status":"done","summary":"hello","accomplished":[],"remaining":[],"files":[]}"#;

    let parsed = runner().parse_response(raw).unwrap();
    assert_eq!(parsed.response.status, "done");
    assert!(parsed.input_tokens.is_none());
}

#[test]
fn parse_empty_response() {
    let err = runner().parse_response("").unwrap_err();
    assert!(matches!(err, AgentError::InvalidResponse { .. }));
}

#[test]
fn classify_error_rate_limit() {
    let err = runner().classify_error(1, "", "429 Too Many Requests");
    assert!(matches!(err, AgentError::RateLimit { .. }));
}

#[test]
fn classify_error_detects_session_limit_from_mid_stream_system_event() {
    // Regression test for issue #3272: Claude session limit errors appear as
    // `type:system` NDJSON events mid-stream, not as `type:result` lines with
    // `is_error:true`. When significant work product exists before and after
    // the system event, it is outside both head and tail scan windows in
    // `classify_from_text`. The `classify_error` function must extract the
    // error message from the system event and prepend it to the combined text.
    let header_events = "{\"type\":\"conversation\",\"subtype\":\"init\"}\n\
             {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"I'll work on this task.\"}]}}\n"
        .repeat(80);
    let system_event = r#"{"type":"system","subtype":"error","error":{"type":"session_limit","message":"You've hit your session limit · resets 4:10am (America/Sao_Paulo)"}}"#;
    let work_product = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Here is the implementation...\"}]}}\n"
        .repeat(80);
    let stdout = format!("{header_events}{system_event}\n{work_product}");
    // Verify the system event is deep enough to miss head and tail scans.
    assert!(
        stdout.len() > 6000,
        "test stdout must exceed combined head+tail windows, got {} bytes",
        stdout.len()
    );
    let err = runner().classify_error(1, &stdout, "");
    assert!(
        matches!(err, AgentError::RateLimit { .. }),
        "session limit system event mid-stream must be detected, got: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("session limit"),
        "error message should contain the rate limit reason, got: {msg}"
    );
}

#[test]
fn classify_error_killed_run_with_429_in_uuid_hex_group_is_not_rate_limit() {
    // Regression for issue #3956: `-429c-` in a hook id labelled a SIGTERM run RateLimit
    let hook = r#"{"type":"system","subtype":"hook_response","hook_id":"18db61b6-7b72-429c-8279-fae3089bf565"}"#;
    let prose = "the first lazy solution that works is the right one.\n".repeat(80);
    let stdout = format!("{hook}\n{prose}");
    assert!(stdout.len() > 3000);
    for code in [143, 137, 1] {
        let err = runner().classify_error(code, &stdout, "");
        assert!(
            !matches!(err, AgentError::RateLimit { .. }),
            "exit {code} must not classify as RateLimit, got: {err:?}"
        );
    }
    let err = runner().classify_error(143, &stdout, "");
    assert!(
        matches!(err, AgentError::Interrupted { .. }),
        "got: {err:?}"
    );
}

#[test]
fn classify_error_402_envelope_not_masked_by_429_in_hook_uuid() {
    // Regression for issue #3760: a 4295 hook UUID in the first 3000 bytes
    // classified a 402 envelope as a rate limit whose message was hook prose
    let hook = r#"{"type":"system","subtype":"init","hook":"SessionStart","uuid":"a1b2c3d4-4295-4e5f-8a9b-0c1d2e3f4a5b","outputTokens":429}"#;
    let hook_prose = "on. The first lazy solution that works is the\nright one once you actually know what the change has to touch.\n\n**Build the smallest thing that can fail.**\n";
    let padding = hook_prose.repeat(40);
    let result_line = r#"{"type":"result","is_error":true,"terminal_reason":"api_error","api_error_status":402,"subtype":"success","result":"API Error: 402 insufficient balance (1008)"}"#;
    let stdout = format!("{hook}\n{padding}\n{result_line}\n");
    assert!(
        stdout.len() > 3000,
        "fixture must put the 4295 UUID inside the 3000-byte head, got {} bytes",
        stdout.len()
    );
    let err = runner().classify_error(1, &stdout, "");
    assert!(
        matches!(err, AgentError::Auth { .. }),
        "402 envelope must classify as Auth, got: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("insufficient balance"),
        "stored error must keep the 402 text, got: {msg}"
    );
}

#[test]
fn classify_error_prefers_clean_result_text_over_byte_truncated_tail() {
    // Regression test for issue #3577: a well-formed `type:result`
    // envelope with `is_error:true` was correctly extracted into
    // `error_result`, but `classify_from_text`'s generic Unknown
    // fallback then re-truncates the combined stdout+stderr to its last
    // 300 bytes — which, once enough preceding NDJSON pushes the result
    // line's start past that window, cuts it off mid-field-name (e.g.
    // `"is_error"` becomes `"i_error"`). The stored error ends up a
    // garbled JSON fragment instead of the clean message, and (in the
    // fallback.rs non-zero-exit path) skips the model cooldown that the
    // AgentFailed branch applies.
    let padding = "{\"type\":\"system\",\"subtype\":\"init\"}\n".repeat(50);
    let result_line = r#"{"is_error":true,"duration_api_ms":0,"num_turns":1,"stop_reason":"stop_sequence","session_id":"32a7c67c","total_cost_usd":0,"usage":{},"modelUsage":{},"permission_denials":[],"terminal_reason":"api_error","fast_mode_state":"off","fast_mode_disabled_reason":"sdk_opt_in_required","subtype":"success","api_error_status":null,"result":"Failed to authenticate: OAuth session expired and could not be refreshed","type":"result","duration_ms":141,"uuid":"fe0ccff2-54d3-47cd-b5ba-a82f99a72b45"}"#;
    let stdout = format!("{padding}{result_line}\n");
    // Sanity check: the message text itself is not caught by any of the
    // specific keyword detectors (auth/rate_limit/context_overflow), so
    // without the fix this must fall to the generic Unknown branch.
    assert!(
        stdout.len() > 300,
        "test stdout must exceed the tail-truncation window"
    );

    let err = runner().classify_error(1, &stdout, "");
    assert!(
        matches!(err, AgentError::AgentFailed { .. }),
        "expected AgentFailed with the clean message, got: {err:?}"
    );
    let msg = err.to_string();
    assert_eq!(
        msg,
        "agent failed: Failed to authenticate: OAuth session expired and could not be refreshed",
        "stored error must be the clean result text, not a byte-truncated JSON fragment"
    );
    assert!(
        !msg.contains("i_error"),
        "stored error must not contain the byte-truncated JSON fragment, got: {msg}"
    );
}

#[test]
fn classify_error_timeout() {
    let err = runner().classify_error(124, "", "");
    assert!(matches!(err, AgentError::Timeout { .. }));
}

#[test]
fn extract_usage_from_model_usage() {
    let response_json: serde_json::Value = serde_json::json!({
        "modelUsage": {
            "claude-haiku-4-5-20251001": {
                "inputTokens": 1000,
                "outputTokens": 500
            }
        }
    });
    let envelope = response_json.as_object().unwrap();
    let (input, output) = extract_usage(envelope);
    assert_eq!(input, Some(1000));
    assert_eq!(output, Some(500));
}

#[test]
fn build_command_skips_an_empty_model() {
    let r = runner();
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec![],
        allowed_tools: vec![],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    for m in ["", "  "] {
        let cmd = r.build_command(Some(m), "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
        assert!(!cmd.contains("--model"), "{cmd}");
    }
}

#[test]
fn build_command_with_model() {
    let r = runner();
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec!["Bash(rm *)".to_string()],
        allowed_tools: vec![],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let cmd = r.build_command(
        Some("opus"),
        "timeout 1800",
        "/tmp/sys.txt",
        "/tmp/msg.txt",
        &perms,
    );
    assert!(cmd.contains("--model 'opus'"));
    assert!(cmd.contains("claude -p"));
    assert!(cmd.contains("--output-format stream-json"));
    assert!(cmd.contains("--permission-mode bypassPermissions"));
    assert!(cmd.contains("--disallowedTools 'Bash(rm *)'"));
}

#[test]
fn build_command_with_effort() {
    let perms = PermissionRules::default();
    let cmd = runner().build_command_with_effort(
        Some("opus"),
        Some("high"),
        "",
        "/tmp/sys.txt",
        "/tmp/msg.txt",
        &perms,
    );
    assert!(cmd.contains("--model 'opus' --effort 'high'"), "{cmd}");
    let plain = runner().build_command(Some("opus"), "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
    assert!(!plain.contains("--effort"), "{plain}");
}

#[test]
fn build_command_supervised_mode() {
    let r = runner();
    let perms = PermissionRules {
        autonomous: false,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec![],
        allowed_tools: vec![],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let cmd = r.build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
    assert!(cmd.contains("--permission-mode acceptEdits"));
}

#[test]
fn a_read_only_chat_runs_its_allowlist_and_nothing_else() {
    let r = runner();
    let perms = PermissionRules::read_only(vec![
        "Read".to_string(),
        "Bash(orch task list:*)".to_string(),
    ]);
    let cmd = r.build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
    assert!(cmd.contains("--permission-mode dontAsk"), "{cmd}");
    assert!(
        cmd.contains("--restricted"),
        "the operator's settings stay out: {cmd}"
    );
    assert!(cmd.contains("--strict-mcp-config"), "no MCP server: {cmd}");
    assert!(
        cmd.contains("--tools 'Read,Bash'"),
        "only the listed kinds: {cmd}"
    );
    assert!(
        cmd.contains("--allowedTools 'Read,Bash(orch task list:*)'"),
        "the exact allowlist: {cmd}"
    );
    assert!(
        cmd.contains("--append-system-prompt-file \"/tmp/sys.txt\""),
        "the prompt's content, not its path: {cmd}"
    );
    assert!(!cmd.contains("bypassPermissions"), "{cmd}");
}

#[test]
fn build_command_allowed_edit_paths_scopes_edit_write() {
    let r = runner();
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
    let cmd = r.build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);

    // Edit/Write should be scoped to the worktree path
    assert!(
        cmd.contains("Edit(/home/user/worktree/*)"),
        "expected scoped Edit, got: {cmd}"
    );
    assert!(
        cmd.contains("Write(/home/user/worktree/*)"),
        "expected scoped Write, got: {cmd}"
    );

    // Read and Bash should not be scoped
    assert!(
        cmd.contains(",Read,") || cmd.contains("'Read,") || cmd.contains(",Read'"),
        "expected unrestricted Read, got: {cmd}"
    );
    assert!(
        cmd.contains(",Bash,") || cmd.contains("'Bash,") || cmd.contains(",Bash'"),
        "expected unrestricted Bash, got: {cmd}"
    );
}

#[test]
fn kimi_runner_name() {
    let r = ClaudeRunner::new("kimi");
    assert_eq!(r.name(), "kimi");
}

// ── Fixture-based tests ─────────────────────────────────────

#[test]
fn fixture_claude_success() {
    let raw = fixture::load(include_str!(
        "../../../../tests/fixtures/claude_success.json"
    ));
    let parsed = runner().parse_response(&raw.stdout).unwrap();
    assert_eq!(parsed.response.status, "done");
    assert!(parsed.response.summary.contains("Implemented"));
    assert_eq!(parsed.response.accomplished.len(), 2);
    assert_eq!(parsed.input_tokens, Some(15000));
    assert_eq!(parsed.output_tokens, Some(3500));
    assert_eq!(parsed.duration_ms, Some(45000));
}

#[test]
fn fixture_claude_rate_limit() {
    let raw = fixture::load(include_str!(
        "../../../../tests/fixtures/claude_error_rate_limit.json"
    ));
    let err = runner().parse_response(&raw.stdout).unwrap_err();
    assert!(matches!(err, AgentError::RateLimit { .. }), "got: {err:?}");
}

// ── NDJSON / stream-json fixtures ────────────────────────────

#[test]
fn fixture_claude_stream_error() {
    let raw = fixture::load(include_str!(
        "../../../../tests/fixtures/claude_stream_error.jsonl"
    ));
    let err = runner().parse_response(&raw.stdout).unwrap_err();
    assert!(matches!(err, AgentError::Auth { .. }), "got: {err:?}");
}

#[test]
fn fixture_claude_stream_thinking_block_conflict() {
    let raw = fixture::load(include_str!(
        "../../../../tests/fixtures/claude_stream_thinking_block_conflict.jsonl"
    ));
    let err = runner().parse_response(&raw.stdout).unwrap_err();
    assert!(
        matches!(err, AgentError::ThinkingBlockConflict { .. }),
        "expected ThinkingBlockConflict, got: {err:?}"
    );
}

#[test]
fn classify_error_thinking_block_conflict_from_stderr() {
    // The error can also surface through classify_error() when the claude
    // process exits non-zero with the message in stdout/stderr.
    let stderr = "claude failed: API Error: 400 messages.1.content.3: `thinking` or `redacted_thinking` blocks in the latest assistant message cannot be modified. These blocks must remain as they were in the original response.";
    let err = runner().classify_error(1, "", stderr);
    assert!(
        matches!(err, AgentError::ThinkingBlockConflict { .. }),
        "expected ThinkingBlockConflict, got: {err:?}"
    );
}

#[test]
fn thinking_block_conflict_does_not_match_unrelated_text() {
    // Ensure the pattern is specific enough to not false-positive on
    // work product that mentions "thinking" or "blocks".
    let unrelated = "I was thinking about how to handle blocks of code in the response.";
    let err = runner().classify_error(1, "", unrelated);
    assert!(
        !matches!(err, AgentError::ThinkingBlockConflict { .. }),
        "should not classify unrelated text as ThinkingBlockConflict, got: {err:?}"
    );
}

#[test]
fn parse_ndjson_with_intermediate_events() {
    // Parser must ignore intermediate events and only look at the result line
    let raw = concat!(
        r#"{"type":"system","subtype":"init"}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#,
        "\n",
        r#"{"type":"tool_use","tool":{"name":"Read","input":{}}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1000,"result":"{\"status\":\"done\",\"summary\":\"streamed\",\"accomplished\":[],\"remaining\":[],\"files\":[]}","usage":{"input_tokens":5,"output_tokens":10}}"#,
        "\n"
    );
    let parsed = runner().parse_response(raw).unwrap();
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.response.summary, "streamed");
    assert_eq!(parsed.input_tokens, Some(5));
    assert_eq!(parsed.output_tokens, Some(10));
}

#[test]
fn parse_ndjson_skips_malformed_lines() {
    // Malformed intermediate lines should be ignored; result line still parsed
    let raw = concat!(
        "not valid json\n",
        r#"{"type":"partial_update","data":"something"}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":500,"result":"{\"status\":\"done\",\"summary\":\"ok\",\"accomplished\":[],\"remaining\":[],\"files\":[]}","usage":{"input_tokens":1,"output_tokens":2}}"#,
        "\n"
    );
    let parsed = runner().parse_response(raw).unwrap();
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.response.summary, "ok");
}

#[test]
fn parse_single_line_json_backward_compat() {
    // Single-line JSON (--output-format json) still works
    let raw = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":2000,"result":"{\"status\":\"done\",\"summary\":\"backward compat\",\"accomplished\":[],\"remaining\":[],\"files\":[]}","usage":{"input_tokens":50,"output_tokens":25}}"#;
    let parsed = runner().parse_response(raw).unwrap();
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.response.summary, "backward compat");
    assert_eq!(parsed.input_tokens, Some(50));
    assert_eq!(parsed.output_tokens, Some(25));
}

// ── Kimi / MiniMax (Claude-compatible wrappers) ───────────────

#[test]
fn kimi_parses_claude_envelope() {
    let r = ClaudeRunner::new("kimi");
    let raw = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 3000,
            "result": "{\"status\":\"done\",\"summary\":\"kimi did it\",\"accomplished\":[\"task\"],\"remaining\":[],\"files\":[\"a.rs\"]}",
            "usage": {"input_tokens": 200, "output_tokens": 100}
        }"#;

    let parsed = r.parse_response(raw).unwrap();
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.response.summary, "kimi did it");
}

#[test]
fn minimax_parses_claude_envelope() {
    let r = ClaudeRunner::new("minimax");
    assert_eq!(r.name(), "minimax");
    let raw = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "{\"status\":\"done\",\"summary\":\"minimax did it\",\"accomplished\":[],\"remaining\":[],\"files\":[]}",
            "usage": {}
        }"#;

    let parsed = r.parse_response(raw).unwrap();
    assert_eq!(parsed.response.status, "done");
}

#[test]
fn kimi_build_command_uses_kimi_binary() {
    let r = ClaudeRunner::new("kimi");
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec![],
        allowed_tools: vec![],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let cmd = r.build_command(
        Some("sonnet"),
        "timeout 1800",
        "/tmp/sys.txt",
        "/tmp/msg.txt",
        &perms,
    );
    assert!(cmd.contains("kimi -p"), "expected kimi binary, got: {cmd}");
    assert!(cmd.contains("--model 'sonnet'"));
}

#[test]
fn build_command_with_allowed_tools() {
    let r = runner();
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec!["Bash(rm *)".to_string()], // should be ignored
        allowed_tools: vec![
            "Edit".to_string(),
            "Write".to_string(),
            "Read".to_string(),
            "Bash".to_string(),
            "Grep".to_string(),
            "git".to_string(),
            "npm".to_string(),
        ],
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let cmd = r.build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);

    // Should use --allowedTools instead of --disallowedTools
    assert!(
        cmd.contains("--allowedTools"),
        "expected --allowedTools flag, got: {cmd}"
    );
    assert!(
        !cmd.contains("--disallowedTools"),
        "should not have --disallowedTools when allowed_tools is set, got: {cmd}"
    );

    // Native tools pass through as-is
    assert!(cmd.contains("Edit"), "missing Edit tool");
    assert!(cmd.contains("Write"), "missing Write tool");
    assert!(cmd.contains("Bash"), "missing Bash tool");

    // CLI commands become Bash(<cmd> *) patterns
    assert!(
        cmd.contains("Bash(git *)"),
        "missing Bash(git *) pattern, got: {cmd}"
    );
    assert!(
        cmd.contains("Bash(npm *)"),
        "missing Bash(npm *) pattern, got: {cmd}"
    );
}

#[test]
fn build_command_allowed_tools_empty_falls_back_to_disallowed() {
    let r = runner();
    let perms = PermissionRules {
        autonomous: true,
        sandbox: SandboxLevel::WorkspaceWrite,
        disallowed_tools: vec!["Bash(rm *)".to_string()],
        allowed_tools: vec![], // empty = use disallowed_tools
        allowed_edit_paths: vec![],
        deny_read_only: false,
        extra_writable_dirs: vec![],
    };
    let cmd = r.build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);

    assert!(
        cmd.contains("--disallowedTools"),
        "should fall back to --disallowedTools when allowed_tools is empty, got: {cmd}"
    );
    assert!(
        !cmd.contains("--allowedTools"),
        "should not have --allowedTools when empty, got: {cmd}"
    );
}

#[test]
fn translate_allowed_tools_categorizes_correctly() {
    let allowed = vec![
        "Edit".to_string(),
        "Bash".to_string(),
        "Grep".to_string(),
        "git".to_string(),
        "cargo".to_string(),
        "WebFetch".to_string(),
    ];
    // No allowed_edit_paths → Edit passes through as-is
    let result = translate_allowed_tools(&allowed, &[]);
    assert_eq!(
        result,
        vec![
            "Edit",
            "Bash",
            "Grep",
            "Bash(git *)",
            "Bash(cargo *)",
            "WebFetch"
        ]
    );
}

#[test]
fn translate_allowed_tools_scopes_edit_write() {
    let allowed = vec!["Edit".to_string(), "Write".to_string(), "Read".to_string()];
    let paths = vec![PathBuf::from("/tmp/worktree")];
    let result = translate_allowed_tools(&allowed, &paths);
    assert_eq!(
        result,
        vec!["Edit(/tmp/worktree/*)", "Write(/tmp/worktree/*)", "Read"]
    );
}

#[test]
fn classify_error_overloaded_529() {
    // Claude 529 overloaded error
    let err = runner().classify_error(1, "", "overloaded_error: 529");
    assert!(matches!(err, AgentError::RateLimit { .. }), "got: {err:?}");
}

#[test]
fn classify_error_context_length() {
    let err = runner().classify_error(1, "", "context_length_exceeded: max 200000 tokens");
    assert!(
        matches!(err, AgentError::ContextOverflow { .. }),
        "got: {err:?}"
    );
}

// ── extract_text ─────────────────────────────────────────────

/// Claude stream-json: extract_text returns the inner result text for review parsing.
#[test]
fn extract_text_stream_json_returns_result_field() {
    let raw = concat!(
        r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reviewing..."}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"test_results\":\"pass\",\"issues\":[]}","usage":{"input_tokens":100,"output_tokens":20}}"#,
    );
    let text = runner().extract_text(raw).unwrap();
    assert_eq!(
        text,
        r#"{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#
    );
}

/// Claude stream-json with markdown-wrapped ReviewResponse in result field.
#[test]
fn extract_text_stream_json_markdown_result() {
    let review_json =
        r#"```json\n{"decision":"request_changes","notes":"Fix it","issues":[]}\n```"#;
    let result_line = format!(
        r#"{{"type":"result","subtype":"success","is_error":false,"result":"{review_json}","usage":{{"input_tokens":10,"output_tokens":5}}}}"#
    );
    let raw = format!(
        "{}\n{}",
        r#"{"type":"system","subtype":"init","session_id":"s1"}"#, result_line
    );
    let text = runner().extract_text(&raw).unwrap();
    assert!(
        text.contains("request_changes"),
        "expected review JSON in result, got: {text}"
    );
}

/// Claude stream-json error (is_error=true, rate limit): extract_text propagates RateLimit.
#[test]
fn extract_text_stream_json_rate_limit_propagates() {
    let raw = concat!(
        r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
        "\n",
        r#"{"type":"result","subtype":"error","is_error":true,"result":"rate limit exceeded: 429 Too Many Requests","usage":{"input_tokens":0,"output_tokens":0}}"#,
    );
    let err = runner().extract_text(raw).unwrap_err();
    assert!(
        matches!(err, AgentError::RateLimit { .. }),
        "expected RateLimit, got: {err:?}"
    );
}

#[test]
fn extract_stream_json_fallbacks_to_opencode_extractor_on_leading_init() {
    // Some Claude wrappers emit a leading system/init envelope and then
    // place the real JSON payload inside an assistant message content array
    // (no final "type":"result" line). Ensure our extractor falls back
    // to the opencode-style NDJSON scanner and returns the inner JSON.
    let raw = concat!(
        r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
        "\n",
        // The inner JSON must be escaped inside the outer JSON string
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"{\"status\":\"done\",\"summary\":\"fallback\",\"accomplished\":[],\"remaining\":[],\"files\":[]}"}]}}"#,
    );

    let text = extract_stream_json_result_text(raw).expect("should fallback and extract JSON");
    let parsed: serde_json::Value =
        serde_json::from_str(&text).expect("extracted text must be JSON");
    assert_eq!(parsed["status"], "done");
    assert_eq!(parsed["summary"], "fallback");
}

/// Claude stream-json error (is_error=true, auth): extract_text propagates Auth.
#[test]
fn extract_text_stream_json_auth_error_propagates() {
    let raw = concat!(
        r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
        "\n",
        r#"{"type":"result","subtype":"error","is_error":true,"result":"401 Unauthorized: invalid api key","usage":{"input_tokens":0,"output_tokens":0}}"#,
    );
    let err = runner().extract_text(raw).unwrap_err();
    assert!(
        matches!(err, AgentError::Auth { .. }),
        "expected Auth, got: {err:?}"
    );
}

/// extract_text on empty input returns empty string (not an error).
#[test]
fn extract_text_empty_input_returns_empty_ok() {
    let text = runner().extract_text("").unwrap();
    assert_eq!(text, "");
}

// ── find_claude_result ──────────────────────────────────────

#[test]
fn find_claude_result_success_with_tokens() {
    let ndjson = concat!(
        r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working..."}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"decision\":\"approve\",\"notes\":\"LGTM\"}","usage":{"input_tokens":1500,"output_tokens":200},"duration_ms":5000}"#,
    );
    let result = find_claude_result(ndjson).expect("should find result");
    assert!(!result.is_error);
    assert!(result.result_text.contains("approve"));
    assert_eq!(result.input_tokens, Some(1500));
    assert_eq!(result.output_tokens, Some(200));
    assert_eq!(result.duration_ms, Some(5000));
}

#[test]
fn find_claude_result_reads_total_cost_usd() {
    let ndjson = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"status\":\"done\",\"summary\":\"ok\"}","usage":{"input_tokens":10,"output_tokens":5},"total_cost_usd":0.1234}"#;
    let result = find_claude_result(ndjson).expect("should find result");
    assert_eq!(result.cost_usd, Some(0.1234));
    let parsed = runner().parse_response(ndjson).expect("should parse");
    assert_eq!(parsed.cost_usd, Some(0.1234));
}

#[test]
fn find_claude_result_without_a_cost_reports_none() {
    let ndjson = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","usage":{"input_tokens":10,"output_tokens":5}}"#;
    let result = find_claude_result(ndjson).expect("should find result");
    assert_eq!(result.cost_usd, None);
}

#[test]
fn find_claude_result_error_flag() {
    let ndjson = r#"{"type":"result","subtype":"error","is_error":true,"result":"rate limit exceeded","usage":{}}"#;
    let result = find_claude_result(ndjson).expect("should find result");
    assert!(result.is_error);
    assert!(result.result_text.contains("rate limit"));
}

#[test]
fn find_claude_result_error_envelope_with_empty_result_uses_errors() {
    // Grok 402: no `result` field, message only in errors[] — must not
    // become an empty AgentFailed (issue #3713)
    let ndjson = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0,"errors":["Internal error: {\"message\":\"API error (status 402 Payment Required): Grok Build usage balance exhausted\",\"http_status\":402}"]}"#;
    let result = find_claude_result(ndjson).expect("should find result");
    assert!(result.is_error);
    assert!(result.result_text.contains("usage balance exhausted"));
    assert!(cooldown::detect_credit_exhaustion(&result.result_text).is_some());
}

#[test]
fn find_claude_result_empty_returns_none() {
    assert!(find_claude_result("").is_none());
    assert!(find_claude_result("   ").is_none());
}

#[test]
fn find_claude_result_no_result_line_falls_back_to_assistant() {
    // MiniMax/Kimi pattern: no type:result, just assistant messages
    let ndjson = concat!(
        r#"{"type":"system","subtype":"init","model":"MiniMax-M2.7"}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"analysis complete"}],"usage":{"input_tokens":800,"output_tokens":150}}}"#,
    );
    let result = find_claude_result(ndjson).expect("should fall back to assistant text");
    assert!(!result.is_error);
    assert_eq!(result.result_text, "analysis complete");
    assert_eq!(result.input_tokens, Some(800));
    assert_eq!(result.output_tokens, Some(150));
}

#[test]
fn find_claude_result_plain_text_returns_none() {
    // Plain text (not NDJSON) should return None
    assert!(find_claude_result("just some plain text output").is_none());
}

#[test]
fn strip_tty_control_lines_drops_show_cursor_and_ctrl_d_redraw() {
    let redraw = "^D\u{0008}".repeat(20);
    let raw = format!(
        "{}\n{}\n{}\n{}\n{}",
        r#"{"type":"system","subtype":"init"}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#,
        SHOW_CURSOR,
        redraw,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"issues\":[]}"}"#,
    );
    let cleaned = strip_tty_control_lines(&raw);
    assert!(
        !cleaned.contains(SHOW_CURSOR),
        "show-cursor sequence must be stripped"
    );
    assert!(
        !cleaned.contains('\u{0008}'),
        "backspace redraw line must be stripped"
    );
    assert!(cleaned.contains("\"type\":\"result\""));
    assert!(cleaned.contains("hello"));
}

#[test]
fn strip_tty_control_lines_strips_trailing_cursor_on_json_line() {
    let line = format!(
        r#"{{"type":"result","subtype":"success","is_error":false,"result":"ok"}}{SHOW_CURSOR}"#
    );
    let cleaned = strip_tty_control_lines(&line);
    assert_eq!(
        cleaned,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#
    );
}

#[test]
fn find_claude_result_ignores_trailing_tty_junk() {
    let ndjson = format!(
        "{}\n{}\n{}",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"issues\":[]}","usage":{"input_tokens":10,"output_tokens":4}}"#,
        SHOW_CURSOR,
    );
    let result = find_claude_result(&ndjson).expect("result envelope should still parse");
    assert!(result.result_text.contains("approve"));
    assert_eq!(result.input_tokens, Some(10));
}

#[test]
fn review_session_exited_mid_thought_detects_thinking_without_envelope() {
    let ndjson = concat!(
        r#"{"type":"system","subtype":"thinking_tokens","tokens":12}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"CI passes. Now verifying callers:"}],"stop_reason":null,"usage":{"output_tokens":0}}}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"checking lease edge cases"}],"stop_reason":null,"usage":{"output_tokens":0}}}"#,
    );
    assert!(review_session_exited_mid_thought(ndjson));
    assert!(find_ndjson_result_line(ndjson).is_none());
}

#[test]
fn review_session_exited_mid_thought_detects_tool_use_without_envelope() {
    let ndjson = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs"}}]}}"#;
    assert!(review_session_exited_mid_thought(ndjson));
}

#[test]
fn review_session_exited_mid_thought_false_when_envelope_present() {
    let ndjson = concat!(
        r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"done"}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"issues\":[]}"}"#,
    );
    assert!(!review_session_exited_mid_thought(ndjson));
}

#[test]
fn review_session_exited_mid_thought_false_for_finished_text() {
    let ndjson = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"I have finished looking at the code."}]}}"#;
    assert!(!review_session_exited_mid_thought(ndjson));
}

#[test]
fn find_claude_result_model_usage() {
    let ndjson = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","modelUsage":{"claude-sonnet-4-20250514":{"inputTokens":5000,"outputTokens":1000}},"duration_ms":3000}"#;
    let result = find_claude_result(ndjson).expect("should extract modelUsage");
    assert_eq!(result.input_tokens, Some(5000));
    assert_eq!(result.output_tokens, Some(1000));
}

// ── Regression tests for issue #1387 ──────────────────────────────────────

/// Regression test: when an NDJSON stream has multiple type:result lines —
/// one with JSON in the result field (inner task delegate) and one with
/// plain text (outer session summary) — find_ndjson_result_line must
/// skip the plain-text one and return the JSON one.
#[test]
fn find_ndjson_result_line_skips_plain_text_result() {
    // Build test NDJSON using serde_json to avoid string-escaping confusion.
    let inner_result = serde_json::json!({"status": "done", "summary": "fix applied"});
    let json_result_line = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "duration_ms": 273_044,
        "result": inner_result.to_string(),
        "usage": {"input_tokens": 12, "output_tokens": 1577}
    })
    .to_string();
    let plain_result_line = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "duration_ms": 2352,
        "result": "All 2352 tests passed.",
        "usage": {"input_tokens": 3, "output_tokens": 16}
    })
    .to_string();
    // Concatenate without using concat! to avoid string-escaping headaches.
    let mut ndjson = json_result_line;
    ndjson.push('\n');
    ndjson.push_str(r#"{"type":"system","subtype":"task_notification"}"#);
    ndjson.push('\n');
    ndjson.push_str(r#"{"type":"system","subtype":"init"}"#);
    ndjson.push('\n');
    ndjson.push_str(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"summary here"}]}}"#,
    );
    ndjson.push('\n');
    ndjson.push_str(&plain_result_line);

    let line = find_ndjson_result_line(&ndjson)
        .expect("should find a result line")
        .0;
    // Must be the JSON result (inner), not the plain-text one (outer).
    let parsed: serde_json::Value =
        serde_json::from_str(line).expect("returned line must be valid JSON");
    let result_val = parsed.get("result").expect("result field must exist");
    assert!(
        result_val.is_string(),
        "result field must be a JSON string (not plain text), got: {result_val}"
    );
    let result_str = result_val.as_str().unwrap();
    assert!(
        result_str.starts_with('{'),
        "result must be JSON (object), got: {result_str}"
    );
    assert!(
        !result_str.contains("All 2352 tests passed"),
        "must NOT return outer plain-text result: {result_str}"
    );
}

/// Regression test: when BOTH result lines have plain-text results (no JSON),
/// find_ndjson_result_line must return the one with more tokens — not the last.
/// This is the actual task 1384 scenario: inner result has 12+1577 tokens,
/// outer confirmation has 3+16 tokens. Must pick the inner one (issue #1387).
#[test]
fn find_ndjson_result_line_prefers_more_tokens_on_plain_text() {
    // Build NDJSON where both results are plain text.
    let inner_result_line = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "duration_ms": 273_044,
        "result": "All 2352 tests pass. The fix is clean and correct.",
        "usage": {"input_tokens": 12, "output_tokens": 1577}
    })
    .to_string();
    let outer_result_line = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "duration_ms": 2352,
        "result": "Already retrieved the result — all 2352 tests passed.",
        "usage": {"input_tokens": 3, "output_tokens": 16}
    })
    .to_string();

    // Inner result FIRST, outer result LAST (the actual task 1384 ordering).
    let ndjson = format!(
        "{0}\n{1}\n{2}",
        inner_result_line, r#"{"type":"system","subtype":"init"}"#, outer_result_line
    );

    let (line, tokens) = find_ndjson_result_line(&ndjson).expect("should find a result line");

    // Must return the inner result (more tokens), not the outer one.
    assert!(
        line.contains("The fix is clean"),
        "expected inner result (more tokens), got: {line}"
    );
    // Tokens must match the inner result.
    assert_eq!(
        tokens,
        Some((12, 1577)),
        "tokens must be from the inner result"
    );
}

/// Regression test: when parse_response encounters plain-text results
/// and falls back to synthesis, tokens from the envelope must be preserved.
#[test]
fn parse_response_preserves_tokens_on_synthesis_fallback() {
    // Both results are plain text (no JSON in result field).
    // parse_response should synthesize but preserve the envelope tokens.
    let ndjson = concat!(
        r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":273044,"result":"All 2352 tests pass. The fix is clean and correct.","usage":{"input_tokens":12,"output_tokens":1577}}"#,
        "\n",
        r#"{"type":"system","subtype":"init"}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":2352,"result":"Already retrieved the result — all 2352 tests passed.","usage":{"input_tokens":3,"output_tokens":16}}"#,
    );

    let parsed = runner()
        .parse_response(ndjson)
        .expect("should parse successfully");

    // Must preserve tokens from the inner (higher-token) result.
    assert_eq!(
        parsed.input_tokens,
        Some(12),
        "input_tokens must be preserved (not lost on synthesis fallback)"
    );
    assert_eq!(
        parsed.output_tokens,
        Some(1577),
        "output_tokens must be preserved (not lost on synthesis fallback)"
    );
}

#[test]
fn parse_response_uses_inner_json_result_preserving_tokens() {
    // Simulates task 1384: outer session says "tests passed" (plain text),
    // but inner task delegate result is JSON with tokens.
    let ndjson = concat!(
        r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":273044,"result":"{\"status\":\"done\",\"summary\":\"fix applied\",\"accomplished\":[\"fix\"],\"remaining\":[],\"files\":[]}","usage":{"input_tokens":12,"output_tokens":1577}}"#,
        "\n",
        r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"tests passed"}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":2352,"result":"All 2352 tests passed.","usage":{"input_tokens":3,"output_tokens":16}}"#,
    );

    let parsed = runner()
        .parse_response(ndjson)
        .expect("should parse successfully");

    // Must use the inner JSON result (fix applied), not outer plain text.
    assert_eq!(
        parsed.response.status, "done",
        "expected inner JSON result status, got: {:?}",
        parsed.response.status
    );
    assert_eq!(
        parsed.response.summary.as_str(),
        "fix applied",
        "expected inner JSON result summary, got: {:?}",
        parsed.response.summary
    );
    // Tokens must be from the inner result with tokens, not the outer plain-text one.
    assert_eq!(
        parsed.input_tokens,
        Some(12),
        "input_tokens must be from inner result (not outer plain-text result)"
    );
    assert_eq!(
        parsed.output_tokens,
        Some(1577),
        "output_tokens must be from inner result (not outer plain-text result)"
    );
}

/// Regression test: when find_ndjson_result_line returns None (no JSON
/// result found), extract_stream_json_result_text must fall back to
/// opencode-style text extraction, not return raw NDJSON.
#[test]
fn extract_text_falls_back_to_opencode_style_when_no_json_result() {
    // An NDJSON stream with no type:result at all — just a text event.
    let ndjson = concat!(
        r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"All tasks complete"}]}}"#,
    );

    let text = runner().extract_text(ndjson).expect("should extract text");
    // Must extract the assistant message text, not raw NDJSON.
    assert!(
        text.contains("All tasks complete"),
        "expected extracted assistant text, got: {text}"
    );
    assert!(!text.contains("type"), "must not return raw NDJSON: {text}");
}

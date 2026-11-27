use super::*;
use serde_json::json;

fn events(line: &str) -> Vec<StreamEvent> {
    ClaudeRunner::new("claude").stream_events(&serde_json::from_str(line).unwrap())
}

#[test]
fn init_reports_model() {
    let line = r#"{"type":"system","subtype":"init","model":"claude-sonnet-5","cwd":"/w"}"#;
    assert_eq!(
        events(line),
        vec![StreamEvent::Started {
            model: "claude-sonnet-5".into()
        }]
    );
}

#[test]
fn assistant_blocks_in_order() {
    let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"plan"},{"type":"text","text":"Reading."},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"a.rs"}}]}}"#;
    assert_eq!(
        events(line),
        vec![
            StreamEvent::Thinking("plan".into()),
            StreamEvent::Text("Reading.".into()),
            StreamEvent::ToolCall {
                id: "t1".into(),
                name: "Read".into(),
                input: json!({"file_path": "a.rs"}),
            },
        ]
    );
}

#[test]
fn tool_result_string_and_block_content() {
    let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"},{"type":"tool_result","tool_use_id":"t2","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}],"is_error":true}]}}"#;
    assert_eq!(
        events(line),
        vec![
            StreamEvent::ToolResult {
                id: "t1".into(),
                name: String::new(),
                output: "ok".into(),
                is_error: false,
            },
            StreamEvent::ToolResult {
                id: "t2".into(),
                name: String::new(),
                output: "a\nb".into(),
                is_error: true,
            },
        ]
    );
}

#[test]
fn result_success_and_error() {
    let ok = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":66,"duration_ms":580214}"#;
    assert_eq!(
        events(ok),
        vec![StreamEvent::Done("66 turns · 9m 40s".into())]
    );
    let err = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"too many turns"}"#;
    assert_eq!(
        events(err),
        vec![StreamEvent::Error("too many turns".into())]
    );
}

#[test]
fn rate_limit_windows_parses_rejected_and_unknown_types() {
    let ndjson = concat!(
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","utilization":0.4,"resetsAt":1900000000}}"#,
        "\n",
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"weekly_opus","resetsAt":1900003600}}"#,
        "\n",
        r#"{"type":"rate_limit_event","rate_limit_info":{}}"#,
    );
    let windows = rate_limit_windows(ndjson);
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].status, "allowed");
    assert_eq!(windows[0].utilization, Some(0.4));
    assert_eq!(windows[1].status, "rejected");
    assert_eq!(windows[1].window_type, "weekly_opus");
    assert_eq!(windows[1].resets_at, Some(1_900_003_600));
}

#[test]
fn rate_limit_windows_reads_every_unified_window() {
    let ndjson = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1791259200,"rateLimitType":"seven_day","overageStatus":"rejected","isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":0.13,"resetsAt":1791250200},"seven_day":{"utilization":1,"resetsAt":1791259200}}}}"#;
    let windows = rate_limit_windows(ndjson);
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].status, "rejected");
    assert_eq!(windows[0].window_type, "seven_day");
    assert_eq!(windows[0].utilization, Some(1.0));
    assert_eq!(windows[0].resets_at, Some(1_791_259_200));
    assert_eq!(windows[1].status, "unknown");
    assert_eq!(windows[1].window_type, "five_hour");
    assert_eq!(windows[1].utilization, Some(0.13));
    assert_eq!(windows[1].resets_at, Some(1_791_250_200));
}

#[test]
fn noise_events_are_dropped() {
    for line in [
        r#"{"type":"system","subtype":"thinking_tokens","tokens":12}"#,
        r#"{"type":"system","subtype":"task_started"}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{}}"#,
        r#"{"type":"tool_progress"}"#,
    ] {
        assert!(events(line).is_empty(), "{line}");
    }
}

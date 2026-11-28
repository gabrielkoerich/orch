use super::*;
use serde_json::json;

fn events(line: &str) -> Vec<StreamEvent> {
    OpenCodeRunner::new().stream_events(&serde_json::from_str(line).unwrap())
}

#[test]
fn text_in_part_or_top_level() {
    assert_eq!(
        events(r#"{"type":"text","part":{"type":"text","text":"Reviewing."}}"#),
        vec![StreamEvent::Text("Reviewing.".into())]
    );
    assert_eq!(
        events(r#"{"type":"text","text":"Plain."}"#),
        vec![StreamEvent::Text("Plain.".into())]
    );
}

#[test]
fn completed_tool_has_call_and_result() {
    let line = r#"{"type":"tool_use","part":{"type":"tool","tool":"read","callID":"c1","state":{"status":"completed","input":{"filePath":"src/lib.rs"},"output":"a\nb"}}}"#;
    assert_eq!(
        events(line),
        vec![
            StreamEvent::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                input: json!({"filePath": "src/lib.rs"}),
            },
            StreamEvent::ToolResult {
                id: "c1".into(),
                name: "read".into(),
                output: "a\nb".into(),
                is_error: false,
            },
        ]
    );
}

#[test]
fn failed_tool_result_is_error() {
    let line = r#"{"type":"tool_use","part":{"tool":"bash","callID":"c2","state":{"status":"error","input":{"command":"x"},"error":"not found"}}}"#;
    assert_eq!(
        events(line)[1],
        StreamEvent::ToolResult {
            id: "c2".into(),
            name: "bash".into(),
            output: "not found".into(),
            is_error: true,
        }
    );
}

#[test]
fn only_final_step_is_done() {
    let dropped = events(r#"{"type":"step_finish","part":{"reason":"tool-calls"}}"#);
    assert!(dropped.is_empty(), "{dropped:?}");
    assert_eq!(
        events(r#"{"type":"step_finish","part":{"reason":"stop"}}"#),
        vec![StreamEvent::Done(String::new())]
    );
    let dropped = events(r#"{"type":"step_start"}"#);
    assert!(dropped.is_empty(), "{dropped:?}");
}

#[test]
fn error_event() {
    let line = r#"{"type":"error","error":{"name":"UnknownError","data":{"message":"Provider returned error"}}}"#;
    assert_eq!(
        events(line),
        vec![StreamEvent::Error("Provider returned error".into())]
    );
}

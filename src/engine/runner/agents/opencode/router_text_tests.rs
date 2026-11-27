use super::*;

#[test]
fn extract_router_text_supports_message_with_content_array() {
    // Some opencode versions nest assistant text in a message.content array.
    let raw = r#"{"type":"step_start","timestamp":1}
{"type":"message","timestamp":2,"part":{"type":"message","content":[{"type":"text","text":"```json\n{\"executor\":\"opencode\",\"complexity\":\"medium\",\"reason\":\"ok\"}\n```"}]}}
{"type":"step_finish","timestamp":3,"part":{"type":"step-finish","reason":"stop"}}"#;
    let text = extract_router_text(raw).expect("should extract text");
    assert!(
        text.contains("executor"),
        "extracted text should include JSON"
    );
}

#[test]
fn extract_router_text_ignores_leading_system_init() {
    // Early opencode wrappers may emit a system/init envelope before the
    // actual text/result event. Ensure the extractor skips the init envelope
    // and returns the real JSON payload from the subsequent text event.
    let raw = r#"{"type":"system","subtype":"init","cwd":"/"}
{"type":"text","timestamp":2,"text":"{\"executor\":\"opencode\",\"complexity\":\"medium\",\"reason\":\"ndjson\"}"}
{"type":"step_finish","timestamp":3}"#;

    let text = extract_router_text(raw).expect("should extract text even with leading init");
    let parsed: serde_json::Value =
        serde_json::from_str(&text).expect("extracted text must be JSON");
    assert_eq!(parsed["executor"], "opencode");
    assert_eq!(parsed["complexity"], "medium");
}

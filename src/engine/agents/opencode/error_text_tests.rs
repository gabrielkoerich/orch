use super::*;
use serde_json::json;

#[test]
fn unrecognized_error_shape_keeps_raw_json() {
    let e = json!({"type":"error","error":{"code":429,"detail":"slow down"}});
    assert!(opencode_error_text(&e).contains("slow down"));
    let e = json!({"type":"error"});
    assert!(opencode_error_text(&e).contains("\"type\""));
}

#[test]
fn known_shapes_still_extract() {
    assert_eq!(opencode_error_text(&json!({"message":"a"})), "a");
    assert_eq!(opencode_error_text(&json!({"error":"b"})), "b");
    assert_eq!(opencode_error_text(&json!({"error":{"message":"c"}})), "c");
    assert_eq!(
        opencode_error_text(&json!({"error":{"name":"N","data":{"message":"d"}}})),
        "d"
    );
}

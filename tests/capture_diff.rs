#[allow(dead_code)]
mod channels {
    pub mod tmux {
        pub async fn capture_pane(_session: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }

        pub async fn is_session_dead(_session: &str) -> bool {
            false
        }
    }

    #[derive(Debug, Clone)]
    pub struct OutputChunk {
        pub content: String,
        pub is_final: bool,
    }

    pub mod transport {
        use crate::channels::OutputChunk;

        #[derive(Debug)]
        pub struct Transport;

        impl Transport {
            pub async fn get_session_output(&self, _task_id: &str) -> Option<String> {
                None
            }

            pub async fn push_output(&self, _task_id: &str, _chunk: OutputChunk) {}
        }
    }
}

#[allow(dead_code)]
#[path = "../src/channels/capture.rs"]
mod capture;

use capture::{OutputBuffer, MAX_OUTPUT_BUFFER_BYTES};

fn buffer() -> OutputBuffer {
    OutputBuffer {
        session: "session".to_string(),
        task_id: "task".to_string(),
        last_content: String::new(),
        last_len: 0,
        last_hash: None,
        last_capture: chrono::Utc::now(),
        seen_alive: false,
    }
}

#[test]
fn repeated_capture_no_duplicate_output() {
    let mut buf = buffer();
    let first = buf.diff_and_update("hello");
    assert_eq!(first.as_deref(), Some("hello"));

    let second = buf.diff_and_update("hello");
    assert!(second.is_none());
}

#[test]
fn pane_clear_resets_offset() {
    let mut buf = buffer();
    let _ = buf.diff_and_update("line1\nline2\nline3");
    // Terminal clear shrinks the pane — must not replay visible content as "new" output.
    let cleared = buf.diff_and_update("line1\n");

    assert_eq!(cleared, None);
    // last_len must be updated so subsequent appends diff from the new position.
    assert_eq!(buf.last_len, "line1\n".len());
}

#[test]
fn large_output_is_capped() {
    let mut buf = buffer();
    let big = "a".repeat(MAX_OUTPUT_BUFFER_BYTES + 1024);
    let _ = buf.diff_and_update(&big);

    assert!(buf.last_content.len() <= MAX_OUTPUT_BUFFER_BYTES);
    assert_eq!(buf.last_len, big.len());
}

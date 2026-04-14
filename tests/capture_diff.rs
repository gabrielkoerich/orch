#[allow(dead_code)]
mod channels {
    pub mod tmux {
        pub async fn capture_pane(_session: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }

        /// Sessions whose names end with "-dead" are treated as dead; all others
        /// are treated as alive. This lets integration tests exercise both the
        /// #2573 fix (dead seen-alive sessions → silent) and the #2318 fix
        /// (alive seen-alive sessions → NOT silent) without real tmux.
        pub async fn is_session_dead(session: &str) -> bool {
            session.ends_with("-dead")
        }
    }

    #[derive(Debug, Clone)]
    pub struct OutputChunk {
        pub content: String,
        pub is_final: bool,
    }

    pub mod transport {
        use crate::channels::OutputChunk;

        pub fn session_key(repo: &str, task_id: &str) -> String {
            if task_id.starts_with("internal:") {
                task_id.to_string()
            } else {
                format!("{repo}:{task_id}")
            }
        }

        #[derive(Debug)]
        pub struct Transport;

        impl Transport {
            pub fn new() -> Self {
                Self
            }

            pub async fn get_session_output(&self, _task_id: &str) -> Option<String> {
                None
            }

            pub async fn push_output(&self, _repo: &str, _task_id: &str, _chunk: OutputChunk) {}

            pub async fn unbind(&self, _repo: &str, _task_id: &str) {}
        }
    }
}

#[allow(dead_code)]
#[path = "../src/channels/capture.rs"]
mod capture;

use capture::{CaptureService, OutputBuffer, MAX_OUTPUT_BUFFER_BYTES};
use channels::transport::Transport;
use std::sync::Arc;

fn buffer() -> OutputBuffer {
    OutputBuffer {
        repo: "owner/repo".to_string(),
        session: "session".to_string(),
        task_id: "task".to_string(),
        last_content: String::new(),
        last_len: 0,
        last_hash: None,
        last_capture: chrono::Utc::now(),
        seen_alive: false,
        registered_at: chrono::Utc::now(),
        has_output: false,
        generation: 0,
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

/// Regression test for issue #2318: a session confirmed alive (`seen_alive=true`)
/// with no terminal output and a LIVE tmux session must NOT be treated as silent.
///
/// The mock in this file returns `is_session_dead=false` for sessions that don't
/// end in "-dead", so "orch-seen-alive" simulates a running session doing
/// file-heavy work with sparse terminal output (e.g., complex Claude refactoring).
#[tokio::test]
async fn seen_alive_with_live_session_not_silenced() {
    let transport = Arc::new(Transport::new());
    let svc = CaptureService::new(transport);
    let repo = "owner/repo";

    // Session past grace period, seen alive, no output, tmux session ALIVE
    // (name does not end in "-dead" so mock returns is_session_dead=false).
    svc.register_session(repo, "long-running-task", "orch-seen-alive")
        .await;
    svc.set_buffer_state_for_test(
        repo,
        "long-running-task",
        true,  // seen_alive
        false, // has_output (sparse but alive)
        chrono::Utc::now() - chrono::Duration::seconds(900),
    )
    .await;

    let grace = std::time::Duration::from_secs(120);
    let silent = svc.get_silent_sessions_for_repo(repo, grace).await;
    assert!(
        silent.is_empty(),
        "seen_alive=true + live session must NOT appear in silent list (issue #2318)"
    );
}

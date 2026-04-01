//! Kimi agent runner — extends the Claude-compatible runner.
//!
//! Kimi uses the `claude` CLI via a shell wrapper that sets
//! `ANTHROPIC_BASE_URL` to the Kimi API. Like MiniMax, the underlying
//! model may not emit `"type":"result"` events consistently.
//!
//! This runner delegates to `MiniMaxRunner` for stream parsing since
//! both use the same claude CLI wrapper pattern and share the same
//! output format differences from native Claude.

use super::minimax::MiniMaxClaudeRunner;
use super::{delegate_agent_runner, AgentError, AgentRunner, ParsedResponse, PermissionRules};

/// Runner for Kimi-via-Claude agents (same output handling as MiniMaxClaude).
/// Named `KimiClaudeRunner` to distinguish from a future native Kimi CLI runner.
pub struct KimiClaudeRunner {
    inner: MiniMaxClaudeRunner,
}

impl KimiClaudeRunner {
    pub fn new() -> Self {
        Self {
            inner: MiniMaxClaudeRunner::new("kimi"),
        }
    }
}

delegate_agent_runner!(KimiClaudeRunner, inner);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_name() {
        let runner = KimiClaudeRunner::new();
        assert_eq!(runner.name(), "kimi");
    }
}

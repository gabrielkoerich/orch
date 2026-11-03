//! Generate a complete, delegating `AgentRunner` impl for a wrapper struct.
//!
//! Use this for runner types that delegate every `AgentRunner` method to an
//! inner field (e.g., `KimiClaudeRunner → MiniMaxClaudeRunner`).
//!
//! ```ignore
//! delegate_agent_runner!(KimiClaudeRunner, inner);
//! ```

macro_rules! delegate_agent_runner {
    ($runner:ty, $inner:ident) => {
        impl AgentRunner for $runner {
            #[cfg(test)]
            fn name(&self) -> &str {
                self.$inner.name()
            }

            fn build_command(
                &self,
                model: Option<&str>,
                timeout_cmd: &str,
                sys_file: &str,
                msg_file: &str,
                permissions: &PermissionRules,
            ) -> String {
                self.$inner
                    .build_command(model, timeout_cmd, sys_file, msg_file, permissions)
            }

            fn parse_response(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
                self.$inner.parse_response(raw)
            }

            fn extract_text(&self, raw: &str) -> Result<String, AgentError> {
                self.$inner.extract_text(raw)
            }

            fn classify_error(&self, exit_code: i32, stdout: &str, stderr: &str) -> AgentError {
                self.$inner.classify_error(exit_code, stdout, stderr)
            }

            fn stream_events(&self, event: &serde_json::Value) -> Vec<StreamEvent> {
                self.$inner.stream_events(event)
            }

            fn router_transcript(&self) -> crate::engine::runner::agents::RouterTranscript {
                self.$inner.router_transcript()
            }

            fn router_command(
                &self,
                prompt: &str,
                model: Option<&str>,
            ) -> anyhow::Result<tokio::process::Command> {
                self.$inner.router_command(prompt, model)
            }
        }
    };
}

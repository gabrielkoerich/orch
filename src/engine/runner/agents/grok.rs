//! Grok agent runner.
//!
//! Headless Grok (`grok -p`) does not read the prompt from stdin. The task
//! message is `--prompt-file`. The system prompt is passed with
//! `--system-prompt-override`. Output is `streaming-messages-json`, the
//! Messages API NDJSON shape, so parsing delegates to the Claude runner.
//!
//! ```bash
//! grok --output-format streaming-messages-json \
//!   --permission-mode bypassPermissions \
//!   --sandbox off \
//!   --deny 'Bash(rm *)' \
//!   --system-prompt-override "$(cat "{sys_file}")" \
//!   --prompt-file "{msg_file}"
//! ```
//!
//! `--sandbox workspace` only writes the cwd. A worktree's git dir lives
//! outside that cwd, and the CLI has no extra-dir flag, so task runs use
//! `--sandbox off`. Orch still isolates the agent in the worktree and denies
//! `rm` and `git push`. Control sessions (`deny_read_only`) use `read-only`.

use super::claude::{self, ClaudeRunner};
use super::{
    shell_single_quote, AgentError, AgentRunner, ParsedResponse, PermissionRules, StreamEvent,
};

/// Runner for the `grok` CLI
#[derive(Debug)]
pub struct GrokRunner;

impl GrokRunner {
    fn parser() -> ClaudeRunner {
        ClaudeRunner::new("grok")
    }
}

impl AgentRunner for GrokRunner {
    #[cfg(test)]
    fn name(&self) -> &'static str {
        "grok"
    }

    fn stream_events(&self, event: &serde_json::Value) -> Vec<StreamEvent> {
        claude::stream_events(event)
    }

    fn router_transcript(&self) -> super::RouterTranscript {
        super::RouterTranscript::Messages
    }

    fn extract_text(&self, raw: &str) -> Result<String, AgentError> {
        Self::parser().extract_text(raw)
    }

    fn build_command(
        &self,
        model: Option<&str>,
        timeout_cmd: &str,
        sys_file: &str,
        msg_file: &str,
        permissions: &PermissionRules,
    ) -> String {
        let sq_sys = shell_single_quote(sys_file);
        let sq_msg = shell_single_quote(msg_file);
        let mut args = Vec::new();
        if !timeout_cmd.is_empty() {
            args.push(timeout_cmd.to_string());
        }
        args.push("grok".to_string());
        args.push("--no-plan".to_string());
        args.push("--output-format".to_string());
        args.push("streaming-messages-json".to_string());
        args.push("--permission-mode".to_string());
        args.push(permission_mode(permissions).to_string());
        args.push("--sandbox".to_string());
        args.push(sandbox_profile(permissions).to_string());
        if let Some(model) = model.filter(|m| !m.is_empty()) {
            args.push("--model".to_string());
            args.push(shell_single_quote(model));
        }
        args.extend(permission_args(permissions));
        args.push("--system-prompt-override".to_string());
        args.push(format!("\"$(cat {sq_sys})\""));
        args.push("--prompt-file".to_string());
        args.push(sq_msg);
        args.join(" ")
    }

    fn parse_response(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
        Self::parser().parse_response(raw)
    }

    fn classify_error(&self, exit_code: i32, stdout: &str, stderr: &str) -> AgentError {
        let extracted = startup_errors(stdout);
        for message in &extracted {
            if let Some(err) = classify_grok_message(message) {
                return err;
            }
        }
        let stderr = if extracted.is_empty() {
            stderr.to_string()
        } else {
            format!("{stderr}\n{}", extracted.join("\n"))
        };
        Self::parser().classify_error(exit_code, stdout, &stderr)
    }

    fn router_command(
        &self,
        prompt: &str,
        model: Option<&str>,
    ) -> anyhow::Result<tokio::process::Command> {
        // Classification only needs a JSON answer. Strip the tools so the
        // call cannot edit the repo or browse the network while routing.
        let mut cmd = tokio::process::Command::new("grok");
        cmd.arg("--no-plan")
            .arg("--no-subagents")
            .arg("--disable-web-search")
            .arg("--max-turns")
            .arg("1")
            .arg("--output-format")
            .arg("streaming-messages-json")
            .arg("--permission-mode")
            .arg("dontAsk")
            .arg("--disallowed-tools")
            .arg("read_file,grep,list_dir,search_replace,write,run_terminal_cmd,web_search,web_fetch,task");
        if let Some(model) = model.filter(|m| !m.is_empty()) {
            cmd.arg("--model").arg(model);
        }
        cmd.arg("-p").arg(prompt);
        Ok(cmd)
    }
}

/// `streaming-messages-json` result, plus Grok's cost and stop reason
pub fn find_grok_result(ndjson: &str) -> Option<super::AgentResult> {
    let mut result = claude::find_claude_result(ndjson)?;
    let Some((line, _)) = claude::find_ndjson_result_line(ndjson.trim()) else {
        return Some(result);
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
        return Some(result);
    };
    if result.cost_usd.is_none() {
        result.cost_usd = val
            .get("total_cost_usd")
            .and_then(serde_json::Value::as_f64);
    }
    if val.get("stop_reason").and_then(|v| v.as_str()) == Some("max_tokens") {
        result.truncated_by_length = true;
    }
    Some(result)
}

fn permission_mode(permissions: &PermissionRules) -> &'static str {
    if permissions.autonomous {
        "bypassPermissions"
    } else if permissions.deny_read_only {
        "dontAsk"
    } else {
        "acceptEdits"
    }
}

fn sandbox_profile(permissions: &PermissionRules) -> &'static str {
    if permissions.deny_read_only {
        "read-only"
    } else {
        "off"
    }
}

/// Claude tool names that map to a Grok tool id for `--tools` / `--disallowed-tools`
fn grok_tool_id(name: &str) -> Option<&'static str> {
    Some(match name {
        "Read" => "read_file",
        "Grep" => "grep",
        "Glob" => "list_dir",
        "Edit" => "search_replace",
        "Write" => "write",
        "Bash" => "run_terminal_cmd",
        "WebFetch" => "web_fetch",
        "WebSearch" => "web_search",
        "Task" => "task",
        "TodoWrite" => "todo_write",
        _ => return None,
    })
}

fn permission_args(permissions: &PermissionRules) -> Vec<String> {
    if !permissions.allowed_tools.is_empty() {
        return allow_args(permissions);
    }
    deny_args(&permissions.disallowed_tools)
}

fn allow_args(permissions: &PermissionRules) -> Vec<String> {
    let mut tools = Vec::new();
    let mut allows = Vec::new();
    for item in &permissions.allowed_tools {
        if let Some(id) = grok_tool_id(item) {
            if !tools.contains(&id) {
                tools.push(id);
            }
            if (item == "Edit" || item == "Write") && !permissions.allowed_edit_paths.is_empty() {
                for path in &permissions.allowed_edit_paths {
                    allows.push(format!("{item}({}/**)", path.display()));
                }
            }
        } else if item.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
            // CLI command names become shell allow rules. PascalCase leftovers
            // such as Skill are Claude-only tools, not shell commands
            allows.push(format!("Bash({item} *)"));
        }
    }
    // An allowlist does not express the rm / git-push bans. Keep those denies.
    let mut args = Vec::new();
    if !tools.is_empty() {
        args.push("--tools".to_string());
        args.push(shell_single_quote(&tools.join(",")));
    }
    for rule in allows {
        args.push("--allow".to_string());
        args.push(shell_single_quote(&rule));
    }
    args.extend(deny_args(&permissions.disallowed_tools));
    args
}

fn deny_args(rules: &[String]) -> Vec<String> {
    let mut remove = Vec::new();
    let mut denies = Vec::new();
    for rule in rules {
        match rule.as_str() {
            "Read" | "Grep" | "Glob" | "Edit" | "Write" | "Bash" => {
                if let Some(id) = grok_tool_id(rule) {
                    if !remove.contains(&id) {
                        remove.push(id);
                    }
                }
            }
            _ => denies.push(rule.as_str()),
        }
    }
    let mut args = Vec::new();
    if !remove.is_empty() {
        args.push("--disallowed-tools".to_string());
        args.push(shell_single_quote(&remove.join(",")));
    }
    for rule in denies {
        args.push("--deny".to_string());
        args.push(shell_single_quote(rule));
    }
    args
}

/// `type:error` messages. These are startup failures, not assistant prose.
fn startup_errors(stdout: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|t| t.as_str()) != Some("error") {
            continue;
        }
        if let Some(message) = val.get("message").and_then(|m| m.as_str()) {
            if !message.is_empty() {
                out.push(message.to_string());
            }
        }
    }
    out
}

/// Grok phrases the shared classifier does not already name
fn classify_grok_message(message: &str) -> Option<AgentError> {
    let lower = message.to_lowercase();
    if lower.contains("not logged in")
        || lower.contains("please log in")
        || lower.contains("grok login")
        || lower.contains("xai_api_key")
        || lower.contains("authentication failed")
        || lower.contains("could not refresh")
    {
        return Some(AgentError::Auth {
            message: message.to_string(),
        });
    }
    if lower.contains("permission required") {
        return Some(AgentError::PermissionDenied {
            message: message.to_string(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::agents::SandboxLevel;

    fn runner() -> GrokRunner {
        GrokRunner
    }

    fn perms(autonomous: bool, disallowed: &[&str], allowed: &[&str]) -> PermissionRules {
        PermissionRules {
            autonomous,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: disallowed.iter().map(|s| (*s).to_string()).collect(),
            allowed_tools: allowed.iter().map(|s| (*s).to_string()).collect(),
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        }
    }

    #[test]
    fn build_command_autonomous_denies_rm_and_passes_files() {
        let cmd = runner().build_command(
            Some("fast"),
            "timeout 1800",
            "/tmp/sys.txt",
            "/tmp/msg.txt",
            &perms(true, &["Bash(rm *)", "Bash(git push*)"], &[]),
        );
        assert!(cmd.starts_with("timeout 1800 grok "), "{cmd}");
        assert!(cmd.contains("--model 'fast'"), "{cmd}");
        assert!(
            cmd.contains("--output-format streaming-messages-json"),
            "{cmd}"
        );
        assert!(cmd.contains("--permission-mode bypassPermissions"), "{cmd}");
        assert!(cmd.contains("--sandbox off"), "{cmd}");
        assert!(cmd.contains("--no-plan"), "{cmd}");
        assert!(cmd.contains("--deny 'Bash(rm *)'"), "{cmd}");
        assert!(cmd.contains("--deny 'Bash(git push*)'"), "{cmd}");
        assert!(cmd.contains("--prompt-file '/tmp/msg.txt'"), "{cmd}");
        assert!(
            cmd.contains("--system-prompt-override \"$(cat '/tmp/sys.txt')\""),
            "{cmd}"
        );
        assert!(!cmd.contains("< "), "{cmd}");
    }

    #[test]
    fn build_command_omits_empty_model() {
        let cmd = runner().build_command(
            Some(""),
            "",
            "/tmp/sys.txt",
            "/tmp/msg.txt",
            &perms(true, &[], &[]),
        );
        assert!(!cmd.contains("--model"), "{cmd}");
    }

    #[test]
    fn build_command_supervised_accepts_edits() {
        let cmd = runner().build_command(
            None,
            "",
            "/tmp/sys.txt",
            "/tmp/msg.txt",
            &perms(false, &[], &[]),
        );
        assert!(cmd.contains("--permission-mode acceptEdits"), "{cmd}");
    }

    #[test]
    fn build_command_read_only_removes_read_tools() {
        let rules = PermissionRules::deny_read_only();
        let cmd = runner().build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &rules);
        assert!(cmd.contains("--sandbox read-only"), "{cmd}");
        assert!(cmd.contains("--permission-mode bypassPermissions"), "{cmd}");
        assert!(cmd.contains("read_file"), "{cmd}");
        assert!(cmd.contains("grep"), "{cmd}");
        assert!(cmd.contains("list_dir"), "{cmd}");
        assert!(cmd.contains("--deny 'Bash(cat *)'"), "{cmd}");
    }

    #[test]
    fn build_command_allowlist_keeps_destructive_denies() {
        let mut rules = perms(true, &["Bash(rm *)"], &["Edit", "Write", "Bash", "git"]);
        rules.allowed_edit_paths = vec![std::path::PathBuf::from("/work/tree")];
        let cmd = runner().build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &rules);
        assert!(
            cmd.contains("--tools 'search_replace,write,run_terminal_cmd'"),
            "{cmd}"
        );
        assert!(cmd.contains("--allow 'Edit(/work/tree/**)'"), "{cmd}");
        assert!(cmd.contains("--allow 'Write(/work/tree/**)'"), "{cmd}");
        assert!(cmd.contains("--allow 'Bash(git *)'"), "{cmd}");
        assert!(cmd.contains("--deny 'Bash(rm *)'"), "{cmd}");
    }

    #[test]
    fn parse_response_reads_messages_result() {
        let raw = concat!(
            r#"{"type":"system","subtype":"init","model":"test"}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"status\":\"done\",\"summary\":\"shipped\",\"accomplished\":[\"a\"],\"remaining\":[],\"files\":[]}","usage":{"input_tokens":8,"output_tokens":3},"total_cost_usd":0.02,"duration_ms":40,"stop_reason":"end_turn"}"#,
            "\n",
        );
        let parsed = runner().parse_response(raw).expect("parse");
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.response.summary, "shipped");
        assert_eq!(parsed.input_tokens, Some(8));
        assert_eq!(parsed.output_tokens, Some(3));
    }

    #[test]
    fn find_result_reads_cost_and_max_tokens() {
        let raw = r#"{"type":"result","subtype":"success","is_error":false,"result":"partial","usage":{"input_tokens":1,"output_tokens":2},"total_cost_usd":1.5,"stop_reason":"max_tokens"}"#;
        let result = find_grok_result(raw).expect("result");
        assert_eq!(result.result_text, "partial");
        assert_eq!(result.cost_usd, Some(1.5));
        assert!(result.truncated_by_length);
        assert_eq!(result.input_tokens, Some(1));
    }

    #[test]
    fn classify_error_rate_limit_from_error_event() {
        let stdout = r#"{"type":"error","message":"429 Too Many Requests"}"#;
        let err = runner().classify_error(1, stdout, "");
        assert!(matches!(err, AgentError::RateLimit { .. }), "{err}");
    }

    #[test]
    fn classify_error_model_unavailable() {
        let stdout = r#"{"type":"error","message":"The model `missing` is not supported"}"#;
        let err = runner().classify_error(1, stdout, "");
        match err {
            AgentError::ModelUnavailable { model, .. } => assert_eq!(model, "missing"),
            other => panic!("expected model unavailable, got {other}"),
        }
    }

    #[test]
    fn classify_error_login_is_auth() {
        let stdout = r#"{"type":"error","message":"Not logged in. Run grok login."}"#;
        let err = runner().classify_error(1, stdout, "");
        assert!(matches!(err, AgentError::Auth { .. }), "{err}");
    }

    #[test]
    fn classify_error_permission_required() {
        let stdout =
            r#"{"type":"error","message":"permission required: Edit calc.py was not run"}"#;
        let err = runner().classify_error(1, stdout, "");
        assert!(matches!(err, AgentError::PermissionDenied { .. }), "{err}");
    }
}

use super::AgentError;
use std::time::Duration;

/// Returns true if the pattern found at `lower_pos` (byte offset in lowercase text)
/// is at a word boundary in the original `text` — i.e., surrounded by non-identifier
/// chars. Used to suppress false positives from identifiers like `record_rate_limit_returns_id`.
fn is_word_boundary(text: &str, lower_pos: usize, pattern: &str) -> bool {
    let after_match = lower_pos + pattern.len();
    let prev_is_boundary = lower_pos == 0
        || !text[..lower_pos]
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
    let next_is_boundary = after_match >= text.len()
        || !text[after_match..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
    prev_is_boundary && next_is_boundary
}

fn lower_offset_to_original_byte(text: &str, lower: &str, lower_pos: usize) -> usize {
    let char_idx = lower[..lower_pos].chars().count();
    text.char_indices()
        .nth(char_idx)
        .map_or(text.len(), |(i, _)| i)
}

fn line_bounds(text: &str, byte_pos: usize) -> (usize, usize) {
    let start = text[..byte_pos].rfind('\n').map_or(0, |idx| idx + 1);
    let end = text[byte_pos..]
        .find('\n')
        .map_or(text.len(), |idx| byte_pos + idx);
    (start, end)
}

fn is_markdown_table_cell(text: &str, byte_pos: usize, pattern: &str) -> bool {
    let (line_start, line_end) = line_bounds(text, byte_pos);
    let line = &text[line_start..line_end];
    let trimmed = line.trim_start();
    if !(trimmed.starts_with('|')
        || trimmed.starts_with("+|")
        || trimmed.starts_with("-|")
        || trimmed.starts_with(">|"))
    {
        return false;
    }
    if line.matches('|').count() < 2 {
        return false;
    }

    let rel_pos = byte_pos.saturating_sub(line_start);
    let after_match = rel_pos + pattern.len();
    let prev_is_pipe = line[..rel_pos]
        .chars()
        .rev()
        .find(|c| !c.is_whitespace())
        .is_some_and(|c| c == '|');
    let next_is_pipe = line[after_match..]
        .chars()
        .find(|c| !c.is_whitespace())
        .is_some_and(|c| c == '|');
    prev_is_pipe && next_is_pipe
}

/// Check for rate limit / usage limit patterns in text.
pub(crate) fn detect_rate_limit(text: &str) -> Option<AgentError> {
    let lower = text.to_lowercase();
    let patterns = [
        "rate limit",
        "rate_limit",
        "ratelimit",
        "rate increased too quickly", // Alibaba / Qwen via OpenCode
        "too many requests",
        "usage limit",
        "session limit", // catches "You've hit your session limit · resets …"
        "weekly limit",  // catches "You've hit your weekly limit · resets …"
        "quota exceeded",
        "monthly quota", // GitHub Copilot: "You have exceeded your monthly quota"
        "overloaded",
        "capacity",
        "throttled",
        "insufficient_quota",
        "tokens_exceeded",
        "you've hit your usage limit",
        "resourceexhausted", // Nvidia / gRPC provider capacity
        "worker local total request limit reached", // Nvidia nemotron via OpenCode
    ];
    // Find the earliest valid match position so we can extract context around
    // the actual error message rather than the tail (which may be unrelated
    // JSON or markdown work product). Map the byte offset from `lower` back
    // to `text` via char-count to avoid using a lowercased byte index on the
    // original string (Unicode case-folding can change byte lengths, e.g.
    // İ→i̇).
    let match_info = patterns
        .iter()
        .filter_map(|pattern| lower.find(pattern).map(|lower_pos| (*pattern, lower_pos)))
        .filter_map(|(pattern, lower_pos)| {
            let byte_pos = lower_offset_to_original_byte(text, &lower, lower_pos);
            if matches!(pattern, "rate_limit" | "ratelimit")
                && !is_word_boundary(text, byte_pos, pattern)
            {
                return None;
            }

            if matches!(pattern, "rate_limit" | "ratelimit")
                && is_markdown_table_cell(text, byte_pos, pattern)
            {
                return None;
            }
            Some((pattern, byte_pos))
        })
        .min_by_key(|(_, byte_pos)| *byte_pos);
    // "429" only when neither neighbor is alphanumeric, so `"outputTokens":429` still
    // matches and a UUID group like `4295` or `-429c-` does not
    let pos_429 = find_standalone_digits(&lower, "429");
    let has_429 = pos_429.is_some();
    // HTTP 529 is used by Cloudflare for rate limiting. Only match in HTTP status
    // contexts (e.g. "HTTP 529", "status: 529") to avoid false positives from bare
    // numbers like line numbers, port numbers, or file sizes.
    let has_529 = lower.contains("http 529")
        || lower.contains("529 service")
        || (lower.find(": 529").is_some_and(|pos| {
            !lower[pos + 5..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        }));

    if match_info.is_some() || has_429 || has_529 {
        let message = if let Some((_, pos)) = match_info {
            extract_context_around(text, pos, 300)
        } else if let Some(pos) = pos_429 {
            extract_context_around(text, lower_offset_to_original_byte(text, &lower, pos), 300)
        } else {
            safe_tail(text, 300).to_string()
        };
        return Some(AgentError::RateLimit { message });
    }
    None
}

/// Extract up to `window` bytes of context centred around `byte_pos`.
///
/// `byte_pos` must be a valid byte offset in `text` (not derived from a
/// transformed copy).  Both ends are snapped to char boundaries to avoid
/// splitting multi-byte codepoints.
fn extract_context_around(text: &str, byte_pos: usize, window: usize) -> String {
    let half = window / 2;
    let start = byte_pos.saturating_sub(half);
    let end = (byte_pos + window).min(text.len());
    // Align both ends to char boundaries.
    let mut s = start;
    while s < text.len() && !text.is_char_boundary(s) {
        s += 1;
    }
    let mut e = end;
    while e < text.len() && !text.is_char_boundary(e) {
        e += 1;
    }
    text[s..e].to_string()
}

/// Byte offset of the first `digits` in `lower` with a non-alphanumeric (or string edge)
/// neighbour on both sides
fn find_standalone_digits(lower: &str, digits: &str) -> Option<usize> {
    let mut start = 0;
    while let Some(rel) = lower[start..].find(digits) {
        let pos = start + rel;
        let before_ok = pos == 0
            || !lower[..pos]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        let after = pos + digits.len();
        let after_ok = after >= lower.len()
            || !lower[after..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        if before_ok && after_ok {
            return Some(pos);
        }
        start = pos + 1;
    }
    None
}

/// Returns true if `lower` contains the HTTP status `code` (e.g. "401") as a
/// standalone number — not as part of a larger digit sequence like `4010292`.
///
/// Matches:
///   - `"http 401"` / `"http/1.1 401"`
///   - `"401 unauthorized"` / `"401\n"` / `"401"` at end-of-string
///   - `": 401"` when not immediately followed by another digit
pub(crate) fn contains_http_status(lower: &str, code: &str) -> bool {
    // Fast prefix checks that are inherently unambiguous.
    if lower.contains(&format!("http {code}")) || lower.contains(&format!("{code} ")) {
        return true;
    }
    // ": NNN" — only accept when the next character is not a digit.
    let needle = format!(": {code}");
    let mut start = 0;
    while let Some(rel) = lower[start..].find(needle.as_str()) {
        let after = start + rel + needle.len();
        match lower[after..].chars().next() {
            Some(c) if c.is_ascii_digit() => {
                // Part of a longer number (e.g. ": 4010292") — skip.
                start = after;
            }
            _ => return true,
        }
    }
    false
}

/// Check for auth / billing error patterns in text.
///
/// Returns the line containing the match (not the tail) so the stored
/// error message is the actual error string rather than trailing output
/// (e.g. NDJSON session metadata that agents emit at exit).
pub(crate) fn detect_auth_error(text: &str) -> Option<AgentError> {
    let lower = text.to_lowercase();
    let patterns = [
        "unauthorized",
        "authentication required",
        "invalid api",
        "invalid key",
        "invalid token",
        "auth fail",
        "no api key",
        "no token",
        "expired key",
        "expired token",
        "expired plan",
        // Use specific billing-related phrases instead of the bare word
        // "billing" which produced false positives when agents mentioned
        // billing in non-error contexts (e.g. reviews discussing billing).
        "billing plan",
        "billing error",
        "billing suspended",
        "billing expired",
        "billing limit",
        "billing failed",
        "billing cycle exhausted",
        "insufficient credit",
        "insufficient balance",
        "credit balance too low",
        "payment required",
    ];
    // Use standalone check for HTTP status codes (same rationale as
    // detect_rate_limit: avoid matching port numbers, line numbers, etc.).
    let http_401 = contains_http_status(&lower, "401");
    let http_403 = contains_http_status(&lower, "403");
    let http_407 = contains_http_status(&lower, "407");
    let proxy_auth_required = lower.contains("proxy authentication required");
    if !patterns.iter().any(|p| lower.contains(p))
        && !http_401
        && !http_403
        && !http_407
        && !proxy_auth_required
    {
        return None;
    }
    // Find the first auth error match position to extract its line.
    let first_match_pos = patterns
        .iter()
        .filter_map(|p| lower.find(p))
        .min()
        .map(|lower_pos| {
            let char_idx = lower[..lower_pos].chars().count();
            text.char_indices()
                .nth(char_idx)
                .map_or(text.len(), |(i, _)| i)
        });
    // Also check HTTP status codes for standalone matches (not caught by
    // pattern strings like "401" is too short to add as a pattern).
    let http_401_match_pos = if http_401 {
        lower.find("401 ").or_else(|| lower.find(": 401"))
    } else {
        None
    };
    let http_403_match_pos = if http_403 {
        lower.find("403 ").or_else(|| lower.find(": 403"))
    } else {
        None
    };
    let http_407_match_pos = if http_407 {
        lower.find("407 ").or_else(|| lower.find(": 407"))
    } else {
        None
    };
    let http_pos = [http_401_match_pos, http_403_match_pos, http_407_match_pos]
        .into_iter()
        .flatten()
        .min()
        .and_then(|rel| {
            let char_idx = lower[..rel].chars().count();
            text.char_indices().nth(char_idx).map(|(i, _)| i)
        });
    let match_pos = [first_match_pos, http_pos].into_iter().flatten().min();
    let message = if let Some(pos) = match_pos {
        find_line_containing(text, pos)
    } else {
        // Pattern matched but couldn't find position (e.g. 401 standalone).
        // Fall back to scanning last 50 lines for the error line.
        find_auth_line_scan(text).unwrap_or_else(|| safe_tail(text, 300).to_string())
    };
    Some(AgentError::Auth { message })
}

/// Returns the full line containing `byte_pos` in `text`.
fn find_line_containing(text: &str, byte_pos: usize) -> String {
    let start = text[..byte_pos].rfind('\n').map_or(0, |i| i + 1);
    let end = text[byte_pos..]
        .find('\n')
        .map_or(text.len(), |i| byte_pos + i);
    text[start..end].trim().to_string()
}

/// Scans the last 50 lines of `text` for a line containing an auth error
/// pattern (401, unauthorized, invalid api, etc.). Used as fallback when
/// match position is unavailable.
fn find_auth_line_scan(text: &str) -> Option<String> {
    for line in text.lines().rev().take(50) {
        let line_lower = line.to_lowercase();
        if contains_http_status(&line_lower, "401")
            || contains_http_status(&line_lower, "403")
            || contains_http_status(&line_lower, "407")
            || line_lower.contains("unauthorized")
            || line_lower.contains("authentication required")
            || line_lower.contains("invalid api")
            || line_lower.contains("invalid key")
            || line_lower.contains("invalid token")
            || line_lower.contains("auth fail")
            || line_lower.contains("no api key")
            || line_lower.contains("no token")
            || line_lower.contains("expired key")
            || line_lower.contains("expired token")
            || line_lower.contains("expired plan")
            || line_lower.contains("billing plan")
            || line_lower.contains("billing error")
            || line_lower.contains("billing suspended")
            || line_lower.contains("billing expired")
            || line_lower.contains("billing limit")
            || line_lower.contains("billing failed")
            || line_lower.contains("billing cycle exhausted")
            || line_lower.contains("insufficient credit")
            || line_lower.contains("credit balance too low")
            || line_lower.contains("payment required")
            || line_lower.contains("proxy authentication required")
        {
            return Some(line.trim().to_string());
        }
    }
    None
}

/// Check for transient network connectivity errors.
pub(crate) fn detect_network_error(text: &str) -> Option<AgentError> {
    let lower = text.to_lowercase();
    let patterns = [
        "connectionrefused",
        "connection refused",
        "unable to connect",
        "econnrefused",
        "network unreachable",
        // Socket / HTTP transport errors (Node fetch / undici / http module)
        "socket connection was closed",
        "socket hang up",
        "econnreset",
        "connection reset",
        "etimedout",
        "connect etimedout",
        "aborterror",
        "the operation was aborted",
        "network request failed",
        "fetch failed",
        // Upstream HTTP-layer timeouts from model provider APIs (e.g. opencode/nemotron)
        "upstream idle timeout",
        "streaming response failed",
        "upstream request failed",
        // Provider-side mid-stream disconnects (e.g. kimi/claude API errors)
        "stalled mid-stream",
        "connection closed mid-response",
    ];
    // Map the byte offset from `lower` back to `text` via char-count (same
    // rationale as detect_rate_limit: Unicode case-folding can change byte lengths).
    let match_pos = patterns
        .iter()
        .filter_map(|p| lower.find(p))
        .min()
        .map(|lower_pos| {
            let char_idx = lower[..lower_pos].chars().count();
            text.char_indices()
                .nth(char_idx)
                .map_or(text.len(), |(i, _)| i)
        });
    if let Some(pos) = match_pos {
        return Some(AgentError::NetworkError {
            message: extract_context_around(text, pos, 300),
        });
    }
    None
}

/// Check for a plain-text "model not found" / "model unavailable" error.
///
/// Mirrors the model-unavailable detection already present in codex.rs's
/// and opencode.rs's structured NDJSON classifiers (`classify_message`,
/// `classify_opencode_message`), for the case where the same failure
/// surfaces as raw stdout/stderr text instead of a recognizable NDJSON
/// event. Without this, the message falls through to `AgentError::Unknown`,
/// which bumps the agent-wide failure cooldown instead of the
/// model-scoped one that `ModelUnavailable` is exempted into.
pub(crate) fn detect_model_unavailable(text: &str) -> Option<AgentError> {
    let lower = text.to_lowercase();
    let is_model_unavailable = lower.contains("model metadata")
        || lower.contains("no endpoints found")
        || (lower.contains("model")
            && (lower.contains("not found")
                || lower.contains("not supported")
                || lower.contains("does not exist")
                || lower.contains("deprecated")
                || lower.contains("unavailable for free")
                || lower.contains("not available in your country")
                || lower.contains("not available in your region")));
    if !is_model_unavailable {
        return None;
    }
    let model = extract_quoted(text, '`')
        .or_else(|| extract_quoted(text, '\''))
        .unwrap_or_default();
    Some(AgentError::ModelUnavailable {
        message: safe_tail(text, 300).to_string(),
        model,
    })
}

/// Extract text between the first matching pair of `quote` characters.
fn extract_quoted(text: &str, quote: char) -> Option<String> {
    let start = text.find(quote)?;
    let rest = &text[start + quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// Check for missing worktree / working directory setup failures.
pub(crate) fn detect_worktree_missing(text: &str) -> Option<AgentError> {
    let lower = text.to_lowercase();
    let patterns = ["worktree directory does not exist"];
    if patterns.iter().any(|p| lower.contains(p)) {
        return Some(AgentError::AgentFailed {
            message: safe_tail(text, 300).to_string(),
        });
    }
    None
}

/// Check for context overflow patterns.
pub(crate) fn detect_context_overflow(text: &str) -> Option<AgentError> {
    let lower = text.to_lowercase();
    let patterns = [
        "context_length_exceeded",
        "context length exceeded",
        "maximum context length",
        "too many tokens",
        "token limit",
        "context overflow",
    ];
    if patterns.iter().any(|p| lower.contains(p)) {
        return Some(AgentError::ContextOverflow {
            message: safe_tail(text, 300).to_string(),
        });
    }
    None
}

/// Check for missing tooling patterns. Returns the tool name.
pub(crate) fn detect_missing_tool(text: &str) -> Option<AgentError> {
    let known_tools = [
        "bun",
        "node",
        "npm",
        "pnpm",
        "yarn",
        "deno",
        "tsc",
        "eslint",
        "prettier",
        "jest",
        "vitest",
        "cargo",
        "rustc",
        "go",
        "python",
        "python3",
        "pip",
        "pip3",
        "uv",
        "poetry",
        "pytest",
        "ruff",
        "black",
        "mypy",
        "make",
        "cmake",
        "ninja",
        "just",
        "bats",
        "docker",
        "docker-compose",
        "podman",
        "kubectl",
        "helm",
        "terraform",
        "anchor",
        "avm",
        "solana",
        "solana-test-validator",
    ];

    let lower = text.to_lowercase();
    for tool in &known_tools {
        if lower.contains(&format!("{tool}: command not found"))
            || lower.contains(&format!("command not found: {tool}"))
            || lower.contains(&format!("{tool}: not found"))
            || lower.contains(&format!("env: {tool}: no such file"))
            || lower.contains(&format!("spawn {tool} enoent"))
        {
            return Some(AgentError::MissingTool {
                tool: tool.to_string(),
            });
        }
    }
    None
}

/// Check for permission/sandbox denied patterns.
pub(crate) fn detect_permission_denied(text: &str) -> Option<AgentError> {
    let lower = text.to_lowercase();
    let patterns = [
        "permission denied",
        "rejected permission",
        "permissionerror",
        "operation not permitted",
        "sandbox violation",
        "access denied",
        "eacces",
        "eperm",
        "read-only file system",
        "not writable",
        "disallowed tool",
    ];
    if patterns.iter().any(|p| lower.contains(p)) {
        return Some(AgentError::PermissionDenied {
            message: safe_tail(text, 300).to_string(),
        });
    }
    None
}

/// Check for interactive input prompts (1Password, SSH passphrase, etc.).
pub(crate) fn detect_waiting_for_input(text: &str) -> Option<AgentError> {
    let lower = text.to_lowercase();
    let patterns = [
        "enter passphrase",
        "1password",
        "op signin",
        "ssh passphrase",
        "password:",
    ];
    let auth_required_with_interactive_context = lower.contains("authentication required")
        && (lower.contains("ssh")
            || lower.contains("passphrase")
            || lower.contains("1password")
            || lower.contains("op signin"));
    if patterns.iter().any(|p| lower.contains(p)) || auth_required_with_interactive_context {
        return Some(AgentError::WaitingForInput {
            message: safe_tail(text, 300).to_string(),
        });
    }
    None
}

/// Check for the Claude 400 "thinking blocks cannot be modified" API error.
///
/// Occurs when Claude CLI reconstructs a multi-turn tool-use request and the
/// Anthropic API rejects it because a thinking block was altered between turns.
/// Classified separately to avoid penalising the model with a failure count.
pub(crate) fn detect_thinking_block_conflict(text: &str) -> Option<AgentError> {
    // Match the exact phrasing the Anthropic API emits (backtick-quoted names,
    // case-sensitive — the API always uses this exact string).
    if text.contains(
        "thinking` or `redacted_thinking` blocks in the latest assistant message cannot be modified",
    ) {
        let message = text
            .lines()
            .find(|l| l.contains("cannot be modified")).map_or_else(|| safe_tail(text, 300).to_string(), |l| l.trim().to_string());
        return Some(AgentError::ThinkingBlockConflict { message });
    }
    None
}

/// Check for a stale/expired Claude session ID.
///
/// Claude returns `"No conversation found with session ID: <uuid>"` when
/// the session UUID stored in orch's KV has expired server-side. The caller
/// should reset the stored UUID and retry with a fresh `--session-id`.
pub(crate) fn detect_stale_session(text: &str) -> Option<AgentError> {
    // Case-insensitive match; extract the UUID if present.
    // We search the lowercased copy for the pattern, then find the
    // corresponding byte offset in the *original* text via char_indices so
    // we never use a byte index from `lower` (whose byte length may differ
    // from `text` after Unicode case-folding, e.g. ß→ss, İ→i̇).
    let needle = "no conversation found with session id";
    let lower = text.to_lowercase();
    if let Some(lower_pos) = lower.find(needle) {
        // Map the byte offset in `lower` back to a char index, then find
        // the same char index in `text`.  Because to_lowercase() maps each
        // char to one or more chars, the char count up to the match may
        // differ between `lower` and `text`, so we count chars in `lower`.
        let char_idx = lower[..lower_pos].chars().count();
        let pos = text
            .char_indices()
            .nth(char_idx)
            .map_or(text.len(), |(i, _)| i);
        // Try to extract the UUID that follows the colon.
        let after = &text[pos..];
        let session_id = after
            .split_once(':')
            .map(|(_, rest)| rest.trim())
            // Take only the UUID portion (up to whitespace or end)
            .map(|s| s.split_whitespace().next().unwrap_or("").to_string())
            .unwrap_or_default();
        return Some(AgentError::StaleSession { session_id });
    }
    None
}

/// Exit code returned by `timeout(1)` when the child is killed.
const TIMEOUT_EXIT_CODE: i32 = 124;

/// Default assumed timeout duration when we only have the exit code.
const DEFAULT_TIMEOUT_SECS: u64 = 1800;

/// How many bytes from the end of combined stdout+stderr we scan for
/// CLI-style error patterns that should come from the process tail.
///
/// Rationale: real CLI error messages (rate limits, auth failures, network
/// failures) almost always appear at the end of the process output.
/// Scanning only the tail avoids false-positives from agent work product
/// (code, diffs, commit messages) which may mention "quota",
/// "unauthorized", HTTP status codes, etc. 3000 bytes is a conservative
/// window large enough to capture multi-line error dumps but small enough
/// to exclude most long-form agent outputs.
pub(crate) const RATE_LIMIT_SCAN_TAIL_BYTES: usize = 3000;

/// How many bytes from the start of combined stdout+stderr we scan for
/// rate limit patterns that appear as early system events.
///
/// Some rate limit errors (e.g., Claude session limit) appear as
/// `type:system` NDJSON events early in the stream, not in the tail.
/// Scanning the head catches these before they get pushed out of the
/// tail window by subsequent agent output.
pub(crate) const RATE_LIMIT_SCAN_HEAD_BYTES: usize = 3000;

/// Run all pattern detectors against combined stdout+stderr.
/// Returns the first matching AgentError, or a generic Unknown.
pub(crate) fn classify_from_text(exit_code: i32, text: &str) -> AgentError {
    if exit_code == TIMEOUT_EXIT_CODE {
        return AgentError::Timeout {
            elapsed: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        };
    }

    // Exit 0 with empty output is a silent failure (common with GitHub Copilot
    // models in opencode). Return Unknown with a deterministic message so the
    // caller (fallback.rs) detects it and applies model cooldown + free-model
    // retry instead of treating it as a generic success.
    if exit_code == 0 && text.trim().is_empty() {
        return AgentError::Unknown {
            exit_code,
            message: "empty-output-exit0".to_string(),
        };
    }

    if let Some(e) = detect_missing_tool(text) {
        return e;
    }
    if let Some(e) = detect_worktree_missing(text) {
        return e;
    }
    if let Some(e) = detect_thinking_block_conflict(text) {
        return e;
    }
    // Also scan the head of the combined output for rate limit patterns.
    // Some rate limit errors (e.g., Claude session limit) appear as
    // `type:system` NDJSON events early in the stream, not in the tail.
    // Scanning the head catches these before they get pushed out of the
    // tail window by subsequent agent output.
    let scan_head = safe_head(text, RATE_LIMIT_SCAN_HEAD_BYTES);
    if let Some(e) = detect_rate_limit(scan_head) {
        return e;
    }
    // Only scan the tail of the combined output for CLI-style error
    // patterns. The full output may contain agent work product (code,
    // diffs, commit messages) that incidentally mentions rate limiting,
    // authorization, HTTP status codes, or context/auth errors. Real
    // process errors appear at the end of the output. Scan a bounded tail
    // (`RATE_LIMIT_SCAN_TAIL_BYTES`) to reduce false positives.
    let scan_tail = safe_tail(text, RATE_LIMIT_SCAN_TAIL_BYTES);
    if let Some(e) = detect_context_overflow(scan_tail) {
        return e;
    }
    if let Some(e) = detect_rate_limit(scan_tail) {
        return e;
    }
    if let Some(e) = detect_model_unavailable(scan_tail) {
        return e;
    }
    if let Some(e) = detect_network_error(scan_tail) {
        return e;
    }
    if let Some(e) = detect_auth_error(scan_tail) {
        return e;
    }
    if let Some(e) = detect_waiting_for_input(scan_tail) {
        return e;
    }
    if let Some(e) = detect_permission_denied(scan_tail) {
        return e;
    }
    if let Some(e) = detect_stale_session(text) {
        return e;
    }

    AgentError::Unknown {
        exit_code,
        message: safe_tail(text, 300).to_string(),
    }
}

/// Safely extract the last `max_bytes` of a string, respecting UTF-8 boundaries.
pub(crate) fn safe_tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let start = text.len() - max_bytes;
    // Walk forward to find a char boundary
    let mut idx = start;
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    &text[idx..]
}

/// Safely extract the first `max_bytes` of a string, respecting UTF-8 boundaries.
pub(crate) fn safe_head(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut idx = max_bytes;
    // Walk backward to find a char boundary
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    &text[..idx]
}

/// Scan NDJSON events for errors using agent-specific extraction and classification.
///
/// `extract_message` is called for each event to extract an error message string.
/// If it returns `Some`, the message is classified via `classify`. The most
/// specific error is kept — any typed error (RateLimit, Auth, etc.) takes
/// precedence over a generic `AgentFailed`.
pub(crate) fn detect_ndjson_error(
    events: &[serde_json::Value],
    extract_message: impl Fn(&serde_json::Value) -> Option<String>,
    classify: impl Fn(&str) -> AgentError,
) -> Option<AgentError> {
    let mut best: Option<AgentError> = None;

    for event in events {
        if let Some(message) = extract_message(event) {
            let err = classify(&message);
            // Priority (highest wins): RateLimit / Auth / other > NetworkError > AgentFailed
            // A transient reconnect mid-stream must not obscure a subsequent billing/rate error.
            let upgrade = match &best {
                None | Some(AgentError::AgentFailed { .. }) => true,
                Some(AgentError::NetworkError { .. }) => !matches!(
                    err,
                    AgentError::NetworkError { .. } | AgentError::AgentFailed { .. }
                ),
                Some(_) => false,
            };
            if upgrade {
                best = Some(err);
            }
        }
    }

    best
}

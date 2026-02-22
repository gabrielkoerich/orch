//! Security utilities — leak detection for agent output.
//!
//! Before posting agent output to GitHub (public), scan for leaked secrets.
//! Inspired by spacebot's approach. Catches API keys, tokens, passwords,
//! private keys, and connection strings before they hit the internet.

use regex::Regex;
use std::sync::LazyLock;

/// A detected secret in agent output.
#[derive(Debug, Clone)]
pub struct LeakMatch {
    pub rule: &'static str,
    pub line: usize,
    pub redacted: String,
}

/// Patterns that indicate leaked secrets.
/// Each tuple: (rule_name, regex_pattern, is_high_confidence)
static LEAK_PATTERNS: LazyLock<Vec<(&str, Regex, bool)>> = LazyLock::new(|| {
    vec![
        // API keys and tokens
        (
            "aws_access_key",
            Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
            true,
        ),
        (
            "aws_secret_key",
            Regex::new(r"(?i)aws[_\-]?secret[_\-]?access[_\-]?key\s*[=:]\s*\S+").unwrap(),
            true,
        ),
        (
            "github_token",
            Regex::new(r"gh[pousr]_[A-Za-z0-9_]{36,}").unwrap(),
            true,
        ),
        (
            "github_pat",
            Regex::new(r"github_pat_[A-Za-z0-9_]{22,}").unwrap(),
            true,
        ),
        (
            "openai_api_key",
            Regex::new(r"sk-[A-Za-z0-9\-]{20,}").unwrap(),
            true,
        ),
        (
            "anthropic_api_key",
            Regex::new(r"sk-ant-[A-Za-z0-9\-]{20,}").unwrap(),
            true,
        ),
        (
            "slack_token",
            Regex::new(r"xox[baprs]-[0-9A-Za-z\-]{10,}").unwrap(),
            true,
        ),
        (
            "stripe_key",
            Regex::new(r"[sr]k_(live|test)_[A-Za-z0-9]{20,}").unwrap(),
            true,
        ),
        (
            "telegram_bot_token",
            Regex::new(r"\d{8,10}:[A-Za-z0-9_-]{35}").unwrap(),
            false, // lower confidence, could be other things
        ),
        // Private keys
        (
            "private_key",
            Regex::new(r"-----BEGIN (RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----").unwrap(),
            true,
        ),
        // Generic patterns (lower confidence)
        (
            "generic_secret",
            Regex::new(r#"(?i)(password|secret|token|api[_\-]?key)\s*[=:]\s*["']?[A-Za-z0-9+/=_\-]{16,}["']?"#).unwrap(),
            false,
        ),
        (
            "connection_string",
            Regex::new(r"(?i)(postgres|mysql|mongodb|redis)://[^\s]{10,}").unwrap(),
            true,
        ),
        (
            "bearer_token",
            Regex::new(r"(?i)bearer\s+[A-Za-z0-9\-._~+/]+=*").unwrap(),
            false,
        ),
    ]
});

/// Scan text for potential leaked secrets.
///
/// Returns a list of matches with rule name, line number, and redacted preview.
/// Use `has_leaks()` for a simple boolean check.
pub fn scan(text: &str) -> Vec<LeakMatch> {
    let mut matches = Vec::new();

    for (line_num, line) in text.lines().enumerate() {
        // Skip lines that look like code comments explaining patterns
        let trimmed = line.trim();
        if trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("<!--")
            || trimmed.starts_with("* ")
        {
            continue;
        }

        for (rule, pattern, _high_conf) in LEAK_PATTERNS.iter() {
            if let Some(m) = pattern.find(line) {
                let matched = m.as_str();
                // Redact: show first 4 chars + ... + last 2 chars
                let redacted = if matched.len() > 8 {
                    format!("{}...{}", &matched[..4], &matched[matched.len() - 2..])
                } else {
                    "****".to_string()
                };

                matches.push(LeakMatch {
                    rule,
                    line: line_num + 1,
                    redacted,
                });
            }
        }
    }

    matches
}

/// Quick check: does this text contain any leaked secrets?
pub fn has_leaks(text: &str) -> bool {
    !scan(text).is_empty()
}

/// Check only high-confidence patterns (fewer false positives).
pub fn has_high_confidence_leaks(text: &str) -> bool {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with("<!--") {
            continue;
        }

        for (_rule, pattern, high_conf) in LEAK_PATTERNS.iter() {
            if *high_conf && pattern.is_match(line) {
                return true;
            }
        }
    }
    false
}

/// Redact all detected secrets in text, replacing them with `[REDACTED:{rule}]`.
pub fn redact(text: &str) -> String {
    let mut result = text.to_string();

    for (rule, pattern, _) in LEAK_PATTERNS.iter() {
        result = pattern
            .replace_all(&result, format!("[REDACTED:{rule}]"))
            .to_string();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_github_token() {
        let text = "export GITHUB_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let matches = scan(text);
        assert!(!matches.is_empty());
        assert_eq!(matches[0].rule, "github_token");
    }

    #[test]
    fn detects_aws_key() {
        let text = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
        let matches = scan(text);
        assert!(!matches.is_empty());
        assert_eq!(matches[0].rule, "aws_access_key");
    }

    #[test]
    fn detects_openai_key() {
        let text = "OPENAI_API_KEY=sk-proj-1234567890abcdefghijklmn";
        let matches = scan(text);
        assert!(matches.iter().any(|m| m.rule == "openai_api_key"));
    }

    #[test]
    fn detects_private_key() {
        let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQ...";
        assert!(has_leaks(text));
    }

    #[test]
    fn detects_connection_string() {
        let text = "DATABASE_URL=postgres://user:pass@host:5432/mydb";
        assert!(has_leaks(text));
    }

    #[test]
    fn ignores_comments() {
        let text = "// Example: OPENAI_API_KEY=sk-proj-1234567890abcdefghijklmn";
        let matches = scan(text);
        assert!(matches.is_empty());
    }

    #[test]
    fn redacts_secrets() {
        let text = "token: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let redacted = redact(text);
        assert!(redacted.contains("[REDACTED:github_token]"));
        assert!(!redacted.contains("ghp_"));
    }

    #[test]
    fn clean_text_has_no_leaks() {
        let text = "This is normal agent output.\nFixed bug in parser.rs\nAll tests pass.";
        assert!(!has_leaks(text));
    }
}

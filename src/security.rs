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
struct LeakPatternSpec {
    rule: &'static str,
    pattern: &'static str,
    high_confidence: bool,
}

const LEAK_PATTERN_SPECS: &[LeakPatternSpec] = &[
    // API keys and tokens
    LeakPatternSpec {
        rule: "aws_access_key",
        pattern: r"AKIA[0-9A-Z]{16}",
        high_confidence: true,
    },
    LeakPatternSpec {
        rule: "aws_secret_key",
        pattern: r"(?i)aws[_\-]?secret[_\-]?access[_\-]?key\s*[=:]\s*\S+",
        high_confidence: true,
    },
    LeakPatternSpec {
        rule: "github_token",
        pattern: r"gh[pousr]_[A-Za-z0-9_]{36,}",
        high_confidence: true,
    },
    LeakPatternSpec {
        rule: "github_pat",
        pattern: r"github_pat_[A-Za-z0-9_]{22,}",
        high_confidence: true,
    },
    LeakPatternSpec {
        rule: "openai_api_key",
        pattern: r"sk-[A-Za-z0-9\-]{20,}",
        high_confidence: true,
    },
    LeakPatternSpec {
        rule: "anthropic_api_key",
        pattern: r"sk-ant-[A-Za-z0-9\-]{20,}",
        high_confidence: true,
    },
    LeakPatternSpec {
        rule: "slack_token",
        pattern: r"xox[baprs]-[0-9A-Za-z\-]{10,}",
        high_confidence: true,
    },
    LeakPatternSpec {
        rule: "stripe_key",
        pattern: r"[sr]k_(live|test)_[A-Za-z0-9]{20,}",
        high_confidence: true,
    },
    LeakPatternSpec {
        rule: "telegram_bot_token",
        pattern: r"\d{8,10}:[A-Za-z0-9_-]{35}",
        high_confidence: false,
    },
    // Private keys
    LeakPatternSpec {
        rule: "private_key",
        pattern: r"-----BEGIN (RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----",
        high_confidence: true,
    },
    // Generic patterns (lower confidence)
    LeakPatternSpec {
        rule: "generic_secret",
        pattern: r#"(?i)(password|secret|token|api[_\-]?key)\s*[=:]\s*["']?[A-Za-z0-9+/=_\-]{16,}["']?"#,
        high_confidence: false,
    },
    LeakPatternSpec {
        rule: "connection_string",
        pattern: r"(?i)(postgres|mysql|mongodb|redis)://[^\s]{10,}",
        high_confidence: true,
    },
    LeakPatternSpec {
        rule: "bearer_token",
        pattern: r"(?i)bearer\s+[A-Za-z0-9\-._~+/]+=*",
        high_confidence: false,
    },
];

fn build_leak_patterns(specs: &[LeakPatternSpec]) -> Vec<(&'static str, Regex, bool)> {
    let mut patterns = Vec::new();

    for spec in specs {
        match Regex::new(spec.pattern) {
            Ok(regex) => patterns.push((spec.rule, regex, spec.high_confidence)),
            Err(err) => {
                tracing::error!(
                    rule = spec.rule,
                    pattern = spec.pattern,
                    error = %err,
                    "failed to compile leak detection regex"
                );
            }
        }
    }

    patterns
}

static LEAK_PATTERNS: LazyLock<Vec<(&str, Regex, bool)>> =
    LazyLock::new(|| build_leak_patterns(LEAK_PATTERN_SPECS));

// Previously we skipped lines that looked like code comments which could
// hide real secrets (eg. `# GITHUB_TOKEN=ghp_...`). That produced a confusing
// situation where `has_leaks()` returned true but `scan()` returned 0 matches.
// Align behavior: do not skip comment-like lines in `scan()` — check every
// line so `scan()` and `has_leaks()` are consistent.

/// Scan text for potential leaked secrets.
///
/// Returns a list of matches with rule name, line number, and redacted preview.
/// Use `has_leaks()` for a simple boolean check.
pub fn scan(text: &str) -> Vec<LeakMatch> {
    let mut matches = Vec::new();

    for (line_num, line) in text.lines().enumerate() {
        for (rule, pattern, _high_conf) in LEAK_PATTERNS.iter() {
            if let Some(m) = pattern.find(line) {
                let matched = m.as_str();
                // Redact: show first 4 chars + ... + last 2 chars (safe UTF-8 boundaries)
                let redacted = if matched.len() > 8 {
                    let head_end = (0..=4)
                        .rev()
                        .find(|&i| matched.is_char_boundary(i))
                        .unwrap_or(0);
                    let tail_start = (matched.len() - 2..=matched.len())
                        .find(|&i| matched.is_char_boundary(i))
                        .unwrap_or(matched.len());
                    format!("{}...{}", &matched[..head_end], &matched[tail_start..])
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
/// All patterns are checked regardless of confidence — nothing is posted to GitHub
/// if any pattern matches.
pub fn has_leaks(text: &str) -> bool {
    LEAK_PATTERNS
        .iter()
        .any(|(_, pattern, _)| pattern.is_match(text))
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
        // We no longer skip comment-like lines in `scan()`; secrets in comments
        // should be detected just like in normal text so `has_leaks()` and
        // `scan()` remain consistent.
        let matches = scan(text);
        assert!(!matches.is_empty());
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

    #[test]
    fn scans_markdown_bullets() {
        // Markdown bullet lines must NOT be skipped — they're content, not code comments.
        let text = "* token: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let matches = scan(text);
        assert!(!matches.is_empty());
    }

    #[test]
    fn build_leak_patterns_skips_invalid_regex() {
        let specs = [
            LeakPatternSpec {
                rule: "good",
                pattern: r"good",
                high_confidence: true,
            },
            LeakPatternSpec {
                rule: "bad",
                pattern: r"(",
                high_confidence: false,
            },
        ];

        let patterns = build_leak_patterns(&specs);

        assert!(patterns.iter().any(|(rule, _, _)| *rule == "good"));
        assert!(!patterns.iter().any(|(rule, _, _)| *rule == "bad"));
    }

    #[test]
    fn has_leaks_triggers_on_low_confidence_patterns() {
        let text = "password = \"some_long_config_value_that_is_16_chars\"";
        assert!(
            has_leaks(text),
            "generic_secret (low confidence) should trigger has_leaks"
        );

        let text = "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0";
        assert!(
            has_leaks(text),
            "bearer_token (low confidence) should trigger has_leaks"
        );
    }

    #[test]
    fn has_leaks_triggers_on_high_confidence_patterns() {
        let text = "password = \"sk-proj-1234567890abcdefghijklmnopqrstuv\"";
        assert!(
            has_leaks(text),
            "openai_api_key (high confidence) should trigger has_leaks"
        );
    }
}

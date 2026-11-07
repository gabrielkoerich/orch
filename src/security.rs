//! Leak detection for agent output before it is posted to GitHub. It uses the strict rules of
//! `secret-redact`, so a low-confidence pattern blocks a post as much as a known token does

use secret_redact::Finding;

/// Every leaked secret in `text`, with its rule and line
pub fn scan(text: &str) -> Vec<Finding> {
    secret_redact::strict().scan(text)
}

/// Whether `text` holds anything that looks like a secret. Nothing is posted to GitHub when it does
pub fn has_leaks(text: &str) -> bool {
    secret_redact::strict().contains_secret(text)
}

/// `text` with every secret replaced by `[REDACTED:{rule}]`
pub fn redact(text: &str) -> String {
    secret_redact::strict().redact(text).text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_and_redacts_a_github_token() {
        let text = "export GITHUB_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        assert_eq!(scan(text)[0].rule, "github-pat");
        let redacted = redact(text);
        assert!(redacted.contains("[REDACTED:github-pat]"), "{redacted}");
        assert!(!redacted.contains("ghp_"), "{redacted}");
    }

    #[test]
    fn detects_private_keys_connection_strings_and_example_keys() {
        assert!(has_leaks(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQ..."
        ));
        assert!(has_leaks(
            "DATABASE_URL=postgres://user:pass@host:5432/mydb"
        ));
        assert!(has_leaks("AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn clean_text_has_no_leaks() {
        let text = "This is normal agent output.\nFixed bug in parser.rs\nAll tests pass.";
        assert!(!has_leaks(text));
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
}

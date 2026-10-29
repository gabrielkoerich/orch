//! Config values that name a secret instead of holding it, so the config file holds no secret.
//! `passbox:<name>` is read once through passbox, any other value is used as written

/// Blocking, it waits on passbox's Touch ID prompt, so call it from `spawn_blocking`
pub fn resolve(value: &str) -> anyhow::Result<String> {
    resolve_with("passbox", value)
}

fn resolve_with(passbox: &str, value: &str) -> anyhow::Result<String> {
    let Some(name) = value.strip_prefix("passbox:") else {
        return Ok(value.to_string());
    };

    let out = std::process::Command::new(passbox)
        .args(["get", name, "--reveal"])
        .output()?;
    anyhow::ensure!(out.status.success(), "passbox could not read {name}");

    // The first line is the secret, later `key: value` lines are fields
    let text = String::from_utf8(out.stdout)?;
    let secret = text.lines().next().unwrap_or_default().to_string();
    anyhow::ensure!(!secret.is_empty(), "passbox returned no value for {name}");
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub(dir: &std::path::Path, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("passbox");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_plain_value_is_used_as_written() {
        assert_eq!(resolve_with("/nonexistent", "plain").unwrap(), "plain");
    }

    #[test]
    fn a_passbox_reference_reads_the_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(
            dir.path(),
            r#"[ "$2" = "orch/relay-token" ] && printf 's3cret\nnote: x\n'"#,
        );
        assert_eq!(
            resolve_with(&bin, "passbox:orch/relay-token").unwrap(),
            "s3cret"
        );
    }

    #[test]
    fn a_failed_or_empty_read_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_with(&stub(dir.path(), "exit 1"), "passbox:x").is_err());
        let empty = tempfile::tempdir().unwrap();
        assert!(resolve_with(&stub(empty.path(), "true"), "passbox:x").is_err());
    }
}

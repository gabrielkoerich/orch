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

    // `exec --stdin` hands the value to a child, so it works without a terminal and does not
    // depend on a flag of `get` that passbox versions differ on
    let out = std::process::Command::new(passbox)
        .args(["exec", "--stdin", name, "--", "/bin/cat"])
        .stdin(std::process::Stdio::null())
        .output()?;

    if !out.status.success() {
        let reason = String::from_utf8_lossy(&out.stderr);
        let reason = reason.lines().next().unwrap_or("no error output").trim();
        anyhow::bail!("passbox could not read {name}: {reason}");
    }

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
            r#"[ "$1 $2 $3 $4 $5" = "exec --stdin orch/relay-token -- /bin/cat" ] && printf 's3cret\nnote: x\n'"#,
        );
        assert_eq!(
            resolve_with(&bin, "passbox:orch/relay-token").unwrap(),
            "s3cret"
        );
    }

    #[test]
    fn a_failed_or_empty_read_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_with(
            &stub(dir.path(), "echo 'error: denied' >&2; exit 1"),
            "passbox:x",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("passbox could not read x: error: denied"),
            "{err}"
        );
        let empty = tempfile::tempdir().unwrap();
        assert!(resolve_with(&stub(empty.path(), "true"), "passbox:x").is_err());
    }
}

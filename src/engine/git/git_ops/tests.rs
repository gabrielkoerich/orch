use super::pr::build_pr_body;
use super::pr::extract_github_http_status;
use super::push::LEASE_ABSENT;
use super::push::LEASE_FALLBACKS;
use super::push::expected_force_sha;
use super::push::is_github_branch_update;
use super::push::is_stale_lease_error;
use super::push::push_needs_rebase;
use super::push::read_push_lease;
use super::push::resolve_push_branch;
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process;
use std::sync::atomic::Ordering;

use super::*;
use crate::engine::git::commit::has_changes;

#[test]
fn pr_create_error_display() {
    let err = PrCreateError::ApiError(anyhow::anyhow!("test error"));
    assert_eq!(format!("{err}"), "GitHub API error: test error");
}

#[test]
fn pr_create_error_from_anyhow() {
    let anyhow_err = anyhow::anyhow!("source error");
    let pr_err: PrCreateError = anyhow_err.into();
    assert!(matches!(pr_err, PrCreateError::ApiError(_)));
}

#[test]
fn git_env_never_signs_with_or_without_a_token() {
    for env in [
        build_git_auth_env_for_token(None),
        build_git_auth_env_for_token(Some("ghp_testtoken1234".into())),
    ] {
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let count: usize = get("GIT_CONFIG_COUNT").unwrap().parse().unwrap();
        let config: Vec<(String, String)> = (0..count)
            .map(|n| {
                (
                    get(&format!("GIT_CONFIG_KEY_{n}")).unwrap(),
                    get(&format!("GIT_CONFIG_VALUE_{n}")).unwrap(),
                )
            })
            .collect();
        assert!(config.contains(&("commit.gpgsign".into(), "false".into())));
        assert!(config.contains(&("tag.gpgsign".into(), "false".into())));
    }
}

#[test]
fn build_git_auth_env_without_token_falls_back_to_ssh_https_conversion() {
    // Pass None directly to test the no-token path without relying on env vars
    // or the gh CLI, keeping the test deterministic regardless of local auth state.
    let env = build_git_auth_env_for_token(None);

    // When no token is available the fallback must include the SSH insteadOf rule.
    // The SSH→HTTPS conversion sets the target URL (the rewrite destination) to git@github.com:.
    let has_ssh_rule = env
        .iter()
        .any(|(k, v)| k == "GIT_CONFIG_VALUE_0" && v == "git@github.com:");
    assert!(
        has_ssh_rule,
        "expected SSH insteadOf fallback (value is git@github.com:), got: {env:?}"
    );
}

#[test]
fn build_git_auth_env_with_token_covers_both_ssh_and_https() {
    // Pass a pre-built token directly to test the token path without touching env vars.
    let env = build_git_auth_env_for_token(Some("ghp_testtoken1234".into()));

    // Must contain the token in the auth URL (the token is in the value of GIT_CONFIG_KEY_N).
    let has_token = env
        .iter()
        .any(|(_, v)| v.contains("x-access-token:ghp_testtoken1234@github.com"));
    assert!(has_token, "expected token in auth URL, got: {env:?}");
    // Must cover HTTPS origins (GIT_CONFIG_VALUE_0 is the HTTPS source URL).
    let has_https_rule = env
        .iter()
        .any(|(k, v)| k == "GIT_CONFIG_VALUE_0" && v == "https://github.com/");
    assert!(
        has_https_rule,
        "expected HTTPS insteadOf rule, got: {env:?}"
    );
    // Must cover SSH origins (GIT_CONFIG_VALUE_1 is the SSH source URL).
    let has_ssh_rule = env
        .iter()
        .any(|(k, v)| k == "GIT_CONFIG_VALUE_1" && v == "git@github.com:");
    assert!(has_ssh_rule, "expected SSH insteadOf rule, got: {env:?}");
}

#[test]
fn find_stash_ref_by_hash_returns_matching_reference() {
    let stash_list = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa stash@{1}\nbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb stash@{0}";

    assert_eq!(
        find_stash_ref_by_hash(stash_list, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        Some("stash@{1}".to_string())
    );
    assert_eq!(
        find_stash_ref_by_hash(stash_list, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        Some("stash@{0}".to_string())
    );
}

#[test]
fn find_stash_ref_by_hash_returns_none_when_missing_or_malformed() {
    let stash_list = "malformed-line\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa stash@{2}";

    assert_eq!(
        find_stash_ref_by_hash(stash_list, "cccccccccccccccccccccccccccccccccccccccc"),
        None
    );
    assert_eq!(find_stash_ref_by_hash("", "anything"), None);
}

#[test]
fn push_needs_rebase_detects_common_rejections() {
    assert!(push_needs_rebase(
        "! [rejected] branch -> branch (fetch first)"
    ));
    assert!(push_needs_rebase("non-fast-forward update was rejected"));
    assert!(push_needs_rebase(
        "push rejected because the remote contains work"
    ));
}

#[test]
fn is_transient_network_error_detects_lfs_locks_timeout() {
    let lfs_err = "Post \"https://x-access-token:***@github.com/o/r.git/info/lfs/locks/verify\": dial tcp 140.82.112.4:443: i/o timeout";
    assert!(is_transient_network_error(lfs_err));
    assert!(is_transient_network_error(
        "dial tcp 140.82.112.4:443: connect: connection refused"
    ));
    assert!(is_transient_network_error(
        "read tcp 10.0.0.1:51234->140.82.112.4:443: read: connection reset by peer"
    ));
}

#[test]
fn is_transient_network_error_rejects_ref_rejections() {
    assert!(!is_transient_network_error(
        "! [rejected] branch -> branch (fetch first)"
    ));
    assert!(!is_transient_network_error(
        "remote: error: GH006: Protected branch update failed"
    ));
}

#[test]
fn is_transient_network_error_detects_github_remote_failure() {
    assert!(is_transient_network_error(
        "remote: fatal error in commit_refs\n ! [remote rejected] b -> b (failure)"
    ));
    assert!(is_transient_network_error(
        "! [remote rejected] b -> b (failure)"
    ));
}

#[test]
fn missing_remote_ref_error_is_detected() {
    let err = anyhow::anyhow!("failed to fetch origin/b: fatal: couldn't find remote ref b");
    assert!(is_missing_remote_ref_error(&err));
    assert!(!is_missing_remote_ref_error(&anyhow::anyhow!("conflict")));
}

#[test]
fn sanitize_push_error_strips_url_credentials() {
    // Shape must not look like a real GitHub PAT so gitleaks stays clean
    let raw = "Post \"https://x-access-token:test-secret-1234567890abcdef@github.com/o/r.git/info/lfs/locks/verify\": dial tcp 140.82.112.4:443: i/o timeout";
    let sanitized = sanitize_push_error(raw);
    assert!(!sanitized.contains("test-secret-1234567890abcdef"));
    assert!(sanitized.contains("://***@github.com"));
    assert!(sanitized.contains("i/o timeout"));
}

#[test]
fn sanitize_push_error_strips_fetch_url_credentials() {
    let raw = "fatal: unable to access 'https://x-access-token:test-secret-1234567890abcdef@github.com/o/r.git/': The requested URL returned error: 403";
    let sanitized = sanitize_push_error(raw);
    assert!(!sanitized.contains("test-secret-1234567890abcdef"));
    assert!(sanitized.contains("://***@github.com"));
    assert!(sanitized.contains("403"));
    assert_eq!(sanitized, sanitize_push_error(&sanitized));
}

#[cfg(unix)]
#[test]
fn sanitize_git_output_strips_url_credentials_from_both_streams() {
    let output = process::Output {
        status: process::ExitStatus::from_raw(0),
        stdout: b"To https://x-access-token:test-secret-1234567890abcdef@github.com/o/r.git\n"
            .to_vec(),
        stderr: b"fatal: unable to access 'https://x-access-token:test-secret-1234567890abcdef@github.com/o/r.git/'\n"
            .to_vec(),
    };
    let sanitized = sanitize_git_output(output);
    let stdout = String::from_utf8_lossy(&sanitized.stdout);
    let stderr = String::from_utf8_lossy(&sanitized.stderr);
    assert!(!stdout.contains("test-secret-1234567890abcdef"));
    assert!(!stderr.contains("test-secret-1234567890abcdef"));
    assert!(stdout.contains("://***@github.com"));
    assert!(stderr.contains("://***@github.com"));
}

#[test]
fn extract_github_http_status_parses_ghhttp_error_format() {
    let err =
        "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (502): Bad Gateway";
    assert_eq!(extract_github_http_status(err), Some(502));

    let err500 = "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (500): Internal Server Error";
    assert_eq!(extract_github_http_status(err500), Some(500));

    let err422 = "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (422): Unprocessable Entity";
    assert_eq!(extract_github_http_status(err422), Some(422));

    let not_github = "some other error";
    assert_eq!(extract_github_http_status(not_github), None);
}

#[test]
fn is_transient_github_error_matches_5xx_only() {
    let err502 =
        "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (502): Bad Gateway";
    assert!(is_transient_github_error(err502));

    let err503 = "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (503): Service Unavailable";
    assert!(is_transient_github_error(err503));

    let err422 = "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (422): Unprocessable Entity";
    assert!(!is_transient_github_error(err422));

    let err404 =
        "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (404): Not Found";
    assert!(!is_transient_github_error(err404));
}

#[test]
fn is_transient_github_error_matches_connection_errors() {
    // Connection-related transient errors
    assert!(is_transient_github_error("connection refused"));
    assert!(is_transient_github_error("Connection reset by peer"));
    assert!(is_transient_github_error("broken pipe"));
    assert!(is_transient_github_error("connection closed unexpectedly"));
    assert!(is_transient_github_error(
        "transport error: connection lost"
    ));

    // Git/Curl connect-failure patterns (review push failures)
    assert!(is_transient_github_error(
        "fatal: unable to access 'https://github.com/owner/repo.git/': Failed to connect to github.com port 443 after 20990 ms: Couldn't connect to server"
    ));
    assert!(is_transient_github_error(
        "Failed to connect to github.com port 443: Connection timed out"
    ));
    assert!(is_transient_github_error(
        "Failed to connect to github.com port 443: Connection refused"
    ));
    assert!(is_transient_github_error(
        "curl: (7) Failed to connect to github.com port 443: Couldn't connect to server"
    ));
    assert!(is_transient_github_error(
        "fatal: unable to access 'https://github.com/owner/repo.git/': Couldn't connect to server"
    ));
}

#[test]
fn is_transient_github_error_matches_dns_errors() {
    // DNS resolution failures
    assert!(is_transient_github_error("dns error: failed to lookup"));
    assert!(is_transient_github_error("failed to resolve hostname"));
    assert!(is_transient_github_error("name resolution failed"));
    assert!(is_transient_github_error("no such host: api.github.com"));
}

#[test]
fn is_transient_github_error_matches_tls_errors() {
    // TLS/SSL handshake failures
    assert!(is_transient_github_error("tls handshake failed"));
    assert!(is_transient_github_error("certificate verification failed"));
    assert!(is_transient_github_error("ssl error: unknown ca"));
}

#[test]
fn is_transient_github_error_matches_network_unreachable() {
    // Network unreachable errors
    assert!(is_transient_github_error("network is unreachable"));
    assert!(is_transient_github_error("host unreachable"));
    assert!(is_transient_github_error(
        "temporary failure in name resolution"
    ));
}

#[test]
fn is_transient_github_error_matches_eof_errors() {
    // EOF/connection dropped errors
    assert!(is_transient_github_error("unexpected eof while reading"));
    assert!(is_transient_github_error("unexpected end of file"));
}

#[test]
fn is_transient_github_error_matches_rate_limit() {
    // Rate limit errors (transient with cooldown)
    assert!(is_transient_github_error("rate limit exceeded"));
    assert!(is_transient_github_error("429 Too Many Requests"));
    assert!(is_transient_github_error("too many requests, retry later"));
}

#[test]
fn is_transient_github_error_matches_timeout() {
    // Timeout errors
    assert!(is_transient_github_error("operation timeout"));
    assert!(is_transient_github_error("request timeout"));
}

#[test]
fn is_transient_github_error_rejects_non_transient() {
    // Non-transient errors should not match
    assert!(!is_transient_github_error("permission denied"));
    assert!(!is_transient_github_error("file not found"));
    assert!(!is_transient_github_error("invalid argument"));
    assert!(!is_transient_github_error("authentication failed"));
    assert!(!is_transient_github_error("bad credentials"));
}

#[test]
fn is_transient_github_api_error_requires_github_context() {
    assert!(is_transient_github_api_error(
        "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (502): Bad Gateway"
    ));
    assert!(is_transient_github_api_error(
        "gh error: transport error: connection reset by peer"
    ));
    assert!(is_transient_github_api_error(
        "push failed: fatal: unable to access 'https://github.com/owner/repo.git/': Failed to connect to github.com port 443 after 20990 ms: Couldn't connect to server"
    ));
    assert!(!is_transient_github_api_error(
        "codex failed: Reconnecting... (stream disconnected before completion: Broken pipe)"
    ));
    assert!(!is_transient_github_api_error(
        "agent stream failed: failed to connect to local relay"
    ));
}

#[test]
fn is_transient_github_api_error_matches_circuit_breaker() {
    // Full message from GhHttp
    assert!(is_transient_github_api_error(
        "GitHub API transient 5xx circuit-breaker active for 240s"
    ));
    // Wrapped through PrCreateError + response_handler
    assert!(is_transient_github_api_error(
        "create PR failed: GitHub API error: GitHub API transient 5xx circuit-breaker active"
    ));
    // Shortened form where upstream wrapping stripped the "GitHub API" prefix —
    // circuit-breaker alone must still be recognized as a GitHub signal.
    assert!(is_transient_github_api_error(
        "create PR failed: circuit-breaker"
    ));
    // Spaced variant
    assert!(is_transient_github_api_error(
        "create PR failed: circuit breaker tripped"
    ));
}

#[tokio::test]
async fn auto_commit_unstages_files_on_commit_failure() {
    // Create a temp git repo with one initial commit
    let dir = env::temp_dir().join(format!("orch_test_auto_commit_{}", process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    // Init repo
    let _ = Command::new("git")
        .args(["init"])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&dir)
        .output()
        .await;
    // Create initial file and commit
    fs::write(dir.join("init.txt"), "init").unwrap();
    let _ = Command::new("git")
        .args(["add", "init.txt"])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "init"])
        .current_dir(&dir)
        .output()
        .await;

    // Create a new file (unstaged)
    fs::write(dir.join("new_file.txt"), "content").unwrap();

    // Verify has_changes sees it
    assert!(has_changes(&dir).await);

    // auto_commit should return false (no git user.email configured for this
    // test repo would cause commit to fail if we hadn't set it, but we did —
    // so this test verifies the happy path too).
    // For this test, the commit should succeed since we set user.email/name.
    let result = auto_commit(
        &dir,
        "1",
        "Test title",
        "test",
        1,
        super::super::commit::When::Always,
    )
    .await;
    assert!(result.unwrap());

    // Verify no changes remain
    assert!(!has_changes(&dir).await);

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn auto_commit_ignores_repo_signing_config() {
    // Repo demands signing via a signer that always fails — auto_commit
    // must still commit (it passes -c commit.gpgsign=false)
    let dir = env::temp_dir().join(format!("orch_test_auto_commit_gpg_{}", process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@test.com"],
        vec!["config", "user.name", "Test"],
        vec!["config", "commit.gpgsign", "true"],
        vec!["config", "gpg.program", "/usr/bin/false"],
    ] {
        let _ = Command::new("git")
            .args(&args)
            .current_dir(&dir)
            .output()
            .await;
    }
    fs::write(dir.join("init.txt"), "init").unwrap();
    let _ = Command::new("git")
        .args(["add", "init.txt"])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "init"])
        .current_dir(&dir)
        .output()
        .await;

    fs::write(dir.join("signed_off.txt"), "content").unwrap();
    assert!(has_changes(&dir).await);

    let result = auto_commit(
        &dir,
        "3",
        "Test title",
        "test",
        1,
        super::super::commit::When::Always,
    )
    .await;
    assert!(result.unwrap());
    assert!(!has_changes(&dir).await);

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn auto_commit_unstages_on_pre_commit_hook_failure() {
    // Create a temp git repo where commit always fails (via pre-commit hook)
    let dir = env::temp_dir().join(format!("orch_test_auto_commit_hook_{}", process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    // Init repo
    let _ = Command::new("git")
        .args(["init"])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&dir)
        .output()
        .await;
    // Create initial commit
    fs::write(dir.join("init.txt"), "init").unwrap();
    let _ = Command::new("git")
        .args(["add", "init.txt"])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "init"])
        .current_dir(&dir)
        .output()
        .await;

    // Install a pre-commit hook that always fails
    let hooks_dir = dir.join(".git/hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    fs::write(hooks_dir.join("pre-commit"), "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(hooks_dir.join("pre-commit"))
            .unwrap()
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(hooks_dir.join("pre-commit"), perms).unwrap();
    }

    // Create a new file
    fs::write(dir.join("staged_file.txt"), "content").unwrap();

    // Verify has_changes sees it
    assert!(has_changes(&dir).await);

    // auto_commit should return false (pre-commit hook blocks it)
    let result = auto_commit(
        &dir,
        "2",
        "Test title",
        "test",
        1,
        super::super::commit::When::Always,
    )
    .await;
    assert!(!result.unwrap());

    // After commit failure, files should be unstaged — has_changes should
    // still return true because the unstaged file is still there.
    assert!(has_changes(&dir).await);

    // Verify the file is NOT staged (git diff --cached --quiet should succeed)
    let cached = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(&dir)
        .status()
        .await;
    assert!(
        cached.is_ok_and(|s| s.success()),
        "files should be unstaged after commit failure"
    );

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn is_workflow_scope_error_detects_backtick_format() {
    let err = "! [remote rejected] branch -> branch (refusing to allow an OAuth App to create or update workflow `.github/workflows/ci.yml` without `workflow` scope)";
    assert!(is_workflow_scope_error(err));
}

#[test]
fn is_workflow_scope_error_detects_single_quote_format() {
    let err = "refusing to allow an OAuth App to create or update workflow '.github/workflows/ci.yml' without 'workflow' scope";
    assert!(is_workflow_scope_error(err));
}

#[test]
fn is_workflow_scope_error_detects_refusing_to_allow_variant() {
    let err = "remote: refusing to allow a Personal Access Token to create or update workflow `.github/workflows/scan.yml` without `workflow` scope";
    assert!(is_workflow_scope_error(err));
}

#[test]
fn is_workflow_scope_error_rejects_unrelated_errors() {
    assert!(!is_workflow_scope_error("non-fast-forward"));
    assert!(!is_workflow_scope_error("permission denied"));
    assert!(!is_workflow_scope_error("authentication failed"));
    assert!(!is_workflow_scope_error("rejected"));
}

#[tokio::test]
async fn strip_workflow_files_removes_workflow_from_commits() {
    // Create a temp git repo simulating the scenario
    let dir = env::temp_dir().join(format!("orch_test_strip_wf_{}", process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    // Init repo with initial commit
    let _ = Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&dir)
        .output()
        .await;
    fs::write(dir.join("init.txt"), "init").unwrap();
    let _ = Command::new("git")
        .args(["add", "."])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "init"])
        .current_dir(&dir)
        .output()
        .await;

    // Create a "remote" by adding the repo as its own origin (for rev-list)
    let _ = Command::new("git")
        .args(["remote", "add", "origin", dir.to_str().unwrap()])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["fetch", "origin"])
        .current_dir(&dir)
        .output()
        .await;

    // Create a branch off main
    let _ = Command::new("git")
        .args(["checkout", "-b", "feature"])
        .current_dir(&dir)
        .output()
        .await;

    // Add both a normal file and a workflow file
    fs::write(dir.join("feature.txt"), "feature work").unwrap();
    let wf_dir = dir.join(".github/workflows");
    fs::create_dir_all(&wf_dir).unwrap();
    fs::write(wf_dir.join("ci.yml"), "name: CI\non: push").unwrap();
    let _ = Command::new("git")
        .args(["add", "."])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "add feature and workflow",
        ])
        .current_dir(&dir)
        .output()
        .await;

    // Get the default branch name
    let branch_out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "origin/HEAD"])
        .current_dir(&dir)
        .output()
        .await;
    // Fallback to "main" if origin/HEAD is not set
    let default_branch = branch_out.ok().filter(|o| o.status.success()).map_or_else(
        || "main".to_string(),
        |o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .strip_prefix("origin/")
                .unwrap_or("main")
                .to_string()
        },
    );

    // Strip workflow files
    let stripped = strip_workflow_files(&dir, &default_branch).await.unwrap();
    assert!(stripped, "should have stripped workflow files");

    // Verify workflow file is no longer in the committed tree
    let ls_output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", "HEAD"])
        .current_dir(&dir)
        .output()
        .await
        .unwrap();
    let files = String::from_utf8_lossy(&ls_output.stdout);
    assert!(
        !files.contains(".github/workflows/ci.yml"),
        "workflow file should be removed from commits"
    );
    assert!(
        files.contains("feature.txt"),
        "non-workflow files should be preserved"
    );

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn strip_workflow_files_returns_false_when_no_workflows() {
    let dir = env::temp_dir().join(format!("orch_test_strip_wf_none_{}", process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    // Init repo
    let _ = Command::new("git")
        .args(["init"])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&dir)
        .output()
        .await;
    fs::write(dir.join("init.txt"), "init").unwrap();
    let _ = Command::new("git")
        .args(["add", "."])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "init"])
        .current_dir(&dir)
        .output()
        .await;

    let _ = Command::new("git")
        .args(["remote", "add", "origin", dir.to_str().unwrap()])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["fetch", "origin"])
        .current_dir(&dir)
        .output()
        .await;

    let _ = Command::new("git")
        .args(["checkout", "-b", "feature2"])
        .current_dir(&dir)
        .output()
        .await;

    // Only non-workflow files
    fs::write(dir.join("feature.txt"), "feature").unwrap();
    let _ = Command::new("git")
        .args(["add", "."])
        .current_dir(&dir)
        .output()
        .await;
    let _ = Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "add feature only",
        ])
        .current_dir(&dir)
        .output()
        .await;

    let stripped = strip_workflow_files(&dir, "main").await.unwrap();
    assert!(!stripped, "should not strip when no workflow files exist");

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn is_invalid_base_error_detects_base_invalid() {
    assert!(is_invalid_base_error(
        "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (422): {\"errors\":[{\"resource\":\"PullRequest\",\"field\":\"base\",\"code\":\"invalid\"}]}"
    ));
    assert!(is_invalid_base_error(
        "GitHub API POST .../pulls failed (422): Base field is invalid"
    ));
}

#[test]
fn is_invalid_base_error_rejects_non_base_errors() {
    // No-commits error should NOT match
    assert!(!is_invalid_base_error(
        "GitHub API POST .../pulls failed (422): No commits between main and branch"
    ));
    // Head invalid should NOT match
    assert!(!is_invalid_base_error(
        "GitHub API POST .../pulls failed (422): head is not a valid ref"
    ));
    // Transient 5xx should NOT match
    assert!(!is_invalid_base_error(
        "GitHub API POST .../pulls failed (502): Bad Gateway"
    ));
    // 404 should NOT match
    assert!(!is_invalid_base_error(
        "GitHub API POST .../pulls failed (404): Not Found"
    ));
}

fn pr_summary(body: &str) -> &str {
    let rest = body
        .strip_prefix("## Summary\n\n")
        .expect("PR body starts with ## Summary");
    rest.split("\n\n").next().unwrap()
}

fn assert_no_push_or_pr_creation(body: &str) {
    let lower = body.to_ascii_lowercase();
    for needle in [
        "git push",
        "gh pr create",
        "did not push",
        "couldn't push",
        "could not push",
        "push was denied",
        "push was blocked",
        "push permission",
        "nothing is on the remote",
        "open the pr",
    ] {
        assert!(
            !lower.contains(needle),
            "PR body still contains {needle:?}:\n{body}"
        );
    }
}

#[test]
fn build_pr_body_strips_push_and_pr_creation_statements() {
    let examples = [
        (
            concat!(
                "Added the lesson list page. ",
                "I did not push or open the PR, because the push was blocked on a permission prompt. ",
                "Run `git push -u origin HEAD`, then `gh pr create`.",
            ),
            "Added the lesson list page.",
        ),
        (
            "It is committed locally ... The push was denied, so nothing is on the remote yet.",
            "It is committed locally",
        ),
        (
            concat!(
                "Fixed the quiz scoring. ",
                "I couldn't push it because the push permission was denied.",
            ),
            "Fixed the quiz scoring.",
        ),
    ];
    for (summary, work) in examples {
        let body = build_pr_body(
            "Fix the widget",
            summary,
            &[
                "Wrote the lesson list".into(),
                "Could not push because the push permission was denied".into(),
            ],
            &["Run `git push -u origin HEAD`".into()],
            &[],
            "16",
            "claude",
            None,
        );
        assert_eq!(pr_summary(&body), work);
        assert!(body.contains("Wrote the lesson list"));
        assert!(!body.contains("### Remaining"));
        assert_no_push_or_pr_creation(&body);
    }
}

#[test]
fn build_pr_body_keeps_summary_without_push_text() {
    let summary = "Added retry logic for GitHub 5xx errors.";
    let accomplished = vec!["Extracted the PR body builder".into()];
    let remaining = vec!["Watch CI after merge".into()];
    let body = build_pr_body(
        "Fix the widget",
        summary,
        &accomplished,
        &remaining,
        &["src/engine/runner/git_ops.rs".into()],
        "16",
        "claude",
        Some("sonnet"),
    );
    assert_eq!(pr_summary(&body), summary);
    assert!(body.contains("### What was done\n\n- Extracted the PR body builder\n"));
    assert!(body.contains("### Remaining\n\n- Watch CI after merge\n"));
    assert!(body.contains("- `src/engine/runner/git_ops.rs`"));
    assert!(body.contains(" using `sonnet`"));
    assert!(body.contains("Closes #16"));
}

#[test]
fn build_pr_body_keeps_a_ci_bootstrap_issue_open() {
    let body = build_pr_body(
        "CI-01 Set up CI and tests",
        "Added the workflow.",
        &[],
        &[],
        &[],
        "16",
        "claude",
        None,
    );
    assert!(!body.contains("Closes #"));
    assert!(!body.contains("Resolves #"));
}

fn sh_git(dir: &Path, args: &[&str]) {
    let out = process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit_file(dir: &Path, name: &str) {
    fs::write(dir.join(name), name).unwrap();
    sh_git(dir, &["add", name]);
    sh_git(dir, &["commit", "-m", name]);
}

/**
 * The review rebases its branch on main and force-pushes with lease. When GitHub's
 * "Update branch" moved the remote branch after the last fetch, the lease is stale and
 * the push was rejected, which blocked approved work (#3783)
 **/
#[tokio::test]
async fn push_branch_recovers_from_stale_lease_after_remote_moved() {
    let temp = tempfile::TempDir::new().unwrap();
    let bare = temp.path().join("origin.git");
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    fs::create_dir_all(&bare).unwrap();
    sh_git(&bare, &["init", "--bare", "-b", "main"]);
    sh_git(temp.path(), &["clone", bare.to_str().unwrap(), "a"]);
    // push_branch's own rebase commits, and CI runners have no global identity
    sh_git(&a, &["config", "user.name", "T"]);
    sh_git(&a, &["config", "user.email", "t@t"]);
    commit_file(&a, "base.txt");
    sh_git(&a, &["push", "origin", "main"]);
    sh_git(&a, &["checkout", "-b", "feat"]);
    commit_file(&a, "feature.txt");
    sh_git(&a, &["push", "-u", "origin", "feat"]);

    sh_git(temp.path(), &["clone", bare.to_str().unwrap(), "b"]);
    commit_file(&b, "main-change.txt");
    sh_git(&b, &["push", "origin", "main"]);
    sh_git(&b, &["checkout", "feat"]);
    commit_file(&b, "update-branch.txt");
    sh_git(&b, &["push", "origin", "feat"]);

    // A only fetches main, so its view of origin/feat is stale when it rewrites feat
    sh_git(&a, &["fetch", "origin", "main"]);
    sh_git(&a, &["rebase", "origin/main"]);

    assert!(push_branch(&a, "feat", "main").await.unwrap());

    sh_git(&b, &["fetch", "origin", "feat"]);
    sh_git(&b, &["checkout", "origin/feat"]);
    for name in [
        "base.txt",
        "feature.txt",
        "main-change.txt",
        "update-branch.txt",
    ] {
        assert!(b.join(name).exists(), "remote feat lost {name}");
    }
}

#[test]
fn stale_lease_error_matches_git_messages() {
    assert!(is_stale_lease_error(
        "! [rejected] feat -> feat (stale info)"
    ));
    assert!(is_stale_lease_error(
        "! [rejected] feat -> feat (remote ref updated since checkout)"
    ));
    assert!(!is_stale_lease_error(
        "! [rejected] feat -> feat (fetch first)"
    ));
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_sha(dir: &Path, rev: &str) -> String {
    let out = process::Command::new("git")
        .args(["rev-parse", rev])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git rev-parse {rev} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_identity(dir: &Path) {
    git_ok(dir, &["config", "user.email", "test@example.com"]);
    git_ok(dir, &["config", "user.name", "Test"]);
    git_ok(dir, &["config", "commit.gpgsign", "false"]);
}

fn write_commit(dir: &Path, file: &str, body: &str, msg: &str) {
    fs::write(dir.join(file), body).unwrap();
    git_ok(dir, &["add", file]);
    git_ok(dir, &["commit", "-m", msg]);
}

/// A bare origin with `main` and a pushed `feature`, and a clone `a` on `feature`
fn publish_feature(root: &Path) -> (PathBuf, PathBuf) {
    let (bare, a) = publish_default(root, "main");
    git_ok(&a, &["checkout", "-b", "feature"]);
    write_commit(&a, "feature.txt", "feature\n", "feature");
    git_ok(&a, &["push", "-u", "origin", "feature"]);
    (bare, a)
}

fn publish_default(root: &Path, default_branch: &str) -> (PathBuf, PathBuf) {
    let bare = root.join("bare.git");
    let a = root.join("a");
    git_ok(root, &["init", "-b", default_branch, a.to_str().unwrap()]);
    init_identity(&a);
    write_commit(&a, "README.md", "base\n", "base");
    git_ok(
        root,
        &[
            "init",
            "--bare",
            "-b",
            default_branch,
            bare.to_str().unwrap(),
        ],
    );
    git_ok(&a, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git_ok(&a, &["push", "-u", "origin", default_branch]);
    (bare, a)
}

fn clone_on(root: &Path, bare: &Path, name: &str, branch: &str) -> PathBuf {
    let dir = root.join(name);
    git_ok(
        root,
        &["clone", bare.to_str().unwrap(), dir.to_str().unwrap()],
    );
    init_identity(&dir);
    if branch != "main" {
        git_ok(
            &dir,
            &["checkout", "-b", branch, &format!("origin/{branch}")],
        );
    }

    dir
}

#[tokio::test]
async fn leased_force_push_does_not_replace_a_newer_remote_tip() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_feature(root.path());
    let b = clone_on(root.path(), &bare, "b", "feature");
    record_push_lease(&b, "feature", "main").await;
    let leased = git_sha(&b, "refs/remotes/origin/feature");

    write_commit(&a, "feature.txt", "from a\n", "from a");
    git_ok(&a, &["push", "origin", "feature"]);
    let newer = git_sha(&a, "HEAD");

    git_ok(&b, &["fetch", "origin"]);
    write_commit(&b, "feature.txt", "from b\n", "from b");
    let pushed = push_branch(&b, "feature", "main").await;
    assert!(
        pushed.is_err(),
        "force push should be rejected, got {pushed:?}"
    );
    assert_eq!(git_sha(&bare, "refs/heads/feature"), newer);

    // A retry sees the fetched tip, which HEAD does not contain, so the lease stays
    record_push_lease(&b, "feature", "main").await;
    assert_eq!(read_push_lease(&b, "feature").await, Some(leased));
    let again = push_branch(&b, "feature", "main").await;
    assert!(
        again.is_err(),
        "second force push should stay rejected, got {again:?}"
    );
    assert_eq!(git_sha(&bare, "refs/heads/feature"), newer);
}

#[tokio::test]
async fn leased_force_push_allows_rewrite_of_the_recorded_tip() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_feature(root.path());
    record_push_lease(&a, "feature", "main").await;
    fs::write(a.join("feature.txt"), "rewritten\n").unwrap();
    git_ok(&a, &["commit", "-am", "--amend", "-m", "feature rewritten"]);

    let pushed = push_branch(&a, "feature", "main").await;
    assert!(
        pushed.is_ok(),
        "rewrite of the leased tip should push, got {pushed:?}"
    );
    assert_eq!(git_sha(&bare, "refs/heads/feature"), git_sha(&a, "HEAD"));
    assert_eq!(
        read_push_lease(&a, "feature").await,
        Some(git_sha(&a, "HEAD"))
    );
}

#[tokio::test]
async fn push_after_update_branch_merge_force_pushes_the_rebased_branch() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_feature(root.path());
    record_push_lease(&a, "feature", "main").await;

    // GitHub's "Update branch": main moves, then main is merged into feature on the remote
    let c = clone_on(root.path(), &bare, "c", "main");
    write_commit(&c, "main.txt", "main\n", "main moves");
    git_ok(&c, &["push", "origin", "main"]);
    git_ok(&c, &["checkout", "-b", "feature", "origin/feature"]);
    git_ok(
        &c,
        &[
            "merge",
            "--no-ff",
            "-m",
            "Merge main into feature",
            "origin/main",
        ],
    );
    git_ok(&c, &["push", "origin", "feature"]);

    rebase_on_default(&a, "main").await;
    write_commit(&a, "feature.txt", "review fix\n", "review fix");
    let pushed = push_branch(&a, "feature", "main").await;
    assert!(
        pushed.is_ok(),
        "push after update-branch should succeed, got {pushed:?}"
    );
    assert_eq!(git_sha(&bare, "refs/heads/feature"), git_sha(&a, "HEAD"));
}

#[tokio::test]
async fn agent_push_moves_the_lease_to_a_tip_head_contains() {
    let root = tempfile::tempdir().unwrap();
    let (_bare, a) = publish_feature(root.path());
    record_push_lease(&a, "feature", "main").await;

    write_commit(&a, "feature.txt", "agent step\n", "agent step");
    git_ok(&a, &["push", "origin", "feature"]);
    record_push_lease(&a, "feature", "main").await;
    assert_eq!(
        read_push_lease(&a, "feature").await,
        Some(git_sha(&a, "HEAD"))
    );
}

#[tokio::test]
async fn record_push_lease_skips_the_default_branch() {
    let root = tempfile::tempdir().unwrap();
    let (_bare, a) = publish_feature(root.path());
    git_ok(&a, &["checkout", "main"]);
    record_push_lease(&a, "main", "main").await;
    assert_eq!(read_push_lease(&a, "main").await, None);
}

/// GitHub's "Update branch" by merge: main moves, then main is merged into feature
fn github_update_by_merge(root: &Path, bare: &Path, name: &str) {
    let c = clone_on(root, bare, name, "main");
    write_commit(&c, "main.txt", "main\n", "main moves");
    git_ok(&c, &["push", "origin", "main"]);
    git_ok(&c, &["checkout", "-b", "feature", "origin/feature"]);
    git_ok(
        &c,
        &[
            "merge",
            "--no-ff",
            "-m",
            "Merge main into feature",
            "origin/main",
        ],
    );
    git_ok(&c, &["push", "origin", "feature"]);
}

/// GitHub's "Update branch" by rebase: main moves, then feature is replayed on it
fn github_update_by_rebase(root: &Path, bare: &Path, name: &str) -> PathBuf {
    let c = clone_on(root, bare, name, "main");
    write_commit(&c, "main.txt", "main\n", "main moves");
    git_ok(&c, &["push", "origin", "main"]);
    git_ok(&c, &["checkout", "-b", "feature", "origin/feature"]);
    git_ok(&c, &["rebase", "origin/main"]);
    git_ok(&c, &["push", "--force", "origin", "feature"]);
    c
}

fn remote_has(root: &Path, bare: &Path, file: &str) -> bool {
    let check = root.join("check");
    if !check.exists() {
        git_ok(
            root,
            &["clone", bare.to_str().unwrap(), check.to_str().unwrap()],
        );
    }

    git_ok(&check, &["fetch", "origin"]);
    git_ok(&check, &["checkout", "--detach", "origin/feature"]);
    check.join(file).exists()
}

#[tokio::test]
async fn push_after_update_branch_rebase_force_pushes_the_rebased_branch() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_feature(root.path());
    record_push_lease(&a, "feature", "main").await;
    github_update_by_rebase(root.path(), &bare, "c");

    rebase_on_default(&a, "main").await;
    write_commit(&a, "fix.txt", "review fix\n", "review fix");
    let pushed = push_branch(&a, "feature", "main").await;
    assert!(
        pushed.is_ok(),
        "push after a rebase update should succeed, got {pushed:?}"
    );
    assert_eq!(git_sha(&bare, "refs/heads/feature"), git_sha(&a, "HEAD"));
}

#[tokio::test]
async fn rebase_update_with_a_foreign_commit_keeps_the_foreign_commit() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_feature(root.path());
    record_push_lease(&a, "feature", "main").await;
    let c = github_update_by_rebase(root.path(), &bare, "c");
    write_commit(&c, "other.txt", "other machine\n", "other machine");
    git_ok(&c, &["push", "origin", "feature"]);

    rebase_on_default(&a, "main").await;
    write_commit(&a, "fix.txt", "review fix\n", "review fix");
    let _ = push_branch(&a, "feature", "main").await;
    assert!(
        remote_has(root.path(), &bare, "other.txt"),
        "the other machine's commit was dropped"
    );
}

#[tokio::test]
async fn merge_of_a_non_default_branch_is_not_a_github_update() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_feature(root.path());
    record_push_lease(&a, "feature", "main").await;
    let c = clone_on(root.path(), &bare, "c", "feature");
    git_ok(&c, &["checkout", "-b", "side", "origin/main"]);
    write_commit(&c, "side.txt", "side\n", "side work");
    git_ok(&c, &["checkout", "feature"]);
    git_ok(&c, &["merge", "--no-ff", "-m", "Merge side", "side"]);
    git_ok(&c, &["push", "origin", "feature"]);

    rebase_on_default(&a, "main").await;
    write_commit(&a, "fix.txt", "review fix\n", "review fix");
    let _ = push_branch(&a, "feature", "main").await;
    assert!(
        remote_has(root.path(), &bare, "side.txt"),
        "the merged side branch was dropped"
    );
}

#[tokio::test]
async fn update_merge_on_top_of_a_foreign_push_keeps_the_foreign_commit() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_feature(root.path());
    record_push_lease(&a, "feature", "main").await;
    let c = clone_on(root.path(), &bare, "c", "feature");
    write_commit(&c, "other.txt", "other machine\n", "other machine");
    git_ok(&c, &["push", "origin", "feature"]);
    github_update_by_merge(root.path(), &bare, "d");

    rebase_on_default(&a, "main").await;
    write_commit(&a, "fix.txt", "review fix\n", "review fix");
    let _ = push_branch(&a, "feature", "main").await;
    assert!(
        remote_has(root.path(), &bare, "other.txt"),
        "the other machine's commit was dropped"
    );
}

#[tokio::test]
async fn force_push_leased_accepts_github_updates() {
    for by_rebase in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (bare, a) = publish_feature(root.path());
        record_push_lease(&a, "feature", "main").await;
        if by_rebase {
            github_update_by_rebase(root.path(), &bare, "c");
        } else {
            github_update_by_merge(root.path(), &bare, "c");
        }

        git_ok(&a, &["fetch", "origin"]);
        git_ok(&a, &["rebase", "origin/main"]);
        let pushed = force_push_leased(&a, "feature", "main").await;
        assert!(pushed.is_ok(), "by_rebase={by_rebase}: {pushed:?}");
        assert_eq!(git_sha(&bare, "refs/heads/feature"), git_sha(&a, "HEAD"));
    }
}

#[tokio::test]
async fn force_push_leased_accepts_a_branch_recreated_at_the_default_head() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_feature(root.path());
    record_push_lease(&a, "feature", "main").await;
    let c = clone_on(root.path(), &bare, "c", "main");
    write_commit(&c, "feature.txt", "main version\n", "main moves");
    git_ok(&c, &["push", "origin", "main"]);
    git_ok(
        &c,
        &["push", "origin", "main:refs/heads/feature", "--force"],
    );

    git_ok(&a, &["fetch", "origin"]);
    record_push_lease(&a, "feature", "main").await;
    fs::write(a.join("feature.txt"), "feature rewritten\n").unwrap();
    git_ok(&a, &["commit", "-am", "rewrite"]);
    let pushed = force_push_leased(&a, "feature", "main").await;
    assert!(pushed.is_ok(), "{pushed:?}");
    assert_eq!(git_sha(&bare, "refs/heads/feature"), git_sha(&a, "HEAD"));
}

#[tokio::test]
async fn clear_push_lease_drops_the_recorded_lease() {
    let root = tempfile::tempdir().unwrap();
    let (_bare, a) = publish_feature(root.path());
    record_push_lease(&a, "feature", "main").await;
    assert!(read_push_lease(&a, "feature").await.is_some());
    clear_push_lease(&a, "feature").await;
    assert_eq!(read_push_lease(&a, "feature").await, None);
}

#[tokio::test]
async fn force_push_leased_refuses_a_foreign_push() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_feature(root.path());
    record_push_lease(&a, "feature", "main").await;
    let c = clone_on(root.path(), &bare, "c", "feature");
    write_commit(&c, "other.txt", "other machine\n", "other machine");
    git_ok(&c, &["push", "origin", "feature"]);
    let foreign = git_sha(&c, "HEAD");

    git_ok(&a, &["fetch", "origin"]);
    record_push_lease(&a, "feature", "main").await;
    fs::write(a.join("feature.txt"), "rewritten\n").unwrap();
    git_ok(&a, &["commit", "-am", "--amend", "-m", "rewritten"]);
    assert!(force_push_leased(&a, "feature", "main").await.is_err());
    assert_eq!(git_sha(&bare, "refs/heads/feature"), foreign);
}

#[tokio::test]
async fn force_push_leased_refuses_a_branch_created_after_an_absent_lease() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_feature(root.path());
    git_ok(&a, &["checkout", "-b", "fresh", "main"]);
    record_push_lease(&a, "fresh", "main").await;
    assert_eq!(
        read_push_lease(&a, "fresh").await.as_deref(),
        Some(LEASE_ABSENT)
    );

    let c = clone_on(root.path(), &bare, "c", "main");
    git_ok(&c, &["checkout", "-b", "fresh"]);
    write_commit(&c, "other.txt", "other machine\n", "other machine");
    git_ok(&c, &["push", "origin", "fresh"]);
    let foreign = git_sha(&c, "HEAD");

    git_ok(&a, &["fetch", "origin"]);
    write_commit(&a, "mine.txt", "mine\n", "mine");
    assert!(force_push_leased(&a, "fresh", "main").await.is_err());
    assert_eq!(git_sha(&bare, "refs/heads/fresh"), foreign);
}

#[tokio::test]
async fn first_push_records_the_pushed_head_as_the_lease() {
    let root = tempfile::tempdir().unwrap();
    let (_bare, a) = publish_feature(root.path());
    git_ok(&a, &["checkout", "-b", "fresh", "main"]);
    write_commit(&a, "mine.txt", "mine\n", "mine");
    assert!(push_branch(&a, "fresh", "main").await.unwrap());
    assert_eq!(
        read_push_lease(&a, "fresh").await,
        Some(git_sha(&a, "HEAD"))
    );
}

#[tokio::test]
async fn missing_lease_falls_back_to_the_fetched_tip_and_counts_it() {
    let root = tempfile::tempdir().unwrap();
    let (_bare, a) = publish_feature(root.path());
    let before = LEASE_FALLBACKS.load(Ordering::Relaxed);
    let expected = expected_force_sha(&a, "feature", "main").await.unwrap();
    assert_eq!(expected, git_sha(&a, "refs/remotes/origin/feature"));
    assert!(LEASE_FALLBACKS.load(Ordering::Relaxed) > before);
}

#[tokio::test]
async fn agent_push_then_foreign_push_keeps_the_agent_tip_as_lease() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_feature(root.path());
    record_push_lease(&a, "feature", "main").await;
    write_commit(&a, "step.txt", "agent step\n", "agent step");
    git_ok(&a, &["push", "origin", "feature"]);
    let agent_tip = git_sha(&a, "HEAD");

    let c = clone_on(root.path(), &bare, "c", "feature");
    write_commit(&c, "other.txt", "other machine\n", "other machine");
    git_ok(&c, &["push", "origin", "feature"]);

    // Callers record before they fetch, so the lease moves to the agent's push and no further
    record_push_lease(&a, "feature", "main").await;
    git_ok(&a, &["fetch", "origin"]);
    record_push_lease(&a, "feature", "main").await;
    assert_eq!(read_push_lease(&a, "feature").await, Some(agent_tip));
    let foreign = git_sha(&bare, "refs/heads/feature");
    git_ok(&a, &["commit", "--amend", "-m", "agent step rewritten"]);
    assert!(force_push_leased(&a, "feature", "main").await.is_err());
    assert_eq!(git_sha(&bare, "refs/heads/feature"), foreign);
}

#[tokio::test]
async fn branch_without_remote_and_head_without_twins_are_not_github_updates() {
    let root = tempfile::tempdir().unwrap();
    let (_bare, a) = publish_feature(root.path());
    let lease = git_sha(&a, "HEAD");
    let main = git_sha(&a, "origin/main");
    // A tip equal to main has nothing in base..tip, which must not count as a rebase update
    assert!(!is_github_branch_update(&a, &main, &lease, "main").await);
    assert!(
        !is_github_branch_update(
            &a,
            "0000000000000000000000000000000000000000",
            &lease,
            "main"
        )
        .await
    );
}

#[test]
fn resolve_push_branch_picks_the_task_branch_when_checkout_is_main() {
    assert_eq!(
        resolve_push_branch("main", "feature", "main").unwrap(),
        "feature"
    );
    assert_eq!(
        resolve_push_branch("main", "feature", "develop").unwrap(),
        "feature"
    );
}

#[test]
fn resolve_push_branch_refuses_a_develop_default_checkout() {
    let err = resolve_push_branch("develop", "develop", "develop")
        .unwrap_err()
        .to_string();
    assert!(err.contains("push refused"), "{err}");
    assert!(err.contains("develop"), "{err}");
}

#[test]
fn resolve_push_branch_picks_the_task_branch_when_checkout_is_default_develop() {
    assert_eq!(
        resolve_push_branch("develop", "feature", "develop").unwrap(),
        "feature"
    );
}

#[tokio::test]
async fn push_branch_refuses_when_checkout_is_default_develop() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_default(root.path(), "develop");
    write_commit(&a, "secret.txt", "must not land on develop\n", "local");
    let before = git_sha(&bare, "refs/heads/develop");

    let err = push_branch(&a, "develop", "develop")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("push refused"), "{err}");
    assert_eq!(git_sha(&bare, "refs/heads/develop"), before);
}

#[tokio::test]
async fn push_branch_pushes_the_task_branch_when_checkout_is_main() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_feature(root.path());
    write_commit(&a, "more.txt", "more\n", "more");
    let feature_sha = git_sha(&a, "feature");
    git_ok(&a, &["checkout", "main"]);

    assert!(push_branch(&a, "feature", "main").await.unwrap());
    assert_eq!(git_sha(&bare, "refs/heads/feature"), feature_sha);
    assert_ne!(git_sha(&bare, "refs/heads/main"), feature_sha);
    assert_eq!(
        read_push_lease(&a, "feature").await.as_deref(),
        Some(feature_sha.as_str())
    );
}

#[tokio::test]
async fn push_branch_pushes_the_task_branch_when_checkout_is_default_develop() {
    let root = tempfile::tempdir().unwrap();
    let (bare, a) = publish_default(root.path(), "develop");
    git_ok(&a, &["checkout", "-b", "feature"]);
    write_commit(&a, "feature.txt", "feature\n", "feature");
    let feature_sha = git_sha(&a, "feature");
    git_ok(&a, &["checkout", "develop"]);

    assert!(push_branch(&a, "feature", "develop").await.unwrap());
    assert_eq!(git_sha(&bare, "refs/heads/feature"), feature_sha);
    assert_ne!(git_sha(&bare, "refs/heads/develop"), feature_sha);
}

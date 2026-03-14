//! Centralized GitHub token resolution — supports env vars, GitHub App JWT, and gh CLI fallback.
//!
//! This module provides a [`TokenResolver`] that caches token resolution and supports multiple modes:
//! - `env` (default): Read from `GH_TOKEN` or `GITHUB_TOKEN` env vars, then `gh.auth.token` config,
//!   then `gh auth token` CLI (controlled by `allow_gh_fallback`, which defaults to `true`)
//! - `github_app`: Generate JWT from app_id + private_key (for GitHub App authentication)
//!
//! ## Default behavior
//!
//! Out of the box, `TokenResolver::default()` works with just `gh auth login` — no extra config needed.
//! Resolution order:
//! 1. `GH_TOKEN` environment variable
//! 2. `GITHUB_TOKEN` environment variable
//! 3. `gh.auth.token` config value
//! 4. `gh auth token` CLI (via `allow_gh_fallback = true`)

use anyhow::Context;
use std::env;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

/// Token source mode for GitHub authentication.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum TokenMode {
    /// Read from environment variables and config, with optional `gh auth token` fallback.
    ///
    /// Resolution order:
    /// 1. `GH_TOKEN` env var
    /// 2. `GITHUB_TOKEN` env var
    /// 3. `gh.auth.token` config value
    /// 4. `gh auth token` CLI (if `allow_gh_fallback` is enabled, which is the default)
    #[default]
    Env,
    /// GitHub App authentication with JWT generation.
    GitHubApp {
        /// GitHub App ID.
        app_id: String,
        /// Path to the private key file (.pem).
        private_key_path: String,
    },
}

impl TokenMode {
    /// Parse token mode from configuration string.
    ///
    /// Supported values:
    /// - `"env"` → `TokenMode::Env`
    /// - `"github_app"` → `TokenMode::GitHubApp` (requires `app_id` and `private_key_path` config)
    #[allow(dead_code)]
    pub fn from_config(
        mode: &str,
        app_id: Option<String>,
        private_key_path: Option<String>,
    ) -> Self {
        match mode {
            "github_app" => {
                if let (Some(app_id), Some(key_path)) = (app_id, private_key_path) {
                    TokenMode::GitHubApp {
                        app_id,
                        private_key_path: key_path,
                    }
                } else {
                    tracing::warn!(
                        "github_app mode requires app_id and private_key_path, falling back to env"
                    );
                    TokenMode::Env
                }
            }
            _ => TokenMode::Env,
        }
    }
}

/// Cached token with optional expiration (for GitHub App JWTs).
#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    /// Expiration time as Unix epoch seconds (None for non-expiring tokens).
    expires_at: Option<u64>,
}

/// Centralized token resolver for GitHub authentication.
///
/// The resolver caches tokens and supports async resolution for modes that
/// may require I/O (e.g., reading private key files).
///
/// ## Default setup
///
/// ```rust,ignore
/// let resolver = TokenResolver::default(); // uses gh auth token if no env var set
/// let token = resolver.get_token().await?;
/// ```
#[derive(Debug, Clone)]
pub struct TokenResolver {
    mode: TokenMode,
    /// Whether to fall back to `gh auth token` CLI when env vars and config are unset.
    ///
    /// Defaults to `true` — just run `gh auth login` and everything works.
    /// Set to `false` only when you want to enforce explicit token configuration.
    allow_gh_fallback: bool,
    /// Cached token with expiration tracking.
    cached: Arc<RwLock<Option<CachedToken>>>,
}

impl TokenResolver {
    /// Create a new TokenResolver with the specified mode.
    ///
    /// `allow_gh_fallback` defaults to `true` — `gh auth token` is tried last if env vars
    /// and config are not set.
    pub fn new(mode: TokenMode) -> Self {
        Self {
            mode,
            allow_gh_fallback: true,
            cached: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a TokenResolver from configuration values.
    #[allow(dead_code)]
    pub fn from_config(
        mode_str: Option<&str>,
        app_id: Option<String>,
        private_key_path: Option<String>,
        allow_gh_fallback: bool,
    ) -> Self {
        let mode = TokenMode::from_config(mode_str.unwrap_or("env"), app_id, private_key_path);
        Self {
            mode,
            allow_gh_fallback,
            cached: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a default TokenResolver using environment variables with gh CLI fallback.
    pub fn default_env() -> Self {
        Self::new(TokenMode::Env)
    }

    /// Enable or disable `gh auth token` CLI fallback.
    ///
    /// Defaults to `true` — disable only to enforce explicit token configuration.
    #[allow(dead_code)]
    pub fn with_gh_fallback(mut self, enabled: bool) -> Self {
        self.allow_gh_fallback = enabled;
        self
    }

    /// Resolve the GitHub token asynchronously.
    ///
    /// Returns `Ok(Some(token))` if a token is available,
    /// `Ok(None)` if no token could be resolved,
    /// or `Err` if an error occurred during resolution.
    pub async fn get_token(&self) -> anyhow::Result<Option<String>> {
        // Check cache first
        {
            let cached = self.cached.read().await;
            if let Some(ref entry) = *cached {
                // Check expiration if present
                if let Some(expires_at) = entry.expires_at {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    if now < expires_at {
                        return Ok(Some(entry.token.clone()));
                    }
                    // Token expired, will re-resolve
                } else {
                    // No expiration, use cached token
                    return Ok(Some(entry.token.clone()));
                }
            }
        }

        // Resolve fresh token
        let token = self.resolve_fresh().await?;

        // Cache the result if we got a token
        if let Some(ref t) = token {
            let expires_at = self.calculate_expiration();
            let mut cached = self.cached.write().await;
            *cached = Some(CachedToken {
                token: t.clone(),
                expires_at,
            });
        }

        Ok(token)
    }

    /// Get token synchronously (for contexts where async is not available).
    ///
    /// Works for Env mode (env vars, config, and gh CLI fallback).
    /// GitHub App mode requires async due to file reading.
    #[allow(dead_code)]
    pub fn get_token_sync(&self) -> anyhow::Result<Option<String>> {
        match self.mode {
            TokenMode::Env => {
                // Try env vars + config first
                if let Some(t) = Self::resolve_env_token() {
                    return Ok(Some(t));
                }
                // gh CLI fallback
                if self.allow_gh_fallback {
                    return Self::resolve_gh_cli_token();
                }
                Ok(None)
            }
            TokenMode::GitHubApp { .. } => {
                // GitHub App requires async due to file I/O
                Err(anyhow::anyhow!(
                    "GitHub App token resolution requires async context, use get_token() instead"
                ))
            }
        }
    }

    /// Clear the cached token (useful when token is known to be invalid).
    #[allow(dead_code)]
    pub async fn clear_cache(&self) {
        let mut cached = self.cached.write().await;
        *cached = None;
    }

    /// Get the current token mode.
    #[allow(dead_code)]
    pub fn mode(&self) -> &TokenMode {
        &self.mode
    }

    /// Resolve a fresh token based on the configured mode.
    async fn resolve_fresh(&self) -> anyhow::Result<Option<String>> {
        // Try primary mode first
        let token = match &self.mode {
            TokenMode::Env => Self::resolve_env_token(),
            TokenMode::GitHubApp {
                app_id,
                private_key_path,
            } => {
                self.resolve_github_app_token(app_id, private_key_path)
                    .await?
            }
        };

        // Return if we got a token from primary mode
        if token.is_some() {
            return Ok(token);
        }

        // gh CLI fallback — the default when nothing else is configured
        if self.allow_gh_fallback {
            tracing::debug!("No token in env/config — trying gh auth token CLI");
            return Self::resolve_gh_cli_token();
        }

        Ok(None)
    }

    /// Resolve token from environment variables and config.
    ///
    /// Checks (in order):
    /// 1. `GH_TOKEN` env var
    /// 2. `GITHUB_TOKEN` env var
    /// 3. `gh.auth.token` config value
    fn resolve_env_token() -> Option<String> {
        if let Ok(t) = env::var("GH_TOKEN") {
            if !t.is_empty() {
                tracing::debug!("Resolved GitHub token from GH_TOKEN environment variable");
                return Some(t);
            }
        }
        if let Ok(t) = env::var("GITHUB_TOKEN") {
            if !t.is_empty() {
                tracing::debug!("Resolved GitHub token from GITHUB_TOKEN environment variable");
                return Some(t);
            }
        }
        // Config-based token (explicit override in ~/.orch/config.yml)
        if let Ok(t) = crate::config::get("gh.auth.token") {
            if !t.is_empty() {
                tracing::debug!("Resolved GitHub token from gh.auth.token config");
                return Some(t);
            }
        }
        None
    }

    /// Resolve token using `gh auth token` CLI.
    fn resolve_gh_cli_token() -> anyhow::Result<Option<String>> {
        tracing::debug!("Attempting to resolve GitHub token via gh CLI");

        // Try gh in PATH first
        if let Some(token) = Self::try_gh_command("gh")? {
            return Ok(Some(token));
        }

        // Try common gh install paths for launchd/Homebrew environments
        for path in &["/opt/homebrew/bin/gh", "/usr/local/bin/gh"] {
            if let Some(token) = Self::try_gh_command(path)? {
                return Ok(Some(token));
            }
        }

        Ok(None)
    }

    /// Try to get token from a specific gh binary path.
    ///
    /// # Why `std::process::Command` (blocking) is intentional here
    ///
    /// This looks like a "blocking call in async context" antipattern, but it is not a problem:
    /// - `resolve_fresh()` is only called from `get_token()` under a write-lock
    /// - The result is immediately cached: env tokens forever, gh CLI tokens for 1 hour
    /// - This executes AT MOST once per hour, typically once at startup
    /// - A one-time ~100ms call does not meaningfully block the Tokio runtime
    ///
    /// DO NOT replace this with `spawn_blocking` or `tokio::process::Command` —
    /// the added complexity is not justified and previous attempts to "fix" this
    /// broke the auth flow. See issues #418 and #421 (closed as invalid).
    fn try_gh_command(gh_path: &str) -> anyhow::Result<Option<String>> {
        match std::process::Command::new(gh_path)
            .args(["auth", "token"])
            .output()
        {
            Ok(out) if out.status.success() => {
                let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !token.is_empty() {
                    tracing::debug!(gh_path, "Resolved GitHub token via gh CLI");
                    return Ok(Some(token));
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                tracing::debug!(gh_path, %stderr, "gh auth token failed");
            }
            Err(e) => {
                tracing::debug!(gh_path, error = %e, "Failed to execute gh command");
            }
        }
        Ok(None)
    }

    /// Resolve token for GitHub App (JWT-based).
    async fn resolve_github_app_token(
        &self,
        app_id: &str,
        private_key_path: &str,
    ) -> anyhow::Result<Option<String>> {
        tracing::debug!(app_id, private_key_path, "Generating GitHub App JWT");

        // Read the private key
        let private_key = tokio::fs::read_to_string(private_key_path)
            .await
            .with_context(|| format!("Failed to read private key from {private_key_path}"))?;

        // Generate JWT
        let jwt = generate_github_app_jwt(app_id, &private_key)?;

        // If an installation_id is configured, exchange the JWT for an
        // installation access token. Otherwise, return the raw JWT so callers
        // can perform manual exchanges or use it as-is.
        let installation_id = crate::config::get("github.installation_id").ok();
        if let Some(installation) = installation_id {
            if installation.trim().is_empty() {
                tracing::warn!("github.installation_id is empty — returning raw JWT");
                return Ok(Some(jwt));
            }

            tracing::debug!(installation, "Exchanging GitHub App JWT for installation token");

            // Exchange JWT for installation access token via REST API
            let client = reqwest::Client::new();
            let url = format!("https://api.github.com/app/installations/{installation}/access_tokens");
            let resp = client
                .post(&url)
                .header(reqwest::header::AUTHORIZATION, format!("Bearer {jwt}"))
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .header(reqwest::header::USER_AGENT, "orch")
                .send()
                .await
                .with_context(|| format!("failed to POST to installation access_tokens endpoint: {url}"))?;

            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                tracing::warn!(status = %status, body = %text, "installation token exchange failed");
                anyhow::bail!("installation token exchange failed ({status}): {text}");
            }

            // Parse token from response
            let v: serde_json::Value = serde_json::from_str(&text)
                .with_context(|| "failed to parse installation token response as JSON")?;
            if let Some(tok) = v.get("token").and_then(|t| t.as_str()) {
                tracing::debug!(installation, "obtained installation token via GitHub App");
                return Ok(Some(tok.to_string()));
            }

            anyhow::bail!("installation token response missing 'token' field: {v:?}");
        }

        // No installation configured — return JWT
        tracing::warn!(
            "GitHub App JWT generation is available but github.installation_id not configured; returning raw JWT"
        );
        Ok(Some(jwt))
    }

    /// Calculate expiration time for cached tokens.
    fn calculate_expiration(&self) -> Option<u64> {
        match self.mode {
            // GitHub App JWTs expire (typically 10 minutes)
            TokenMode::GitHubApp { .. } => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                // JWT valid for 9 minutes (GitHub allows up to 10)
                Some(now + 540)
            }
            // Env tokens don't expire (managed externally)
            _ => None,
        }
    }
}

/// Process-wide shared [`TokenResolver`].
///
/// Initialized once on first call. All callers (GhHttp, agent runner, etc.)
/// share the same instance so the token is resolved once — via GH_TOKEN,
/// GITHUB_TOKEN, or `gh auth token` CLI — and cached for the life of the process.
static SHARED_RESOLVER: OnceLock<Arc<TokenResolver>> = OnceLock::new();

/// Get the process-wide shared token resolver.
pub fn shared() -> Arc<TokenResolver> {
    SHARED_RESOLVER
        .get_or_init(|| Arc::new(TokenResolver::default_env()))
        .clone()
}

impl Default for TokenResolver {
    fn default() -> Self {
        Self::default_env()
    }
}

/// Generate a JWT for GitHub App authentication.
///
/// This creates a signed JWT using the app's private key (RS256).
/// The JWT can then be used to authenticate as the app or exchange for
/// an installation access token.
fn generate_github_app_jwt(app_id: &str, private_key_pem: &str) -> anyhow::Result<String> {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize)]
    struct Claims {
        /// Issued at time (Unix timestamp)
        iat: u64,
        /// Expiration time (Unix timestamp, max 10 minutes from iat)
        exp: u64,
        /// GitHub App ID
        iss: String,
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let claims = Claims {
        iat: now - 60,  // Allow 60 seconds clock skew
        exp: now + 540, // 9 minutes expiration
        iss: app_id.to_string(),
    };

    let header = Header::new(Algorithm::RS256);
    let encoding_key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
        .context("Failed to parse private key PEM")?;

    let token = encode(&header, &claims, &encoding_key).context("Failed to encode JWT")?;

    Ok(token)
}

/// Create a global token resolver from configuration.
///
/// Resolution order:
/// 1. `GH_TOKEN` / `GITHUB_TOKEN` env vars
/// 2. `gh.auth.token` config value
/// 3. `gh auth token` CLI (unless `gh.allow_gh_fallback = false`)
///
/// This is typically called once at application startup.
#[allow(dead_code)]
pub fn create_token_resolver() -> TokenResolver {
    let mode = crate::config::get("github.token_mode").ok();
    let app_id = crate::config::get("github.app_id").ok();
    let private_key_path = crate::config::get("github.private_key_path").ok();
    // allow_gh_fallback defaults to true — gh auth login is all you need
    let allow_gh_fallback = crate::config::get("gh.allow_gh_fallback")
        .ok()
        .map(|s| s != "false" && s != "0")
        .unwrap_or(true);

    TokenResolver::from_config(mode.as_deref(), app_id, private_key_path, allow_gh_fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    static ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    #[test]
    fn token_mode_from_config_env() {
        let mode = TokenMode::from_config("env", None, None);
        assert!(matches!(mode, TokenMode::Env));
    }

    #[test]
    fn token_mode_from_config_unknown_defaults_to_env() {
        let mode = TokenMode::from_config("unknown", None, None);
        assert!(matches!(mode, TokenMode::Env));
    }

    #[test]
    fn token_mode_from_config_github_app() {
        let mode = TokenMode::from_config(
            "github_app",
            Some("123456".to_string()),
            Some("/path/to/key.pem".to_string()),
        );
        assert!(matches!(mode, TokenMode::GitHubApp { .. }));
    }

    #[test]
    fn token_mode_from_config_github_app_missing_params() {
        // Should fall back to env if app_id or private_key_path is missing
        let mode = TokenMode::from_config("github_app", Some("123456".to_string()), None);
        assert!(matches!(mode, TokenMode::Env));

        let mode = TokenMode::from_config("github_app", None, Some("/path/to/key.pem".to_string()));
        assert!(matches!(mode, TokenMode::Env));
    }

    #[test]
    fn token_resolver_default_is_env() {
        let resolver = TokenResolver::default();
        assert!(matches!(resolver.mode(), TokenMode::Env));
    }

    #[test]
    fn token_resolver_new_env() {
        let resolver = TokenResolver::new(TokenMode::Env);
        assert!(matches!(resolver.mode(), TokenMode::Env));
    }

    #[test]
    fn token_resolver_gh_fallback_enabled_by_default() {
        let resolver = TokenResolver::new(TokenMode::Env);
        assert!(resolver.allow_gh_fallback);
    }

    #[test]
    fn token_resolver_with_gh_fallback() {
        let resolver = TokenResolver::new(TokenMode::Env).with_gh_fallback(false);
        assert!(!resolver.allow_gh_fallback);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn get_token_returns_none_when_no_env_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        // Disable gh fallback so we don't actually call gh CLI in tests
        let resolver = TokenResolver::new(TokenMode::Env).with_gh_fallback(false);
        resolver.clear_cache().await;

        let token = resolver.get_token().await.unwrap();
        assert!(token.is_none());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn get_token_prefers_gh_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        let resolver = TokenResolver::new(TokenMode::Env).with_gh_fallback(false);
        resolver.clear_cache().await;

        env::set_var("GH_TOKEN", "gh_token_value");
        env::remove_var("GITHUB_TOKEN");

        let resolver = TokenResolver::new(TokenMode::Env).with_gh_fallback(false);
        let token = resolver.get_token().await.unwrap();
        assert_eq!(token, Some("gh_token_value".to_string()));

        env::remove_var("GH_TOKEN");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn get_token_falls_back_to_github_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        let resolver = TokenResolver::new(TokenMode::Env).with_gh_fallback(false);
        resolver.clear_cache().await;

        env::set_var("GITHUB_TOKEN", "github_token_value");

        let token = resolver.get_token().await.unwrap();
        assert_eq!(token, Some("github_token_value".to_string()));

        env::remove_var("GITHUB_TOKEN");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn get_token_prefers_gh_token_over_github_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        let resolver = TokenResolver::new(TokenMode::Env).with_gh_fallback(false);
        resolver.clear_cache().await;

        env::set_var("GH_TOKEN", "gh_token_value");
        env::set_var("GITHUB_TOKEN", "github_token_value");

        let token = resolver.get_token().await.unwrap();
        // GH_TOKEN should be preferred
        assert_eq!(token, Some("gh_token_value".to_string()));

        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn get_token_ignores_empty_env_vars() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        let resolver = TokenResolver::new(TokenMode::Env).with_gh_fallback(false);
        resolver.clear_cache().await;

        env::set_var("GH_TOKEN", "");
        env::set_var("GITHUB_TOKEN", "");

        let token = resolver.get_token().await.unwrap();
        assert!(token.is_none());

        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn get_token_sync_env_mode() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        let resolver = TokenResolver::new(TokenMode::Env).with_gh_fallback(false);
        resolver.clear_cache().await;

        env::set_var("GH_TOKEN", "sync_test_token");

        let token = resolver.get_token_sync().unwrap();
        assert_eq!(token, Some("sync_test_token".to_string()));

        env::remove_var("GH_TOKEN");
    }

    #[test]
    fn get_token_sync_github_app_requires_async() {
        let resolver = TokenResolver::new(TokenMode::GitHubApp {
            app_id: "123".to_string(),
            private_key_path: "/path/to/key.pem".to_string(),
        });

        let result = resolver.get_token_sync();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("async"));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn clear_cache_clears_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        let resolver = TokenResolver::new(TokenMode::Env).with_gh_fallback(false);
        resolver.clear_cache().await;

        env::set_var("GH_TOKEN", "cached_token");

        // First call should cache the token
        let token1 = resolver.get_token().await.unwrap();
        assert_eq!(token1, Some("cached_token".to_string()));

        // Clear cache
        resolver.clear_cache().await;

        // Token should still resolve (fresh lookup)
        let token2 = resolver.get_token().await.unwrap();
        assert_eq!(token2, Some("cached_token".to_string()));

        env::remove_var("GH_TOKEN");
    }

    #[test]
    fn resolve_env_token_returns_none_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");

        let token = TokenResolver::resolve_env_token();
        assert!(token.is_none());
    }

    #[test]
    fn resolve_env_token_prefers_gh_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::set_var("GH_TOKEN", "gh_pref");
        env::set_var("GITHUB_TOKEN", "github_pref");

        let token = TokenResolver::resolve_env_token();
        assert_eq!(token, Some("gh_pref".to_string()));

        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
    }
}

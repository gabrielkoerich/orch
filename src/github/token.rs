//! Centralized GitHub token resolution — supports env vars, GitHub App JWT, and legacy CLI fallback.
//!
//! This module provides a [`TokenResolver`] that caches token resolution and supports multiple modes:
//! - `env`: Read from `GH_TOKEN` or `GITHUB_TOKEN` environment variables
//! - `github_app`: Generate JWT from app_id + private_key (for GitHub App authentication)
//! - `legacy`: Use `gh auth token` CLI as fallback (only if explicitly enabled)
//!
//! The resolver is designed to be used throughout the application without spawning
//! subprocesses during per-task operations.

use anyhow::Context;
use std::env;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

/// Token source mode for GitHub authentication.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum TokenMode {
    /// Read from environment variables (`GH_TOKEN` → `GITHUB_TOKEN`).
    #[default]
    Env,
    /// GitHub App authentication with JWT generation.
    GitHubApp {
        /// GitHub App ID.
        app_id: String,
        /// Path to the private key file (.pem).
        private_key_path: String,
    },
    /// Legacy fallback using `gh auth token` CLI.
    Legacy,
}

impl TokenMode {
    /// Parse token mode from configuration string.
    ///
    /// Supported values:
    /// - `"env"` → `TokenMode::Env`
    /// - `"github_app"` → `TokenMode::GitHubApp` (requires `app_id` and `private_key_path` config)
    /// - `"legacy"` → `TokenMode::Legacy`
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
            "legacy" => TokenMode::Legacy,
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
#[derive(Debug, Clone)]
pub struct TokenResolver {
    mode: TokenMode,
    /// Whether to allow gh CLI fallback when primary mode fails.
    allow_legacy_fallback: bool,
    /// Cached token with expiration tracking.
    cached: Arc<RwLock<Option<CachedToken>>>,
}

impl TokenResolver {
    /// Create a new TokenResolver with the specified mode.
    pub fn new(mode: TokenMode) -> Self {
        Self {
            mode,
            allow_legacy_fallback: false,
            cached: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a TokenResolver from configuration values.
    #[allow(dead_code)]
    pub fn from_config(
        mode_str: Option<&str>,
        app_id: Option<String>,
        private_key_path: Option<String>,
        allow_legacy_fallback: bool,
    ) -> Self {
        let mode = TokenMode::from_config(mode_str.unwrap_or("env"), app_id, private_key_path);
        Self {
            mode,
            allow_legacy_fallback,
            cached: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a default TokenResolver using environment variables.
    pub fn default_env() -> Self {
        Self::new(TokenMode::Env)
    }

    /// Enable or disable legacy gh CLI fallback.
    #[allow(dead_code)]
    pub fn with_legacy_fallback(mut self, enabled: bool) -> Self {
        self.allow_legacy_fallback = enabled;
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
    /// This only works for Env and Legacy modes that don't require async I/O.
    /// GitHub App mode requires async due to file reading.
    pub fn get_token_sync(&self) -> anyhow::Result<Option<String>> {
        match self.mode {
            TokenMode::Env => Ok(Self::resolve_env_token()),
            TokenMode::Legacy => Self::resolve_legacy_token(),
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
            TokenMode::Legacy => Self::resolve_legacy_token()?,
        };

        // Return if we got a token from primary mode
        if token.is_some() {
            return Ok(token);
        }

        // Try legacy fallback if enabled and primary mode failed
        if self.allow_legacy_fallback && !matches!(self.mode, TokenMode::Legacy) {
            tracing::debug!("Primary token mode failed, trying legacy gh CLI fallback");
            return Self::resolve_legacy_token();
        }

        Ok(None)
    }

    /// Resolve token from environment variables.
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
        None
    }

    /// Resolve token using gh CLI (legacy mode).
    fn resolve_legacy_token() -> anyhow::Result<Option<String>> {
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

        // Exchange JWT for installation token (requires installation_id)
        // For now, just return the JWT (this is a placeholder for full implementation)
        tracing::warn!(
            "GitHub App JWT generation is available but token exchange requires installation_id"
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
            // Env and legacy tokens don't expire (they're managed externally)
            _ => None,
        }
    }
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
/// This is typically called once at application startup.
#[allow(dead_code)]
pub fn create_token_resolver() -> TokenResolver {
    // Try to load configuration
    let mode = crate::config::get("github.token_mode").ok();
    let app_id = crate::config::get("github.app_id").ok();
    let private_key_path = crate::config::get("github.private_key_path").ok();
    let allow_legacy = crate::config::get("github.allow_legacy_fallback")
        .ok()
        .map(|s| s == "true" || s == "1")
        .unwrap_or(false);

    TokenResolver::from_config(mode.as_deref(), app_id, private_key_path, allow_legacy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_mode_from_config_env() {
        let mode = TokenMode::from_config("env", None, None);
        assert!(matches!(mode, TokenMode::Env));
    }

    #[test]
    fn token_mode_from_config_legacy() {
        let mode = TokenMode::from_config("legacy", None, None);
        assert!(matches!(mode, TokenMode::Legacy));
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
    fn token_mode_from_config_default() {
        let mode = TokenMode::from_config("unknown", None, None);
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
    fn token_resolver_with_legacy_fallback() {
        let resolver = TokenResolver::new(TokenMode::Env).with_legacy_fallback(true);
        assert!(resolver.allow_legacy_fallback);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_token_returns_none_when_no_env_set() {
        // Ensure no env vars are set - set them explicitly to empty
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        // Also clear any cached token
        let resolver = TokenResolver::default_env();
        resolver.clear_cache().await;

        let token = resolver.get_token().await.unwrap();
        assert!(token.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_token_prefers_gh_token() {
        // Only set GH_TOKEN, remove GITHUB_TOKEN
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        // Clear cache first
        let resolver = TokenResolver::default_env();
        resolver.clear_cache().await;

        env::set_var("GH_TOKEN", "gh_token_value");
        env::remove_var("GITHUB_TOKEN");

        let resolver = TokenResolver::default_env();
        let token = resolver.get_token().await.unwrap();
        assert_eq!(token, Some("gh_token_value".to_string()));

        env::remove_var("GH_TOKEN");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_token_falls_back_to_github_token() {
        // Clear environment first
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        let resolver = TokenResolver::default_env();
        resolver.clear_cache().await;

        // Now set GITHUB_TOKEN only
        env::set_var("GITHUB_TOKEN", "github_token_value");

        let token = resolver.get_token().await.unwrap();
        assert_eq!(token, Some("github_token_value".to_string()));

        env::remove_var("GITHUB_TOKEN");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_token_prefers_gh_token_over_github_token() {
        // Clear environment first
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        let resolver = TokenResolver::default_env();
        resolver.clear_cache().await;

        // Set both env vars
        env::set_var("GH_TOKEN", "gh_token_value");
        env::set_var("GITHUB_TOKEN", "github_token_value");

        let token = resolver.get_token().await.unwrap();
        // GH_TOKEN should be preferred
        assert_eq!(token, Some("gh_token_value".to_string()));

        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_token_ignores_empty_env_vars() {
        // Clear environment first
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        let resolver = TokenResolver::default_env();
        resolver.clear_cache().await;

        // Set empty env vars
        env::set_var("GH_TOKEN", "");
        env::set_var("GITHUB_TOKEN", "");

        let token = resolver.get_token().await.unwrap();
        assert!(token.is_none());

        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_token_sync_env_mode() {
        // Clear environment first
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        let resolver = TokenResolver::default_env();
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

    #[tokio::test(flavor = "current_thread")]
    async fn clear_cache_clears_token() {
        // Clear environment first
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        let resolver = TokenResolver::default_env();
        resolver.clear_cache().await;

        // Now set the env var
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
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");

        let token = TokenResolver::resolve_env_token();
        assert!(token.is_none());
    }

    #[test]
    fn resolve_env_token_prefers_gh_token() {
        env::set_var("GH_TOKEN", "gh_pref");
        env::set_var("GITHUB_TOKEN", "github_pref");

        let token = TokenResolver::resolve_env_token();
        assert_eq!(token, Some("gh_pref".to_string()));

        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
    }
}

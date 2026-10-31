//! Centralized GitHub token resolution from env vars, config, and the gh CLI.
//!
//! Out of the box, `TokenResolver::default()` works with just `gh auth login`, no extra config needed.
//! Resolution order:
//! 1. `GH_TOKEN` environment variable
//! 2. `GITHUB_TOKEN` environment variable
//! 3. `gh.auth.token` config value
//! 4. `gh auth token` CLI (via `allow_gh_fallback = true`)

#[cfg(test)]
use once_cell::sync::Lazy;
use std::env;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;

/// Centralized token resolver for GitHub authentication.
///
/// ## Default setup
///
/// ```rust,ignore
/// let resolver = TokenResolver::default(); // uses gh auth token if no env var set
/// let token = resolver.get_token().await?;
/// ```
#[derive(Debug, Clone)]
pub struct TokenResolver {
    /// Whether to fall back to `gh auth token` CLI when env vars and config are unset.
    ///
    /// Defaults to `true` — just run `gh auth login` and everything works.
    allow_gh_fallback: bool,
    /// Cached token, kept until `clear_cache` drops it.
    cached: Arc<RwLock<Option<String>>>,
}

impl TokenResolver {
    /// Create a default TokenResolver using environment variables with gh CLI fallback.
    pub fn default_env() -> Self {
        Self {
            allow_gh_fallback: true,
            cached: Arc::new(RwLock::new(None)),
        }
    }

    /// Enable or disable `gh auth token` CLI fallback.
    #[expect(dead_code)]
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
        if let Some(ref t) = *self.cached.read().await {
            return Ok(Some(t.clone()));
        }

        let token = self.resolve_fresh()?;
        if let Some(ref t) = token {
            *self.cached.write().await = Some(t.clone());
        }

        Ok(token)
    }

    /// Clear the cached token (useful when token is known to be invalid).
    pub async fn clear_cache(&self) {
        let mut cached = self.cached.write().await;
        *cached = None;
    }

    /// Resolve a fresh token: env vars and config first, then the gh CLI.
    fn resolve_fresh(&self) -> anyhow::Result<Option<String>> {
        if let Some(t) = Self::resolve_env_token() {
            return Ok(Some(t));
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
                // Info, not debug: when a token turns out bad, the source is the first thing to check
                tracing::info!("Resolved GitHub token from GH_TOKEN environment variable");
                return Some(t);
            }
        }
        if let Ok(t) = env::var("GITHUB_TOKEN") {
            if !t.is_empty() {
                tracing::info!("Resolved GitHub token from GITHUB_TOKEN environment variable");
                return Some(t);
            }
        }
        // Config-based token (explicit override in ~/.orch/config.yml)
        if let Ok(t) = crate::config::get("gh.auth.token") {
            if !t.is_empty() {
                tracing::info!("Resolved GitHub token from gh.auth.token config");
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
    /// - `resolve_fresh()` is only called from `get_token()` on a cache miss
    /// - The result is cached until a 401 clears it, so this runs once at startup
    ///   and again after each 401
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
                    tracing::info!(gh_path, "Resolved GitHub token via gh CLI (keychain)");
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
}

/// Process-wide shared [`TokenResolver`].
///
/// Initialized once on first call. All callers (GhHttp, agent runner, etc.)
/// share the same instance so the token is resolved once — via GH_TOKEN,
/// GITHUB_TOKEN, or `gh auth token` CLI — and cached for the life of the process.
static SHARED_RESOLVER: OnceLock<Arc<TokenResolver>> = OnceLock::new();

/// Get the process-wide shared token resolver.
pub fn shared() -> Arc<TokenResolver> {
    std::sync::Arc::clone(SHARED_RESOLVER.get_or_init(|| Arc::new(TokenResolver::default_env())))
}

impl Default for TokenResolver {
    fn default() -> Self {
        Self::default_env()
    }
}

/// Serializes tests that mutate `GH_TOKEN`/`GITHUB_TOKEN`, shared with http.rs tests
#[cfg(test)]
pub(crate) static ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_resolver_gh_fallback_enabled_by_default() {
        let resolver = TokenResolver::default_env();
        assert!(resolver.allow_gh_fallback);
    }

    #[test]
    fn token_resolver_with_gh_fallback() {
        let resolver = TokenResolver::default_env().with_gh_fallback(false);
        assert!(!resolver.allow_gh_fallback);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn get_token_returns_none_when_no_env_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        // Disable gh fallback so we don't actually call gh CLI in tests
        let resolver = TokenResolver::default_env().with_gh_fallback(false);
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
        let resolver = TokenResolver::default_env().with_gh_fallback(false);
        resolver.clear_cache().await;

        env::set_var("GH_TOKEN", "gh_token_value");
        env::remove_var("GITHUB_TOKEN");

        let resolver = TokenResolver::default_env().with_gh_fallback(false);
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
        let resolver = TokenResolver::default_env().with_gh_fallback(false);
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
        let resolver = TokenResolver::default_env().with_gh_fallback(false);
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
        let resolver = TokenResolver::default_env().with_gh_fallback(false);
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
    async fn clear_cache_clears_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("GH_TOKEN");
        env::remove_var("GITHUB_TOKEN");
        let resolver = TokenResolver::default_env().with_gh_fallback(false);
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

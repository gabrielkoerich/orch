//! GitHub authentication — Token resolver supporting PAT and GitHub App auth.
//!
//! Provides multiple authentication modes:
//! - **gh CLI (default)**: Uses `gh auth token` — just run `gh auth login` and you're done
//! - **Personal Access Token (PAT)**: Static token from env vars or config
//! - **GitHub App**: JWT-based auth with automatic installation token refresh
//!
//! # Configuration
//!
//! Auth mode is selected via config `gh.auth.mode`:
//! - `"auto"` (default) - Try GH_TOKEN/GITHUB_TOKEN env vars, then GitHub App, then gh CLI
//! - `"token"` - Use PAT from `gh.auth.token` or `GH_TOKEN`/`GITHUB_TOKEN` env vars
//! - `"github_app"` - Use GitHub App auth with `gh.auth.app_id` and `gh.auth.private_key`
//! - `"gh_cli"` - Always use `gh auth token`
//!
//! # Example
//!
//! ```yaml
//! # ~/.orch/config.yml
//! gh:
//!   auth:
//!     mode: "github_app"
//!     app_id: "123456"
//!     private_key: "/path/to/app.pem"
//!     # Optional: specific installation ID (auto-detected if not set)
//!     installation_id: "78901234"
//! ```

use anyhow::Context;
use async_trait::async_trait;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const GITHUB_API: &str = "https://api.github.com";
const DEFAULT_INSTALLATION_TOKEN_TTL: Duration = Duration::from_secs(3600); // 1 hour
const TOKEN_REFRESH_BUFFER: Duration = Duration::from_secs(300); // Refresh 5 min before expiry

// Allow dead code for GitHub App auth - these will be used when
// async token resolution is fully integrated
#[allow(dead_code)]
/// Token resolver trait — abstracts over different auth methods.
#[async_trait]
pub trait TokenResolver: Send + Sync {
    /// Resolve a valid GitHub API token.
    ///
    /// For PAT, this returns the static token.
    /// For GitHub App, this exchanges/returns a cached installation token.
    async fn resolve_token(&self) -> anyhow::Result<String>;

    /// Check if authentication is properly configured.
    #[allow(dead_code)]
    fn health_check(&self) -> anyhow::Result<()>;

    /// Get a descriptive name for this auth method (for logging).
    #[allow(dead_code)]
    fn auth_method(&self) -> &'static str;
}

/// Async counterpart to `TokenResolver` for code that needs to await token
/// retrieval or health checks (e.g., GitHub App token refresh flows).
#[async_trait]
pub trait AsyncTokenResolver: Send + Sync {
    async fn resolve_token(&self) -> anyhow::Result<String>;
    async fn health_check(&self) -> anyhow::Result<()>;
    fn auth_method(&self) -> &'static str;
}

/// Adapter that wraps a synchronous `TokenResolver` and exposes an async API
/// by delegating calls to `tokio::task::spawn_blocking` to avoid blocking
/// the async runtime.
///
/// This type is part of the async token resolver infrastructure (see issue #391).
/// It will be wired into GhHttp startup once the migration is complete.
#[allow(dead_code)]
pub struct AsyncResolverAdapter {
    inner: Arc<dyn TokenResolver>,
}

impl AsyncResolverAdapter {
    #[allow(dead_code)]
    pub fn new(inner: Arc<dyn TokenResolver>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl AsyncTokenResolver for AsyncResolverAdapter {
    async fn resolve_token(&self) -> anyhow::Result<String> {
        let inner = self.inner.clone();
        let res = tokio::task::spawn_blocking(move || inner.resolve_token())
            .await
            .context("token resolution task panicked")?;
        res
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        let inner = self.inner.clone();
        let res = tokio::task::spawn_blocking(move || inner.health_check())
            .await
            .context("health check task panicked")?;
        res
    }

    fn auth_method(&self) -> &'static str {
        self.inner.auth_method()
    }
}

/// Static token resolver for Personal Access Tokens.
pub struct StaticTokenResolver {
    token: String,
    source: &'static str,
}

impl StaticTokenResolver {
    /// Create from an explicit token string.
    pub fn new(token: String, source: &'static str) -> Self {
        Self { token, source }
    }

    /// Try to create from environment variables or config.
    pub fn from_env_or_config() -> Option<Self> {
        // Try env vars first (highest priority)
        if let Ok(token) = std::env::var("GH_TOKEN") {
            if !token.is_empty() {
                return Some(Self::new(token, "GH_TOKEN env var"));
            }
        }
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            if !token.is_empty() {
                return Some(Self::new(token, "GITHUB_TOKEN env var"));
            }
        }

        // Try config
        if let Ok(token) = crate::config::get("gh.auth.token") {
            if !token.is_empty() {
                return Some(Self::new(token, "gh.auth.token config"));
            }
        }

        None
    }
}

#[async_trait]
impl TokenResolver for StaticTokenResolver {
    async fn resolve_token(&self) -> anyhow::Result<String> {
        if self.token.is_empty() {
            anyhow::bail!("GitHub token is empty (from {})", self.source);
        }
        Ok(self.token.clone())
    }

    fn health_check(&self) -> anyhow::Result<()> {
        if self.token.is_empty() {
            anyhow::bail!("GitHub token is empty (from {})", self.source);
        }
        Ok(())
    }

    fn auth_method(&self) -> &'static str {
        "personal_access_token"
    }
}

/// JWT claims for GitHub App authentication.
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
struct AppJwtClaims {
    /// Issued at time (Unix timestamp)
    iat: u64,
    /// Expiration time (Unix timestamp, max 10 minutes from iat)
    exp: u64,
    /// GitHub App ID
    iss: String,
}

/// Cached installation token with expiration.
#[allow(dead_code)]
struct CachedToken {
    token: String,
    /// When this token expires
    expires_at: Instant,
    /// The installation ID this token is for
    installation_id: u64,
}

/// GitHub App authentication resolver.
///
/// Generates JWTs from app credentials and exchanges them for installation tokens.
/// Automatically refreshes tokens before they expire.
#[allow(dead_code)]
pub struct GitHubAppResolver {
    /// GitHub App ID
    app_id: String,
    /// Path to the PEM private key file
    private_key_path: PathBuf,
    /// Optional: specific installation ID (auto-detected if None)
    installation_id: Mutex<Option<u64>>,
    /// Cached installation token
    cached_token: Mutex<Option<CachedToken>>,
    /// HTTP client for token exchange
    client: reqwest::Client,
}

#[allow(dead_code)]
impl GitHubAppResolver {
    /// Create a new GitHub App resolver.
    pub fn new(app_id: String, private_key_path: PathBuf) -> anyhow::Result<Self> {
        if app_id.is_empty() {
            anyhow::bail!("GitHub App ID cannot be empty");
        }
        if !private_key_path.exists() {
            anyhow::bail!(
                "GitHub App private key not found: {}",
                private_key_path.display()
            );
        }

        let client = reqwest::Client::builder()
            .user_agent("orch/0.1 (reqwest)")
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client for GitHub App auth")?;

        Ok(Self {
            app_id,
            private_key_path,
            installation_id: Mutex::new(None),
            cached_token: Mutex::new(None),
            client,
        })
    }

    /// Try to create from config.
    pub fn from_config() -> Option<anyhow::Result<Self>> {
        let app_id = crate::config::get("gh.auth.app_id").ok()?;
        let private_key_path = crate::config::get("gh.auth.private_key").ok()?;

        if app_id.is_empty() || private_key_path.is_empty() {
            return None;
        }

        Some(Self::new(app_id, PathBuf::from(private_key_path)))
    }

    /// Generate a JWT for GitHub App authentication.
    fn generate_jwt(&self) -> anyhow::Result<String> {
        let private_key_pem =
            std::fs::read_to_string(&self.private_key_path).with_context(|| {
                format!(
                    "failed to read private key: {}",
                    self.private_key_path.display()
                )
            })?;

        let encoding_key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
            .context("invalid RSA private key format")?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // GitHub requires JWT to expire in max 10 minutes
        let claims = AppJwtClaims {
            iat: now,
            exp: now + 600, // 10 minutes
            iss: self.app_id.clone(),
        };

        let header = Header::new(Algorithm::RS256);
        let token =
            encode(&header, &claims, &encoding_key).context("failed to encode GitHub App JWT")?;

        Ok(token)
    }

    /// Get or discover the installation ID.
    async fn get_installation_id(&self) -> anyhow::Result<u64> {
        // Check if we already have an installation ID cached
        if let Ok(guard) = self.installation_id.lock() {
            if let Some(id) = *guard {
                return Ok(id);
            }
        }

        // Check if there's a specific installation ID in config
        if let Ok(id_str) = crate::config::get("gh.auth.installation_id") {
            if let Ok(id) = id_str.parse::<u64>() {
                if let Ok(mut guard) = self.installation_id.lock() {
                    *guard = Some(id);
                }
                return Ok(id);
            }
        }

        // Auto-discover installation ID using the JWT
        let jwt = self.generate_jwt()?;
        let installations = self.list_installations(&jwt).await?;

        match installations.len() {
            0 => anyhow::bail!("No GitHub App installations found for app {}", self.app_id),
            1 => {
                let id = installations[0].id;
                if let Ok(mut guard) = self.installation_id.lock() {
                    *guard = Some(id);
                }
                Ok(id)
            }
            _ => {
                // Multiple installations - require explicit config
                let ids: Vec<String> = installations.iter().map(|i| i.id.to_string()).collect();
                anyhow::bail!(
                    "Multiple GitHub App installations found: {}. \
                     Please set gh.auth.installation_id in config to specify which one to use.",
                    ids.join(", ")
                )
            }
        }
    }

    /// List all installations for this GitHub App.
    async fn list_installations(&self, jwt: &str) -> anyhow::Result<Vec<Installation>> {
        let url = format!("{}/app/installations", GITHUB_API);
        let resp = self
            .client
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", jwt))
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("failed to list GitHub App installations")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API error ({}): {}", status, body);
        }

        let installations: Vec<Installation> = resp
            .json()
            .await
            .context("failed to parse installations response")?;

        Ok(installations)
    }

    /// Exchange JWT for an installation access token.
    async fn create_installation_token(&self, installation_id: u64) -> anyhow::Result<String> {
        let jwt = self.generate_jwt()?;
        let url = format!(
            "{}/app/installations/{}/access_tokens",
            GITHUB_API, installation_id
        );

        let resp = self
            .client
            .post(&url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", jwt))
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("failed to request installation token")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "GitHub API error creating installation token ({}): {}",
                status,
                body
            );
        }

        let token_response: InstallationTokenResponse = resp
            .json()
            .await
            .context("failed to parse installation token response")?;

        Ok(token_response.token)
    }

    /// Get a valid installation token, refreshing if necessary.
    async fn get_installation_token(&self) -> anyhow::Result<String> {
        // Check if we have a valid cached token
        if let Ok(guard) = self.cached_token.lock() {
            if let Some(ref cached) = *guard {
                // Refresh 5 minutes before expiry to avoid race conditions
                if Instant::now() + TOKEN_REFRESH_BUFFER < cached.expires_at {
                    return Ok(cached.token.clone());
                }
            }
        }

        // Need to refresh the token
        let installation_id = self.get_installation_id().await?;
        let new_token = self.create_installation_token(installation_id).await?;

        // Cache the new token
        if let Ok(mut guard) = self.cached_token.lock() {
            *guard = Some(CachedToken {
                token: new_token.clone(),
                expires_at: Instant::now() + DEFAULT_INSTALLATION_TOKEN_TTL,
                installation_id,
            });
        }

        Ok(new_token)
    }

    /// Synchronous version of token resolution (for non-async contexts).
    /// This will return a cached token if available, otherwise error.
    fn get_token_sync(&self) -> anyhow::Result<String> {
        if let Ok(guard) = self.cached_token.lock() {
            if let Some(ref cached) = *guard {
                // Allow using cached token if not close to expiry
                if Instant::now() + TOKEN_REFRESH_BUFFER < cached.expires_at {
                    return Ok(cached.token.clone());
                }
            }
        }
        anyhow::bail!("GitHub App token needs refresh - use async resolve_token()")
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Installation {
    id: u64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct InstallationTokenResponse {
    token: String,
    expires_at: String,
}

#[async_trait]
impl TokenResolver for GitHubAppResolver {
    async fn resolve_token(&self) -> anyhow::Result<String> {
        self.get_installation_token().await
    }

    fn health_check(&self) -> anyhow::Result<()> {
        // Validate that we can read the private key
        let _ = std::fs::read_to_string(&self.private_key_path).with_context(|| {
            format!(
                "failed to read private key: {}",
                self.private_key_path.display()
            )
        })?;

        // Validate we can generate a JWT
        self.generate_jwt()
            .context("failed to generate GitHub App JWT - check app_id and private_key")?;

        Ok(())
    }

    fn auth_method(&self) -> &'static str {
        "github_app"
    }
}

/// Legacy gh CLI token resolver.
pub struct GhCliResolver;

/// Cached gh CLI token with expiry (1 hour TTL).
static GH_CLI_CACHE: Mutex<Option<(String, Instant)>> = Mutex::new(None);
const GH_CLI_TOKEN_TTL: Duration = Duration::from_secs(3600);

impl GhCliResolver {
    /// Try to get token from gh CLI. Result is cached for 1 hour.
    pub fn resolve() -> Option<String> {
        // Check cache first
        if let Ok(guard) = GH_CLI_CACHE.lock() {
            if let Some((token, expires)) = guard.as_ref() {
                if Instant::now() < *expires {
                    return Some(token.clone());
                }
            }
        }

        // Resolve fresh
        let token = Self::resolve_fresh();

        // Cache the result
        if let Some(ref t) = token {
            if let Ok(mut guard) = GH_CLI_CACHE.lock() {
                *guard = Some((t.clone(), Instant::now() + GH_CLI_TOKEN_TTL));
            }
        }

        token
    }

    fn resolve_fresh() -> Option<String> {
        let gh_paths = ["gh", "/opt/homebrew/bin/gh", "/usr/local/bin/gh"];
        for gh in &gh_paths {
            if let Ok(out) = std::process::Command::new(gh)
                .args(["auth", "token"])
                .output()
            {
                if out.status.success() {
                    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !token.is_empty() {
                        return Some(token);
                    }
                }
            }
        }
        None
    }
}

#[async_trait]
impl TokenResolver for GhCliResolver {
    async fn resolve_token(&self) -> anyhow::Result<String> {
        Self::resolve().context("gh CLI auth token not available - run `gh auth login`")
    }

    fn health_check(&self) -> anyhow::Result<()> {
        if Self::resolve().is_none() {
            anyhow::bail!("gh CLI not authenticated - run `gh auth login`");
        }
        Ok(())
    }

    fn auth_method(&self) -> &'static str {
        "gh_cli"
    }
}

/// Combined resolver that tries multiple methods.
pub struct AutoResolver {
    /// Preferred resolver (token or github_app)
    primary: Option<Box<dyn TokenResolver>>,
    /// Whether to allow gh CLI fallback
    allow_gh_fallback: bool,
}

impl AutoResolver {
    /// Create a new auto-resolver from config.
    pub fn from_config() -> anyhow::Result<Self> {
        let allow_gh_fallback = crate::config::get("gh.auth.allow_gh_fallback")
            .map(|v| v == "true")
            .unwrap_or(true);

        // Try to create the configured auth method
        let primary = Self::create_primary_resolver()?;

        Ok(Self {
            primary,
            allow_gh_fallback,
        })
    }

    fn create_primary_resolver() -> anyhow::Result<Option<Box<dyn TokenResolver>>> {
        // Check explicit auth mode in config
        let auth_mode = crate::config::get("gh.auth.mode").unwrap_or_default();

        match auth_mode.as_str() {
            "token" => {
                if let Some(resolver) = StaticTokenResolver::from_env_or_config() {
                    tracing::info!("Using GitHub auth mode: personal access token");
                    return Ok(Some(Box::new(resolver)));
                }
                anyhow::bail!("gh.auth.mode is 'token' but no token found in gh.auth.token, GH_TOKEN, or GITHUB_TOKEN");
            }
            "github_app" => {
                if let Some(result) = GitHubAppResolver::from_config() {
                    let resolver = result?;
                    tracing::info!(app_id = %resolver.app_id, "Using GitHub auth mode: GitHub App");
                    return Ok(Some(Box::new(resolver)));
                }
                anyhow::bail!("gh.auth.mode is 'github_app' but gh.auth.app_id or gh.auth.private_key not configured");
            }
            "gh_cli" => {
                // Return None - we'll use gh CLI as the primary
                tracing::info!("Using GitHub auth mode: gh CLI");
                Ok(None)
            }
            "" | "auto" => {
                // Auto-detect: try token first, then GitHub App
                if let Some(resolver) = StaticTokenResolver::from_env_or_config() {
                    tracing::info!("Auto-detected GitHub auth: personal access token");
                    return Ok(Some(Box::new(resolver)));
                }
                if let Some(result) = GitHubAppResolver::from_config() {
                    let resolver = result?;
                    tracing::info!(app_id = %resolver.app_id, "Auto-detected GitHub auth: GitHub App");
                    return Ok(Some(Box::new(resolver)));
                }
                static LOGGED: OnceLock<()> = OnceLock::new();
                LOGGED.get_or_init(|| {
                    tracing::info!("No explicit GitHub auth configured — using gh CLI fallback");
                });
                Ok(None)
            }
            other => {
                anyhow::bail!(
                    "Unknown gh.auth.mode: {}. Valid options: token, github_app, gh_cli, auto",
                    other
                );
            }
        }
    }
}

#[async_trait]
impl TokenResolver for AutoResolver {
    async fn resolve_token(&self) -> anyhow::Result<String> {
        // Try primary resolver first
        if let Some(ref primary) = self.primary {
            return primary.resolve_token().await;
        }

        // Try gh CLI fallback if enabled
        if self.allow_gh_fallback {
            if let Some(token) = GhCliResolver::resolve() {
                return Ok(token);
            }
        }

        // Build helpful error message
        let config_path = crate::home::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "~/.orch/config.yml".to_string());

        anyhow::bail!(
            "No GitHub authentication configured. \
             Set GH_TOKEN/GITHUB_TOKEN environment variables, \
             or configure gh.auth in {}. \
             See docs for GitHub App authentication setup.",
            config_path
        )
    }

    fn health_check(&self) -> anyhow::Result<()> {
        if let Some(ref primary) = self.primary {
            return primary.health_check();
        }

        if self.allow_gh_fallback {
            GhCliResolver.health_check()
        } else {
            anyhow::bail!("No GitHub authentication configured")
        }
    }

    fn auth_method(&self) -> &'static str {
        if let Some(ref primary) = self.primary {
            return primary.auth_method();
        }
        if self.allow_gh_fallback {
            return "gh_cli";
        }
        "none"
    }
}

/// Create the default token resolver from configuration.
pub fn create_resolver() -> anyhow::Result<Box<dyn TokenResolver>> {
    AutoResolver::from_config().map(|r| Box::new(r) as Box<dyn TokenResolver>)
}

/// Create an async resolver by adapting the synchronous resolver via
/// `AsyncResolverAdapter`. This is useful for callers that need to await
/// token refresh (e.g., GhHttp async flows).
///
/// Will be called from GhHttp startup as part of issue #391 migration.
#[allow(dead_code)]
pub fn create_async_resolver() -> anyhow::Result<std::sync::Arc<dyn AsyncTokenResolver>> {
    let sync = create_resolver()?;
    Ok(std::sync::Arc::new(AsyncResolverAdapter::new(
        std::sync::Arc::from(sync),
    )))
}

/// Synchronous token resolution for backwards compatibility.
///
/// This tries the auto-resolver first. For GitHub App auth, it may fail
/// if the token needs to be refreshed. Use the async resolver in new code.
#[allow(dead_code)]
pub fn resolve_token_sync() -> String {
    if tokio::runtime::Handle::try_current().is_ok() {
        tracing::error!(
            "resolve_token_sync called from async context; use async resolve_token instead"
        );
        return String::new();
    }

    match create_resolver() {
        Ok(resolver) => match tokio::runtime::Runtime::new()
            .ok()
            .and_then(|rt| rt.block_on(resolver.resolve_token()).ok())
        {
            Some(token) => token,
            None => {
                tracing::error!("Failed to resolve GitHub token synchronously");
                String::new()
            }
        },
        Err(e) => {
            tracing::error!("Failed to create GitHub token resolver: {}", e);
            String::new()
        }
    }
}

/// Resolver that always returns an error.
pub struct ErrorResolver {
    message: String,
}

impl ErrorResolver {
    pub fn new(message: String) -> Self {
        Self { message }
    }
}

#[async_trait]
impl TokenResolver for ErrorResolver {
    async fn resolve_token(&self) -> anyhow::Result<String> {
        anyhow::bail!(self.message.clone())
    }

    fn health_check(&self) -> anyhow::Result<()> {
        anyhow::bail!(self.message.clone())
    }

    fn auth_method(&self) -> &'static str {
        "none"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_token_resolver_returns_token() {
        let resolver = StaticTokenResolver::new("test_token_123".to_string(), "test");
        assert_eq!(resolver.resolve_token().await.unwrap(), "test_token_123");
        assert_eq!(resolver.auth_method(), "personal_access_token");
    }

    #[tokio::test]
    async fn static_token_resolver_fails_on_empty() {
        let resolver = StaticTokenResolver::new(String::new(), "test");
        assert!(resolver.resolve_token().await.is_err());
        assert!(resolver.health_check().is_err());
    }

    #[test]
    fn static_token_resolver_health_check_ok() {
        let resolver = StaticTokenResolver::new("valid_token".to_string(), "test");
        assert!(resolver.health_check().is_ok());
    }

    #[test]
    fn gh_cli_resolver_auth_method() {
        let resolver = GhCliResolver;
        assert_eq!(resolver.auth_method(), "gh_cli");
    }

    #[test]
    fn auto_resolver_auth_method_with_primary() {
        let resolver = AutoResolver {
            primary: Some(Box::new(StaticTokenResolver::new(
                "token".to_string(),
                "test",
            ))),
            allow_gh_fallback: false,
        };
        assert_eq!(resolver.auth_method(), "personal_access_token");
    }

    #[test]
    fn auto_resolver_auth_method_with_gh_fallback() {
        let resolver = AutoResolver {
            primary: None,
            allow_gh_fallback: true,
        };
        assert_eq!(resolver.auth_method(), "gh_cli");
    }

    #[tokio::test]
    async fn auto_resolver_fails_without_config() {
        let resolver = AutoResolver {
            primary: None,
            allow_gh_fallback: false,
        };
        assert!(resolver.resolve_token().await.is_err());
        assert!(resolver.health_check().is_err());
    }

    #[test]
    fn jwt_claims_serialization() {
        let claims = AppJwtClaims {
            iat: 1234567890,
            exp: 1234568490,
            iss: "123456".to_string(),
        };
        let json = serde_json::to_string(&claims).unwrap();
        assert!(json.contains("1234567890"));
        assert!(json.contains("1234568490"));
        assert!(json.contains("123456"));
    }
}

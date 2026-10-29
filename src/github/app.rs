//! GitHub App authentication and the review gate that reads App reviews.
//!
//! The owner token resolver stays in `token.rs`. This module signs a short-lived
//! JWT, exchanges it for an installation token, and decides which reviews count.

use super::types::GitHubReview;
use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use reqwest::{header, Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration as StdDuration;

const JWT_IAT_SKEW_SECS: i64 = 60;
const JWT_EXP_FROM_NOW_SECS: i64 = 9 * 60;
const USER_AGENT: &str = "orch/0.1 (reqwest)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewEvent {
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "APPROVE",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

struct CachedToken {
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
struct Claims {
    iss: String,
    iat: i64,
    exp: i64,
}

#[derive(Deserialize)]
struct Installation {
    id: u64,
}

#[derive(Deserialize)]
struct AccessTokenResponse {
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct AppInfo {
    slug: String,
}

pub(crate) struct AppAuth {
    app_id: String,
    pem: String,
    // `None` means the App is not installed on that repository, so it uses the owner token
    installations: Mutex<HashMap<String, Option<u64>>>,
    tokens: Mutex<HashMap<u64, CachedToken>>,
    login: Mutex<Option<String>>,
    refresh: tokio::sync::Mutex<()>,
    http: Client,
    api_base: String,
}

// Keeps the key and the token out of any `{:?}` that reaches a log
impl std::fmt::Debug for AppAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppAuth")
            .field("app_id", &self.app_id)
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

/// Process-wide App client, `None` when `gh.app.id` is unset or the App config is unusable
pub(crate) fn shared() -> Option<Arc<AppAuth>> {
    static SHARED: OnceLock<Option<Arc<AppAuth>>> = OnceLock::new();
    SHARED.get_or_init(load_shared).clone()
}

#[cfg(test)]
fn load_shared() -> Option<Arc<AppAuth>> {
    None
}

#[cfg(not(test))]
fn load_shared() -> Option<Arc<AppAuth>> {
    let id = nonempty_config("gh.app.id")?;
    match AppAuth::from_config(id) {
        Ok(app) => Some(Arc::new(app)),
        Err(err) => {
            tracing::error!(error = %err, "GitHub App config unusable, orch uses the owner token");
            None
        }
    }
}

impl AppAuth {
    #[cfg(not(test))]
    fn from_config(app_id: String) -> anyhow::Result<Self> {
        if !app_id.chars().all(|c| c.is_ascii_digit()) {
            anyhow::bail!("gh.app.id must be digits");
        }
        let pem = read_key()?;
        // Fail at startup on a bad key, not on the first request
        sign_jwt(&app_id, &pem, Utc::now().timestamp())?;
        Ok(Self::new(app_id, pem, "https://api.github.com".to_string()))
    }

    pub(crate) fn new(app_id: String, pem: String, api_base: String) -> Self {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(StdDuration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            app_id,
            pem,
            installations: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
            login: Mutex::new(None),
            refresh: tokio::sync::Mutex::new(()),
            http,
            api_base,
        }
    }

    /// Drop the cached installation tokens so the next request exchanges new ones
    pub(crate) fn invalidate(&self) {
        lock(&self.tokens).clear();
    }

    /// The installation token for `repo`, `None` when the App is not installed on it
    pub(crate) async fn token_for_repo(&self, repo: &str) -> anyhow::Result<Option<String>> {
        let Some(id) = self.installation_for(repo).await? else {
            return Ok(None);
        };
        self.installation_token(id).await.map(Some)
    }

    // An account and each organization are separate installations, so the lookup is per repo
    async fn installation_for(&self, repo: &str) -> anyhow::Result<Option<u64>> {
        if let Some(found) = lock(&self.installations).get(repo) {
            return Ok(*found);
        }
        let jwt = sign_jwt(&self.app_id, &self.pem, Utc::now().timestamp())?;
        let url = format!("{}/repos/{repo}/installation", self.api_base);
        let found = match self.send_status(self.http.get(&url), &jwt).await? {
            (status, body) if status.is_success() => {
                let installation: Installation = serde_json::from_str(&body)
                    .map_err(|_| anyhow::anyhow!("GitHub App installation could not be read"))?;
                Some(installation.id)
            }
            (StatusCode::NOT_FOUND, _) => None,
            (status, _) => {
                anyhow::bail!("GitHub App installation lookup for {repo} failed: {status}")
            }
        };
        lock(&self.installations).insert(repo.to_string(), found);
        Ok(found)
    }

    async fn installation_token(&self, id: u64) -> anyhow::Result<String> {
        if let Some(token) = self.cached_fresh_token(id) {
            return Ok(token);
        }
        let _guard = self.refresh.lock().await;
        if let Some(token) = self.cached_fresh_token(id) {
            return Ok(token);
        }
        let jwt = sign_jwt(&self.app_id, &self.pem, Utc::now().timestamp())?;
        let url = format!("{}/app/installations/{id}/access_tokens", self.api_base);
        let body = self.send(self.http.post(&url), &jwt).await?;
        let parsed: AccessTokenResponse = serde_json::from_str(&body)
            .map_err(|_| anyhow::anyhow!("GitHub installation token response could not be read"))?;
        if parsed.token.is_empty() {
            anyhow::bail!("GitHub installation token response has no token");
        }
        lock(&self.tokens).insert(
            id,
            CachedToken {
                token: parsed.token.clone(),
                expires_at: parsed.expires_at,
            },
        );
        Ok(parsed.token)
    }

    fn cached_fresh_token(&self, id: u64) -> Option<String> {
        let tokens = lock(&self.tokens);
        let cached = tokens.get(&id)?;
        token_is_fresh(cached.expires_at, Utc::now()).then(|| cached.token.clone())
    }

    /// The App's user login, `<slug>[bot]`. `GET /user` rejects installation tokens, so read
    /// the slug from `GET /app` with the JWT
    pub(crate) async fn login(&self) -> anyhow::Result<String> {
        if let Some(login) = lock(&self.login).clone() {
            return Ok(login);
        }
        let jwt = sign_jwt(&self.app_id, &self.pem, Utc::now().timestamp())?;
        let url = format!("{}/app", self.api_base);
        let body = self.send(self.http.get(&url), &jwt).await?;
        let info: AppInfo = serde_json::from_str(&body)
            .map_err(|_| anyhow::anyhow!("GitHub App info could not be read"))?;
        if info.slug.is_empty() {
            anyhow::bail!("GitHub App info has no slug");
        }
        let login = format!("{}[bot]", info.slug);
        *lock(&self.login) = Some(login.clone());
        Ok(login)
    }

    /// Submit a pull request review as the App on the commit the review agent read
    pub(crate) async fn submit_review(
        &self,
        repo: &str,
        pr_number: u64,
        event: ReviewEvent,
        body: &str,
        commit_id: &str,
    ) -> anyhow::Result<()> {
        let Some(token) = self.token_for_repo(repo).await? else {
            anyhow::bail!("the GitHub App is not installed on {repo}");
        };
        let url = format!("{}/repos/{repo}/pulls/{pr_number}/reviews", self.api_base);
        let payload = serde_json::json!({
            "event": event.as_str(),
            "body": super::http::redact_posted_body(body),
            "commit_id": commit_id,
        });
        let result = self.send(self.http.post(&url).json(&payload), &token).await;
        if result
            .as_ref()
            .is_err_and(|err| err.to_string().contains(StatusCode::UNAUTHORIZED.as_str()))
        {
            self.invalidate();
        }
        result.map(|_| ())
    }

    async fn send(&self, builder: reqwest::RequestBuilder, bearer: &str) -> anyhow::Result<String> {
        let (status, body) = self.send_status(builder, bearer).await?;
        if status.is_success() {
            return Ok(body);
        }
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value.get("message")?.as_str().map(str::to_owned))
            .filter(|msg| !crate::security::has_leaks(msg))
            .unwrap_or_default();
        anyhow::bail!("GitHub App API {status}: {message}")
    }

    async fn send_status(
        &self,
        builder: reqwest::RequestBuilder,
        bearer: &str,
    ) -> anyhow::Result<(StatusCode, String)> {
        let resp = builder
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            // The reqwest error can carry the URL, never the headers
            .map_err(|err| anyhow::anyhow!("GitHub App request failed to send: {err}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Ok((status, body))
    }
}

fn token_is_fresh(expires_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    expires_at > now + Duration::minutes(10)
}

#[cfg(not(test))]
fn nonempty_config(key: &str) -> Option<String> {
    crate::config::get(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(not(test))]
fn read_key() -> anyhow::Result<String> {
    if let Some(path) = nonempty_config("gh.app.private_key_path") {
        let raw = std::fs::read_to_string(&path)
            .map_err(|err| anyhow::anyhow!("failed to read GitHub App private key file: {err}"))?;
        return normalize_pem(&raw);
    }
    if let Some(name) = nonempty_config("gh.app.private_key_env") {
        let raw = std::env::var(&name).unwrap_or_default();
        return normalize_pem(&raw).map_err(|_| {
            anyhow::anyhow!("GitHub App private key variable {name} is unset or empty")
        });
    }
    anyhow::bail!("set gh.app.private_key_path or gh.app.private_key_env")
}

// A key injected through an environment variable often carries literal `\n`
fn normalize_pem(raw: &str) -> anyhow::Result<String> {
    let pem = raw.replace("\\n", "\n").trim().to_string();
    if pem.is_empty() {
        anyhow::bail!("GitHub App private key is empty");
    }
    Ok(pem)
}

fn sign_jwt(app_id: &str, pem: &str, now: i64) -> anyhow::Result<String> {
    let claims = Claims {
        iss: app_id.to_string(),
        iat: now - JWT_IAT_SKEW_SECS,
        exp: now + JWT_EXP_FROM_NOW_SECS,
    };
    let key = EncodingKey::from_rsa_pem(pem.as_bytes())
        .map_err(|_| anyhow::anyhow!("GitHub App private key is not a usable RSA PEM"))?;
    encode(&Header::new(Algorithm::RS256), &claims, &key)
        .map_err(|_| anyhow::anyhow!("failed to sign GitHub App JWT"))
}

/// The App login, `None` when the App is not configured or its login cannot be read
pub(crate) async fn bot_login() -> Option<String> {
    let app = shared()?;
    match app.login().await {
        Ok(login) => Some(login),
        Err(err) => {
            tracing::warn!(error = %err, "GitHub App login unreadable, review gate reads comments only");
            None
        }
    }
}

// REST names a bot `slug[bot]`, GraphQL names it `slug`
pub(crate) fn same_login(left: &str, right: &str) -> bool {
    let strip = |login: &str| login.strip_suffix("[bot]").unwrap_or(login).to_string();
    strip(left).eq_ignore_ascii_case(&strip(right))
}

fn on_head(review: &GitHubReview, head_sha: &str) -> bool {
    !head_sha.is_empty()
        && review
            .commit_id
            .as_deref()
            .is_some_and(|sha| sha.eq_ignore_ascii_case(head_sha))
}

/// Keep every non-App review. Keep an App review only when its commit is the current head
pub(crate) fn reviews_for_gate(
    reviews: &[GitHubReview],
    head_sha: &str,
    app_login: Option<&str>,
) -> Vec<GitHubReview> {
    reviews
        .iter()
        .filter(|review| {
            app_login.is_none_or(|login| {
                !same_login(&review.user.login, login) || on_head(review, head_sha)
            })
        })
        .cloned()
        .collect()
}

/// An App approval or change request on the current head replaces the comment decision
pub(crate) fn effective_comment_flags(
    reviews: &[GitHubReview],
    head_sha: &str,
    app_login: Option<&str>,
    comment_approved: bool,
    comment_changes_requested: bool,
) -> (bool, bool) {
    let replaced = app_login.is_some_and(|login| {
        reviews.iter().any(|review| {
            same_login(&review.user.login, login)
                && on_head(review, head_sha)
                && (review.state == "APPROVED" || review.state == "CHANGES_REQUESTED")
        })
    });
    if replaced {
        (false, false)
    } else {
        (comment_approved, comment_changes_requested)
    }
}

/// A throwaway RSA key pair, private then public PEM, made once per test process
#[cfg(test)]
fn test_keys() -> &'static (String, String) {
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    static KEYS: OnceLock<(String, String)> = OnceLock::new();
    KEYS.get_or_init(|| {
        let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).expect("rsa");
        let private = key.to_pkcs8_pem(LineEnding::LF).expect("pem").to_string();
        let public = rsa::RsaPublicKey::from(&key)
            .to_public_key_pem(LineEnding::LF)
            .expect("public pem");
        (private, public)
    })
}

#[cfg(test)]
pub(crate) fn test_pem() -> &'static str {
    &test_keys().0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::types::GitHubUser;
    use jsonwebtoken::{decode, DecodingKey, Validation};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{body_partial_json, header as header_is, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn app(server: &MockServer) -> AppAuth {
        AppAuth::new("12345".to_string(), test_pem().to_string(), server.uri())
    }

    async fn installed(server: &MockServer, repo: &str, id: u64) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{repo}/installation")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": id})))
            .mount(server)
            .await;
    }

    fn review(login: &str, state: &str, sha: &str, at: &str) -> GitHubReview {
        GitHubReview {
            id: 1,
            user: GitHubUser {
                login: login.to_string(),
            },
            body: None,
            state: state.to_string(),
            html_url: None,
            submitted_at: at.to_string(),
            commit_id: Some(sha.to_string()),
        }
    }

    fn decode_claims(jwt: &str) -> Claims {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.validate_exp = false;
        let key = DecodingKey::from_rsa_pem(test_keys().1.as_bytes()).expect("decode key");
        decode::<Claims>(jwt, &key, &validation)
            .expect("signed with the app key")
            .claims
    }

    #[test]
    fn jwt_has_the_app_claims_and_is_signed_with_the_key() {
        let now = 1_700_000_000;
        let jwt = sign_jwt("12345", test_pem(), now).expect("sign");
        let claims = decode_claims(&jwt);
        assert_eq!(claims.iss, "12345");
        assert_eq!(claims.iat, now - 60);
        assert!(claims.exp > now && claims.exp - now <= 600);
        assert!(
            crate::security::has_leaks(&jwt),
            "a jwt must trip the leak scan"
        );
        assert!(
            crate::security::has_leaks(test_pem()),
            "a key must trip the leak scan"
        );
    }

    #[test]
    fn a_bad_key_is_rejected_without_echoing_it() {
        let pem = format!("-----{} {}-----\nMARKERMATERIAL\n", "BEGIN", "PRIVATE KEY");
        let err = sign_jwt("12345", &pem, 0).expect_err("bad key").to_string();
        assert!(!err.contains("MARKERMATERIAL"), "{err}");
    }

    #[test]
    fn normalize_pem_expands_escaped_newlines() {
        let pem = normalize_pem("  A\\nB\\nC ").expect("pem");
        assert_eq!(pem, "A\nB\nC");
        assert!(normalize_pem("   ").is_err());
    }

    #[tokio::test]
    async fn installation_token_is_cached_and_refreshed_before_expiry() {
        let server = MockServer::start().await;
        installed(&server, "o/r", 42).await;
        let posts = Arc::new(AtomicUsize::new(0));
        let counter = posts.clone();
        Mock::given(method("POST"))
            .and(path("/app/installations/42/access_tokens"))
            .respond_with(move |_req: &Request| {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "token": format!("inst-{n}"),
                    "expires_at": "2099-01-01T00:00:00Z",
                }))
            })
            .mount(&server)
            .await;
        let app = app(&server);

        assert_eq!(
            app.token_for_repo("o/r").await.unwrap().as_deref(),
            Some("inst-1")
        );
        assert_eq!(
            app.token_for_repo("o/r").await.unwrap().as_deref(),
            Some("inst-1")
        );
        assert_eq!(posts.load(Ordering::SeqCst), 1);

        let requests = server.received_requests().await.expect("requests");
        let auth = requests[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let claims = decode_claims(auth.trim_start_matches("Bearer "));
        assert_eq!(claims.iss, "12345");

        // Inside the 10 minute window the token is refreshed
        lock(&app.tokens).get_mut(&42).unwrap().expires_at = Utc::now() + Duration::minutes(9);
        assert_eq!(
            app.token_for_repo("o/r").await.unwrap().as_deref(),
            Some("inst-2")
        );
        assert_eq!(posts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_repo_without_the_app_gets_no_token_and_is_looked_up_once() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/other-org/x/installation"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let app = app(&server);
        assert_eq!(app.token_for_repo("other-org/x").await.unwrap(), None);
        assert_eq!(app.token_for_repo("other-org/x").await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_account_and_an_organization_get_their_own_installation_tokens() {
        let server = MockServer::start().await;
        installed(&server, "me/a", 42).await;
        installed(&server, "org/b", 77).await;
        for (id, token) in [(42, "inst-me"), (77, "inst-org")] {
            Mock::given(method("POST"))
                .and(path(format!("/app/installations/{id}/access_tokens")))
                .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "token": token,
                    "expires_at": "2099-01-01T00:00:00Z",
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
        let app = app(&server);
        assert_eq!(
            app.token_for_repo("me/a").await.unwrap().as_deref(),
            Some("inst-me")
        );
        assert_eq!(
            app.token_for_repo("org/b").await.unwrap().as_deref(),
            Some("inst-org")
        );
    }

    #[tokio::test]
    async fn a_failed_lookup_is_an_error_and_is_not_cached() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/installation"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let app = app(&server);
        assert!(app.token_for_repo("o/r").await.is_err());
        assert!(lock(&app.installations).get("o/r").is_none());
    }

    #[tokio::test]
    async fn login_is_the_slug_with_the_bot_suffix() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"slug": "orch-app"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let app = app(&server);
        assert_eq!(app.login().await.unwrap(), "orch-app[bot]");
        assert_eq!(app.login().await.unwrap(), "orch-app[bot]");
    }

    #[tokio::test]
    async fn a_review_is_submitted_on_the_head_sha_with_the_installation_token() {
        let server = MockServer::start().await;
        installed(&server, "o/r", 42).await;
        Mock::given(method("POST"))
            .and(path("/app/installations/42/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "inst-1",
                "expires_at": "2099-01-01T00:00:00Z",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/pulls/9/reviews"))
            .and(header_is("authorization", "Bearer inst-1"))
            .and(body_partial_json(serde_json::json!({
                "event": "APPROVE",
                "commit_id": "abc123",
                "body": "looks good",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 1})))
            .expect(1)
            .mount(&server)
            .await;
        app(&server)
            .submit_review("o/r", 9, ReviewEvent::Approve, "looks good", "abc123")
            .await
            .expect("review submitted");
    }

    #[tokio::test]
    async fn a_review_on_a_repo_without_the_app_fails_clearly() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/org/b/installation"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = app(&server)
            .submit_review("org/b", 9, ReviewEvent::Approve, "ok", "abc123")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not installed on org/b"), "{err}");
    }

    #[test]
    fn an_app_approval_on_an_older_sha_does_not_pass_the_gate() {
        let reviews = vec![review(
            "orch-app[bot]",
            "APPROVED",
            "old",
            "2026-01-01T00:00:00Z",
        )];
        assert!(reviews_for_gate(&reviews, "new", Some("orch-app[bot]")).is_empty());
        assert_eq!(
            reviews_for_gate(&reviews, "old", Some("orch-app[bot]")).len(),
            1
        );
        // GraphQL drops the suffix
        assert!(reviews_for_gate(&reviews, "new", Some("orch-app")).is_empty());
    }

    #[test]
    fn human_reviews_stay_and_no_app_keeps_everything() {
        let reviews = vec![
            review("human", "CHANGES_REQUESTED", "old", "2026-01-01T00:00:00Z"),
            review("orch-app[bot]", "APPROVED", "old", "2026-01-02T00:00:00Z"),
        ];
        let kept = reviews_for_gate(&reviews, "new", Some("orch-app[bot]"));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].user.login, "human");
        assert_eq!(reviews_for_gate(&reviews, "new", None).len(), 2);
    }

    #[test]
    fn an_app_review_on_the_head_replaces_the_comment_decision() {
        let on_head = vec![review(
            "orch-app",
            "CHANGES_REQUESTED",
            "head",
            "2026-01-01T00:00:00Z",
        )];
        assert_eq!(
            effective_comment_flags(&on_head, "head", Some("orch-app[bot]"), true, false),
            (false, false)
        );
        // An old App review leaves the comment fallback in charge
        assert_eq!(
            effective_comment_flags(&on_head, "new", Some("orch-app[bot]"), true, false),
            (true, false)
        );
        assert_eq!(
            effective_comment_flags(&on_head, "head", None, true, false),
            (true, false)
        );
    }
}

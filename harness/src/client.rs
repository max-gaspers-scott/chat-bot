//! Authenticated HTTP client for the mgs-ai-proxy.
//!
//! # Overview
//!
//! [`ClientBuilder`] constructs an [`OpenAI`] client (rig's OpenAI-shaped
//! provider) pointed at the proxy, with three resilience layers baked into
//! the transport:
//!
//! 1. **Dynamic token injection** — the current JWT is read from a shared
//!    [`TokenStore`] and written as `Authorization: Bearer <token>` on every
//!    request, overriding whatever placeholder rig put there.
//!
//! 2. **401 refresh-and-retry** — when the server returns 401, the token is
//!    refreshed once and the request is retried.
//!
//! 3. **Exponential back-off** — 429 and 5xx responses are retried up to
//!    [`MAX_RETRIES`] times with jittered exponential delay.
//!
//! A background task (started by [`ClientBuilder::build`]) proactively
//! refreshes the token before it expires so requests rarely hit the 401 path.
//!
//! # Usage
//!
//! ```no_run
//! use harness::client::{ClientBuilder, LoginCredentials};
//!
//! # async fn run() -> anyhow::Result<()> {
//! let (rig_client, token_store) = ClientBuilder::new("https://cloud-wolf.team-stingray.com")
//!     .login(LoginCredentials {
//!         email: "user@example.com".into(),
//!         password: "secret".into(),
//!     })
//!     .await?
//!     .build()
//!     .await?;
//!
//! let model = rig_client.chat("gemini-2.5-flash");
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http::header::AUTHORIZATION;
use rig_http::http_client::{
    Error as HttpError, HttpClientExt, LazyBody, MultipartForm, StreamingResponse,
};
use rig_reqwest::ReqwestClient;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, warn};

// Re-export so callers can name the rig client type.
pub use rig_core::providers::openai::OpenAI;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// How many seconds before expiry to proactively refresh.
const REFRESH_MARGIN_SECS: u64 = 60;

/// Maximum number of back-off retries for 429 / 5xx.
const MAX_RETRIES: u32 = 4;

/// Base delay for the first back-off step (doubles each step, +jitter).
const BASE_BACKOFF_MS: u64 = 500;

// ---------------------------------------------------------------------------
// Token store
// ---------------------------------------------------------------------------

/// The live JWT and its expiry, shared across the client and background task.
#[derive(Clone, Debug)]
pub struct TokenState {
    pub token: String,
    /// Wall-clock instant after which the token must not be used.
    pub expires_at: Instant,
}

impl TokenState {
    /// `true` when the token expires within `margin`.
    pub fn expires_within(&self, margin: Duration) -> bool {
        Instant::now() + margin >= self.expires_at
    }
}

/// Thread-safe shared token store.
pub type TokenStore = Arc<RwLock<TokenState>>;

// ---------------------------------------------------------------------------
// Login / refresh types
// ---------------------------------------------------------------------------

/// Credentials the user supplies to obtain a JWT.
#[derive(Debug, Clone)]
pub struct LoginCredentials {
    pub email: String,
    pub password: String,
}

#[derive(Serialize)]
struct LoginPayload<'a> {
    email: &'a str,
    password: &'a str,
}

#[derive(Deserialize, Debug)]
struct LoginResponse {
    res: String,
    token: Option<String>,
}

/// Decode the JWT and return its expiry as an `Instant`.
/// Falls back to 5 minutes if the claim cannot be read.
fn token_expiry(token: &str) -> Instant {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};

    #[derive(Deserialize)]
    struct Claims {
        exp: Option<u64>,
    }

    let fallback = Instant::now() + Duration::from_secs(5 * 60);

    // Decode without signature verification — we trust the server, we just
    // need the expiry timestamp. Also disable exp validation so we can read
    // the claim from tokens that are already expired.
    let mut validation = Validation::new(Algorithm::HS256);
    validation.insecure_disable_signature_validation();
    validation.validate_exp = false;
    validation.set_required_spec_claims::<&str>(&[]);

    let Ok(data) =
        decode::<Claims>(token, &DecodingKey::from_secret(b""), &validation)
    else {
        return fallback;
    };

    let Some(exp) = data.claims.exp else {
        return fallback;
    };

    // exp is a Unix timestamp in seconds; convert to Instant.
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if exp <= now_unix {
        // Already expired — return a clearly past instant so refresh triggers immediately.
        return Instant::now() - Duration::from_secs(1);
    }

    Instant::now() + Duration::from_secs(exp - now_unix)
}

// ---------------------------------------------------------------------------
// Token refresher (shared by transport and background task)
// ---------------------------------------------------------------------------

/// Fetches a fresh token from the proxy and updates the store.
///
/// Cloned cheaply; the inner reqwest client is connection-pooled.
#[derive(Clone, Debug)]
pub struct TokenRefresher {
    base_url: Arc<String>,
    credentials: Arc<LoginCredentials>,
    http: reqwest::Client,
}

impl TokenRefresher {
    pub fn new(base_url: String, credentials: LoginCredentials) -> Self {
        Self {
            base_url: Arc::new(base_url),
            credentials: Arc::new(credentials),
            http: reqwest::Client::new(),
        }
    }

    /// Alias for [`Self::new`], used in integration tests where `new` would
    /// be private via the module path.
    pub fn new_exposed(base_url: String, credentials: LoginCredentials) -> Self {
        Self::new(base_url, credentials)
    }

    /// Create a refresher that knows the base URL but has no credentials.
    ///
    /// Used when an existing token is loaded from disk (e.g. in the CLI after
    /// `login`). Refresh attempts will fail gracefully — the caller falls back
    /// to asking the user to run `login` again.
    pub fn new_for_existing_token(base_url: String) -> Self {
        Self {
            base_url: Arc::new(base_url),
            credentials: Arc::new(LoginCredentials {
                email: String::new(),
                password: String::new(),
            }),
            http: reqwest::Client::new(),
        }
    }

    /// Call `POST /api/login` and return a fresh [`TokenState`].
    pub async fn refresh(&self) -> Result<TokenState> {
        let url = format!("{}/api/login", self.base_url.trim_end_matches('/'));
        let payload = LoginPayload {
            email: &self.credentials.email,
            password: &self.credentials.password,
        };

        let resp = self
            .http
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("failed to reach login endpoint")?;

        let status = resp.status();
        let body: LoginResponse = resp
            .json()
            .await
            .context("login endpoint returned invalid JSON")?;

        if !status.is_success() {
            bail!("login failed (HTTP {status}): {}", body.res);
        }

        let token = body
            .token
            .filter(|t| !t.is_empty())
            .with_context(|| format!("login returned no token: {}", body.res))?;

        let expires_at = token_expiry(&token);
        debug!(?expires_at, "token refreshed");
        Ok(TokenState { token, expires_at })
    }

    /// Refresh and write the new state into the store.
    pub async fn refresh_into(&self, store: &TokenStore) -> Result<()> {
        let state = self.refresh().await?;
        *store.write().await = state;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Back-off helper
// ---------------------------------------------------------------------------

/// Sleep for `base * 2^attempt` ms, with ±25 % jitter, capped at 30 s.
async fn backoff(attempt: u32) {
    use std::time::Duration;

    let base = BASE_BACKOFF_MS * 2u64.saturating_pow(attempt);
    let jitter = {
        // Simple deterministic jitter: hash the attempt.
        let hash = attempt.wrapping_mul(2654435761) as u64;
        hash % (base.max(1) / 2)
    };
    let delay = Duration::from_millis((base + jitter).min(30_000));
    debug!(attempt, delay_ms = delay.as_millis(), "back-off");
    tokio::time::sleep(delay).await;
}

// ---------------------------------------------------------------------------
// HarnessHttpClient — our HttpClientExt implementation
// ---------------------------------------------------------------------------

/// Wraps a `ReqwestClient`, injects the live token, and adds resilience.
///
/// Every request goes through three layers:
/// - Token injection (replace the static `Authorization` header from rig)
/// - 401 refresh-and-retry (once)
/// - 429 / 5xx exponential back-off (up to [`MAX_RETRIES`] retries)
#[derive(Clone, Debug)]
pub struct HarnessHttpClient {
    inner: ReqwestClient,
    store: TokenStore,
    refresher: TokenRefresher,
}

impl HarnessHttpClient {
    fn new(store: TokenStore, refresher: TokenRefresher) -> Self {
        Self {
            inner: ReqwestClient::default(),
            store,
            refresher,
        }
    }

    /// Build from an already-created store and refresher.
    ///
    /// Useful when a caller manages the token lifecycle externally (e.g. the
    /// CLI loading a token from disk).
    pub fn from_parts(store: TokenStore, refresher: TokenRefresher) -> Self {
        Self::new(store, refresher)
    }

    /// Read the current token from the store.
    async fn current_token(&self) -> String {
        self.store.read().await.token.clone()
    }

    /// Replace the `Authorization` header with the live token.
    fn inject_token<B>(request: &mut http::Request<B>, token: &str) {
        let value = format!("Bearer {token}");
        let header_value =
            http::HeaderValue::from_str(&value).expect("token should be a valid header value");
        request
            .headers_mut()
            .insert(AUTHORIZATION, header_value);
    }

    /// `true` when the error represents an HTTP 401.
    fn is_401(err: &HttpError) -> bool {
        matches!(
            err.non_success_status(),
            Some(s) if s == http::StatusCode::UNAUTHORIZED
        )
    }

    /// `true` when the error warrants a back-off retry (429 or 5xx).
    fn is_retryable(err: &HttpError) -> bool {
        matches!(
            err.non_success_status(),
            Some(s) if s.as_u16() == 429 || s.is_server_error()
        )
    }
}

// ---------------------------------------------------------------------------
// HttpClientExt impl — unary
// ---------------------------------------------------------------------------

impl HttpClientExt for HarnessHttpClient {
    fn send<T, U>(
        &self,
        req: http::Request<T>,
    ) -> impl std::future::Future<
        Output = rig_http::http_client::Result<http::Response<LazyBody<U>>>,
    > + Send
           + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let client = self.clone();

        // Consume the body into Bytes *before* the async block so the future
        // only captures owned data and doesn't need T: 'static.
        let (parts, body) = req.into_parts();
        let body_bytes: Bytes = body.into();

        async move {
            // --- inject current token ---
            let token = client.current_token().await;

            let rebuild = || {
                let mut r = http::Request::builder()
                    .method(parts.method.clone())
                    .uri(parts.uri.clone());
                *r.headers_mut().unwrap() = parts.headers.clone();
                let mut built = r
                    .body(body_bytes.clone())
                    .expect("infallible body rebuild");
                Self::inject_token(&mut built, &token);
                built
            };

            // First attempt.
            let result: rig_http::http_client::Result<http::Response<LazyBody<U>>> =
                client.inner.send(rebuild()).await;

            // --- 401 refresh-and-retry (once) ---
            let result = match result {
                Err(ref e) if Self::is_401(e) => {
                    warn!("got 401, refreshing token and retrying");
                    match client.refresher.refresh_into(&client.store).await {
                        Ok(()) => {
                            let new_token = client.current_token().await;
                            let mut req = rebuild();
                            Self::inject_token(&mut req, &new_token);
                            client.inner.send(req).await
                        }
                        Err(refresh_err) => {
                            warn!(%refresh_err, "token refresh failed after 401");
                            result
                        }
                    }
                }
                other => other,
            };

            // --- 429 / 5xx back-off retries ---
            let mut result = result;
            for attempt in 0..MAX_RETRIES {
                match &result {
                    Err(e) if Self::is_retryable(e) => {
                        warn!(attempt, "retryable error, backing off");
                        backoff(attempt).await;
                        result = client.inner.send(rebuild()).await;
                    }
                    _ => break,
                }
            }

            result
        }
    }

    fn send_multipart<U>(
        &self,
        req: http::Request<MultipartForm>,
    ) -> impl std::future::Future<
        Output = rig_http::http_client::Result<http::Response<LazyBody<U>>>,
    > + Send
           + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        // Multipart bodies can't be cheaply cloned, so we only do token injection
        // (no retry on 401 / 429 for multipart — not needed for our use case).
        let client = self.clone();
        async move {
            let (mut parts, body) = req.into_parts();
            let token = client.current_token().await;
            let value = format!("Bearer {token}");
            parts.headers.insert(
                AUTHORIZATION,
                http::HeaderValue::from_str(&value)
                    .expect("token should be a valid header value"),
            );
            client
                .inner
                .send_multipart(http::Request::from_parts(parts, body))
                .await
        }
    }

    fn send_streaming<T>(
        &self,
        mut req: http::Request<T>,
    ) -> impl std::future::Future<Output = rig_http::http_client::Result<StreamingResponse>>
           + Send
    where
        T: Into<Bytes> + Send,
    {
        // Streaming: inject token and delegate. Retry is not meaningful for
        // an open stream.
        let client = self.clone();
        async move {
            let token = client.current_token().await;
            Self::inject_token(&mut req, &token);
            client.inner.send_streaming(req).await
        }
    }
}

// ---------------------------------------------------------------------------
// Background proactive refresh task
// ---------------------------------------------------------------------------

/// Spawn a background task that refreshes the token before it expires.
///
/// The task wakes up periodically, checks whether the token will expire
/// within [`REFRESH_MARGIN_SECS`] seconds, and refreshes if so. It runs
/// until the process exits (it holds a weak reference via the store's Arc,
/// but since the caller also holds one it effectively runs forever — that is
/// intentional for the CLI binary; library users can drop the returned handle).
pub fn spawn_proactive_refresh(
    store: TokenStore,
    refresher: TokenRefresher,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // Wake up and check every 10 seconds.
            tokio::time::sleep(Duration::from_secs(10)).await;

            let should_refresh = {
                let state = store.read().await;
                state.expires_within(Duration::from_secs(REFRESH_MARGIN_SECS))
            };

            if should_refresh {
                debug!("proactively refreshing token");
                if let Err(e) = refresher.refresh_into(&store).await {
                    warn!(%e, "proactive token refresh failed");
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// ClientBuilder
// ---------------------------------------------------------------------------

/// Convenience builder for the authenticated proxy client.
///
/// # Example
///
/// ```no_run
/// use harness::client::{ClientBuilder, LoginCredentials};
///
/// # async fn run() -> anyhow::Result<()> {
/// let (client, _store) = ClientBuilder::new("https://cloud-wolf.team-stingray.com")
///     .login(LoginCredentials {
///         email: "user@example.com".into(),
///         password: "secret".into(),
///     })
///     .await?
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct ClientBuilder {
    base_url: String,
    state: BuilderState,
}

enum BuilderState {
    #[allow(dead_code)]
    NeedsLogin { credentials: LoginCredentials },
    HasToken { token_state: TokenState, credentials: LoginCredentials },
}

impl ClientBuilder {
    /// Create a builder aimed at `base_url` (e.g. `"https://cloud-wolf.team-stingray.com"`).
    pub fn new(base_url: impl Into<String>) -> Self {
        // Placeholder — caller must call `.login()` before `.build()`.
        Self {
            base_url: base_url.into(),
            state: BuilderState::NeedsLogin {
                credentials: LoginCredentials {
                    email: String::new(),
                    password: String::new(),
                },
            },
        }
    }

    /// Authenticate with `credentials` and obtain an initial JWT.
    ///
    /// Returns `self` so you can chain `.build()`.
    pub async fn login(self, credentials: LoginCredentials) -> Result<Self> {
        let refresher = TokenRefresher::new(self.base_url.clone(), credentials.clone());
        let token_state = refresher.refresh().await?;
        Ok(Self {
            base_url: self.base_url,
            state: BuilderState::HasToken { token_state, credentials },
        })
    }

    /// Build the [`OpenAI`] client and [`TokenStore`], and start the
    /// background refresh task.
    ///
    /// Returns `(client, store)`.  The store lets callers inspect the current
    /// token or replace it; drop the `JoinHandle` returned by
    /// [`spawn_proactive_refresh`] to stop background refresh.
    pub async fn build(self) -> Result<(OpenAI, TokenStore)> {
        let (token_state, credentials) = match self.state {
            BuilderState::HasToken { token_state, credentials } => (token_state, credentials),
            BuilderState::NeedsLogin { .. } => {
                bail!("call .login() before .build()")
            }
        };

        let store: TokenStore = Arc::new(RwLock::new(token_state));
        let refresher = TokenRefresher::new(self.base_url.clone(), credentials);

        // Spawn proactive background refresh.
        spawn_proactive_refresh(Arc::clone(&store), refresher.clone());

        // Build the rig OpenAI client pointed at the proxy with the Mira dialect.
        let transport = HarnessHttpClient::new(Arc::clone(&store), refresher);
        let openai_client = mira_openai_client(&self.base_url, transport);

        Ok((openai_client, store))
    }
}

// ---------------------------------------------------------------------------
// Mira dialect wiring
// ---------------------------------------------------------------------------

/// Build an [`OpenAI`] client using the Mira dialect, aimed at `base_url`,
/// sending through `http`.
///
/// The Mira dialect is an OpenAI-compatible wire with a body rewrite that
/// flattens content-part arrays to plain strings (matching the proxy's
/// deserializer). We use `Auth::OptionalBearer` with an empty key because
/// our transport injects the real token itself.
pub fn mira_openai_client(
    base_url: &str,
    http: impl HttpClientExt + 'static,
) -> OpenAI {
    use rig_core::providers::openai::wire::{BodyRewrite, Dialect};

    // Start from the shared OpenAI dialect definition.
    let mut quirks = rig_core::providers::openai::wire::OPENAI.quirks;
    quirks.rewrite = BodyRewrite::Mira;

    let dialect = Dialect {
        quirks,
        ..rig_core::providers::openai::wire::OPENAI
    };

    rig_core::providers::openai::OpenAIConfig::with_key(&dialect, "")
        .with_base_url(format!("{}/v1", base_url.trim_end_matches('/')))
        .connect(http)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_expiry_future() {
        // A JWT with exp = now + 300 s should give a future Instant.
        use jsonwebtoken::{EncodingKey, Header, encode};
        use serde::Serialize;

        #[derive(Serialize)]
        struct Claims {
            sub: &'static str,
            exp: u64,
        }

        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 300;

        let token = encode(
            &Header::default(),
            &Claims { sub: "test", exp },
            &EncodingKey::from_secret(b"secret"),
        )
        .unwrap();

        let expiry = token_expiry(&token);
        assert!(expiry > Instant::now(), "expiry should be in the future");
        assert!(
            expiry < Instant::now() + Duration::from_secs(310),
            "expiry should be ≈ 300s from now"
        );
    }

    #[test]
    fn token_expiry_past() {
        // A JWT with exp in the past should give an already-elapsed Instant.
        use jsonwebtoken::{EncodingKey, Header, encode};
        use serde::Serialize;

        #[derive(Serialize)]
        struct Claims {
            sub: &'static str,
            exp: u64,
        }

        let exp = 1_000_000u64; // safely in the past
        let token = encode(
            &Header::default(),
            &Claims { sub: "test", exp },
            &EncodingKey::from_secret(b"secret"),
        )
        .unwrap();

        let expiry = token_expiry(&token);
        assert!(
            expiry <= Instant::now(),
            "past token should expire immediately"
        );
    }

    #[test]
    fn inject_token_replaces_header() {
        let mut req = http::Request::builder()
            .uri("https://example.com/v1/chat/completions")
            .header(AUTHORIZATION, "Bearer old-token")
            .body(Bytes::new())
            .unwrap();

        HarnessHttpClient::inject_token(&mut req, "new-token");

        let auth = req.headers().get(AUTHORIZATION).unwrap();
        assert_eq!(auth.to_str().unwrap(), "Bearer new-token");
    }

    #[test]
    fn is_401_detects_unauthorized() {
        let err = HttpError::non_success_with_details(
            http::StatusCode::UNAUTHORIZED,
            http::HeaderMap::new(),
            "Unauthorized".into(),
        );
        assert!(HarnessHttpClient::is_401(&err));
    }

    #[test]
    fn is_retryable_detects_429_and_500() {
        let err_429 = HttpError::non_success_with_details(
            http::StatusCode::TOO_MANY_REQUESTS,
            http::HeaderMap::new(),
            "rate limited".into(),
        );
        let err_500 = HttpError::non_success_with_details(
            http::StatusCode::INTERNAL_SERVER_ERROR,
            http::HeaderMap::new(),
            "server error".into(),
        );
        let err_400 = HttpError::non_success_with_details(
            http::StatusCode::BAD_REQUEST,
            http::HeaderMap::new(),
            "bad request".into(),
        );
        assert!(HarnessHttpClient::is_retryable(&err_429));
        assert!(HarnessHttpClient::is_retryable(&err_500));
        assert!(!HarnessHttpClient::is_retryable(&err_400));
    }
}

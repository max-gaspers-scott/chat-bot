//! Integration test: token refresh mid-session.
//!
//! This test does NOT hit the real proxy.  It stands up a tiny Tokio mock
//! HTTP server that:
//!   1. Returns 401 on requests whose `Authorization` header carries the old
//!      token.
//!   2. Returns 200 on a `POST /api/login` request (the "refresh" endpoint).
//!   3. Returns 200 on requests carrying the new token.
//!
//! The test mints a JWT with a 2-second expiry, waits for the proactive
//! refresh to fire (or forces a 401 retry), and confirms the response
//! succeeds with the new token.

use std::sync::Arc;
use std::time::{Duration, Instant};

use harness::client::{
    HarnessHttpClient, LoginCredentials, TokenRefresher, TokenState, TokenStore,
};
use tokio::sync::{RwLock, mpsc};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Mint a JWT expiring `ttl` seconds from now, signed with `secret`.
fn mint_jwt(subject: &str, ttl_secs: u64, secret: &[u8]) -> String {
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde::Serialize;

    #[derive(Serialize)]
    struct Claims<'a> {
        sub: &'a str,
        exp: u64,
    }

    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + ttl_secs;

    encode(
        &Header::default(),
        &Claims { sub: subject, exp },
        &EncodingKey::from_secret(secret),
    )
    .expect("JWT encode should succeed")
}

// ---------------------------------------------------------------------------
// Unit tests (no network)
// ---------------------------------------------------------------------------

#[test]
fn token_state_expires_within() {
    let state = TokenState {
        token: "tok".into(),
        expires_at: Instant::now() + Duration::from_secs(30),
    };
    assert!(state.expires_within(Duration::from_secs(60)));
    assert!(!state.expires_within(Duration::from_secs(10)));
}

#[test]
fn token_store_is_updated() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let store: TokenStore = Arc::new(RwLock::new(TokenState {
            token: "old".into(),
            expires_at: Instant::now() + Duration::from_secs(10),
        }));

        {
            let mut w = store.write().await;
            *w = TokenState {
                token: "new".into(),
                expires_at: Instant::now() + Duration::from_secs(3600),
            };
        }

        let token = store.read().await.token.clone();
        assert_eq!(token, "new");
    });
}

// ---------------------------------------------------------------------------
// Refresh-works test using a mock HTTP server
// ---------------------------------------------------------------------------

/// A minimal mock server that handles:
///   POST /api/login  → 200 { "res": "success", "token": <new_token> }
///   *                 → 200 or 401 depending on the Authorization header
///
/// We use `tokio`'s built-in TcpListener so there are no extra dependencies.
#[tokio::test]
async fn refresh_works_with_short_lived_token() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let old_token = mint_jwt("user", 2, b"old_secret"); // expires in 2s
    let new_token = mint_jwt("user", 3600, b"new_secret");

    let new_token_clone = new_token.clone();
    let old_token_clone = old_token.clone();

    // --- spin up mock server ---
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    let server = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                result = listener.accept() => {
                    let (mut stream, _) = result.unwrap();
                    let old = old_token_clone.clone();
                    let new = new_token_clone.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 8192];
                        let n = stream.read(&mut buf).await.unwrap();
                        let request = String::from_utf8_lossy(&buf[..n]);

                        let response = if request.contains("POST /api/login") {
                            // Respond with a fresh token
                            let body = format!(
                                r#"{{"res":"success","token":"{}"}}"#,
                                new
                            );
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                                body.len(),
                                body
                            )
                        } else if request.contains(&format!("Bearer {old}")) {
                            // Old token → 401
                            let body = r#"{"error":"token expired"}"#;
                            format!(
                                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                                body.len(),
                                body
                            )
                        } else {
                            // Assume the new token → respond with a dummy chat completion
                            let body = r#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"gemini-2.5-flash","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                                body.len(),
                                body
                            )
                        };

                        stream.write_all(response.as_bytes()).await.unwrap();
                    });
                }
            }
        }
    });

    // --- build client with old (short-lived) token ---
    let base_url = format!("http://{addr}");

    let store: TokenStore = Arc::new(RwLock::new(TokenState {
        token: old_token.clone(),
        expires_at: Instant::now() + Duration::from_secs(2),
    }));

    let credentials = LoginCredentials {
        email: "user@example.com".into(),
        password: "secret".into(),
    };

    let refresher = TokenRefresher::new_exposed(base_url.clone(), credentials);
    let transport = HarnessHttpClient::from_parts(Arc::clone(&store), refresher.clone());
    let rig_client = harness::client::mira_openai_client(&base_url, transport);

    // --- attempt a chat completion with the about-to-expire token ---
    // The transport will get a 401, refresh, and retry with the new token.
    let agent = rig::AgentBuilder::new(rig_client.chat("gemini-2.5-flash"))
        .preamble("You are helpful.")
        .build();

    let reply = agent.prompt("hello").await;
    assert!(reply.is_ok(), "chat should succeed after token refresh: {reply:?}");

    // Confirm the store now holds the new token.
    let current = store.read().await.token.clone();
    assert_eq!(current, new_token, "store should hold the refreshed token");

    // Shut down mock server.
    let _ = shutdown_tx.send(()).await;
    let _ = server.await;
}

//! Port of the Python `tests/test_client.py` retry / reauth / error-handling
//! tests, running against an `httpmock` server instead of a monkeypatched
//! transport. Two adaptations to real HTTP fakes are called out per test:
//!
//! - httpmock cannot return a response *sequence* for one path (the first
//!   created matching mock always wins), so success-after-retry tests swap
//!   mocks while the client sleeps out the `Retry-After: 1` backoff.
//! - The 401 reauth test distinguishes attempts by their `Authorization`
//!   header (the retried request carries the refreshed token) and exercises
//!   the real refresh endpoint instead of a patched `refresh_token`.

use std::time::Duration;

use appflowy_mcp::client::AppFlowyClient;
use appflowy_mcp::error::Error;
use httpmock::prelude::*;
use httpmock::Mock;
use reqwest::Method;
use serde_json::json;

async fn make_client(server: &MockServer) -> AppFlowyClient {
    let client = AppFlowyClient::new(Some("e".into()), Some("p".into()), server.base_url());
    client.set_access_token(Some("t".into())).await;
    client.set_refresh_token(Some("r".into())).await;
    client
}

/// Poll until `mock` has been hit `n` times, i.e. the client fired the
/// request and is now backing off before its retry.
async fn wait_hits(mock: &Mock<'_>, n: usize) {
    for _ in 0..250 {
        if mock.calls_async().await >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("mock was not hit {n} time(s) within 5s");
}

#[tokio::test]
async fn get_retried_on_5xx() {
    let server = MockServer::start_async().await;
    let boom = server
        .mock_async(|when, then| {
            when.method(GET).path("/x");
            then.status(500)
                .header("retry-after", "1")
                .json_body(json!({"message": "boom"}));
        })
        .await;
    let client = make_client(&server).await;
    let pending = tokio::spawn({
        let client = client.clone();
        async move { client.request(Method::GET, "/x", None, None).await }
    });
    wait_hits(&boom, 1).await;
    boom.delete_async().await;
    let ok = server
        .mock_async(|when, then| {
            when.method(GET).path("/x");
            then.status(200).json_body(json!({"data": "ok"}));
        })
        .await;

    let out = pending.await.unwrap().unwrap();
    // The envelope unwrap (`body.get("data", body)`) is centralized in
    // `handle_response`, so the payload is returned directly.
    assert_eq!(out, json!("ok"));
    assert_eq!(ok.calls_async().await, 1);
}

#[tokio::test]
async fn post_not_retried_on_5xx() {
    let server = MockServer::start_async().await;
    let boom = server
        .mock_async(|when, then| {
            when.method(POST).path("/x");
            then.status(500).json_body(json!({"message": "boom"}));
        })
        .await;
    let client = make_client(&server).await;

    let err = client
        .request(Method::POST, "/x", None, Some(json!({})))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(500));
    assert_eq!(boom.calls_async().await, 1); // not retried -> no duplicate write
}

#[tokio::test]
async fn post_retried_on_429() {
    // 429 means the request was rejected, not applied, so retry is safe for POST.
    let server = MockServer::start_async().await;
    let rate = server
        .mock_async(|when, then| {
            when.method(POST).path("/x");
            then.status(429)
                .header("retry-after", "1")
                .json_body(json!({"message": "rate"}));
        })
        .await;
    let client = make_client(&server).await;
    let pending = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .request(Method::POST, "/x", None, Some(json!({})))
                .await
        }
    });
    wait_hits(&rate, 1).await;
    rate.delete_async().await;
    let ok = server
        .mock_async(|when, then| {
            when.method(POST).path("/x");
            then.status(200).json_body(json!({"data": "ok"}));
        })
        .await;

    let out = pending.await.unwrap().unwrap();
    assert_eq!(out, json!("ok"));
    assert_eq!(ok.calls_async().await, 1);
}

#[tokio::test]
async fn status_401_triggers_reauth_and_retry() {
    // Attempts are told apart by their Authorization header: the first
    // request still carries "Bearer t", the retry carries the refreshed
    // token, so no mock swapping is needed.
    let server = MockServer::start_async().await;
    let expired = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/x")
                .header("Authorization", "Bearer t");
            then.status(401).json_body(json!({"message": "expired"}));
        })
        .await;
    let ok = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/x")
                .header("Authorization", "Bearer new");
            then.status(200).json_body(json!({"data": "ok"}));
        })
        .await;
    let refresh = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/gotrue/token")
                .query_param("grant_type", "refresh_token")
                .json_body(json!({"refresh_token": "r"}));
            then.status(200).json_body(json!({
                "access_token": "new",
                "refresh_token": "r2",
                "expires_in": 3600,
            }));
        })
        .await;
    let client = make_client(&server).await;

    let out = client.request(Method::GET, "/x", None, None).await.unwrap();
    assert_eq!(out, json!("ok"));
    assert_eq!(client.access_token().await.as_deref(), Some("new"));
    assert_eq!(client.refresh_token_value().await.as_deref(), Some("r2"));
    expired.assert_calls_async(1).await;
    ok.assert_calls_async(1).await;
    refresh.assert_calls_async(1).await;
}

#[tokio::test]
async fn error_preserves_status_and_code() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/x");
            then.status(400)
                .json_body(json!({"message": "bad", "code": 1026}));
        })
        .await;
    let client = make_client(&server).await;

    let err = client
        .request(Method::GET, "/x", None, None)
        .await
        .unwrap_err();
    match err {
        Error::Api { status, code, .. } => {
            assert_eq!(status, Some(400));
            assert_eq!(code, Some(1026));
        }
        other => panic!("expected Error::Api, got {other:?}"),
    }
}

#[tokio::test]
async fn business_error_200_envelope() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/x");
            then.status(200)
                .json_body(json!({"code": 1026, "message": "limit"}));
        })
        .await;
    let client = make_client(&server).await;

    let err = client
        .request(Method::GET, "/x", None, None)
        .await
        .unwrap_err();
    match err {
        Error::Api { status, code, .. } => {
            assert_eq!(status, Some(200));
            assert_eq!(code, Some(1026));
        }
        other => panic!("expected Error::Api, got {other:?}"),
    }
}

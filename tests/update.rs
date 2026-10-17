//! Update HTTP helpers against an `httpmock` GitHub stand-in: latest-release
//! resolution and asset download, success and failure paths. The platform
//! replace/sync mechanics are covered by unit tests in `src/update.rs`.

use appflowy_mcp::update;
use httpmock::prelude::*;

#[tokio::test]
async fn resolves_latest_tag() {
    let server = MockServer::start_async().await;
    let latest = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/weironz/appflowy_mcp/releases/latest");
            then.status(200)
                .json_body(serde_json::json!({"tag_name": "v9.9.9"}));
        })
        .await;
    let tag = update::resolve_latest_tag(&reqwest::Client::new(), &server.base_url())
        .await
        .unwrap();
    assert_eq!(tag, "v9.9.9");
    latest.assert_async().await;
}

#[tokio::test]
async fn latest_tag_errors_carry_status_and_snippet() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/weironz/appflowy_mcp/releases/latest");
            then.status(403)
                .json_body(serde_json::json!({"message": "rate limit exceeded"}));
        })
        .await;
    let err = update::resolve_latest_tag(&reqwest::Client::new(), &server.base_url())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("403"), "unexpected error: {err}");
    assert!(err.contains("rate limit"), "unexpected error: {err}");
    assert!(err.contains("--version"), "unexpected error: {err}");
}

#[tokio::test]
async fn downloads_asset_bytes() {
    let server = MockServer::start_async().await;
    let payload = b"\x7fELF fake binary bytes";
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/weironz/appflowy_mcp/releases/download/v9.9.9/appflowy-linux-x64");
            then.status(200).body(payload.as_slice());
        })
        .await;
    let bytes = update::download_asset(
        &reqwest::Client::new(),
        &server.base_url(),
        "v9.9.9",
        "appflowy-linux-x64",
    )
    .await
    .unwrap();
    assert_eq!(bytes, payload);
}

#[tokio::test]
async fn asset_404_points_at_platform_availability() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/weironz/appflowy_mcp/releases/download/v9.9.9/appflowy-macos-x64");
            then.status(404).body("Not Found");
        })
        .await;
    let err = update::download_asset(
        &reqwest::Client::new(),
        &server.base_url(),
        "v9.9.9",
        "appflowy-macos-x64",
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("404"), "unexpected error: {err}");
    assert!(err.contains("build from source"), "unexpected error: {err}");
}

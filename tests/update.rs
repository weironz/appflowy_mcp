//! Update HTTP helpers against an `httpmock` GitHub stand-in: latest/pinned
//! release resolution (with asset digests), the optional GitHub API bearer
//! token, and asset download with sha256 verification, success and failure
//! paths. The platform replace/sync mechanics are covered by unit tests in
//! `src/update.rs`.

use appflowy_mcp::update;
use httpmock::prelude::*;

/// sha256("\x7fELF fake binary bytes"), the payload the download mocks serve.
const PAYLOAD_SHA256: &str = "7a41c64a3b65332ea84ea720090be6dff352f8db73ad0148d737782f9435f93b";
/// sha256("other bytes") — a valid digest of something else, for mismatches.
const OTHER_SHA256: &str = "a3ead5eedad5df82318c51685dbc1c147a36d1ff8584fc82de6b08d0bf63a795";

#[tokio::test]
async fn resolves_latest_tag() {
    let server = MockServer::start_async().await;
    let latest = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/weironz/appflowy_mcp/releases/latest");
            then.status(200).json_body(serde_json::json!({
                "tag_name": "v9.9.9",
                "assets": [
                    {"name": "appflowy-linux-x64", "digest": format!("sha256:{PAYLOAD_SHA256}")},
                    {"name": "appflowy-macos-aarch64"}
                ]
            }));
        })
        .await;
    let release = update::resolve_latest_tag(&reqwest::Client::new(), &server.base_url(), None)
        .await
        .unwrap();
    assert_eq!(release.tag, "v9.9.9");
    assert_eq!(
        release.sha256_digest("appflowy-linux-x64"),
        Some(PAYLOAD_SHA256)
    );
    // No digest recorded (pre-2025 release or a digestless asset): nothing
    // to verify against, so no digest is offered.
    assert_eq!(release.sha256_digest("appflowy-macos-aarch64"), None);
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
    let err = update::resolve_latest_tag(&reqwest::Client::new(), &server.base_url(), None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("403"), "unexpected error: {err}");
    assert!(err.contains("rate limit"), "unexpected error: {err}");
    assert!(err.contains("--version"), "unexpected error: {err}");
}

#[tokio::test]
async fn fetches_pinned_release_by_tag() {
    let server = MockServer::start_async().await;
    let by_tag = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/weironz/appflowy_mcp/releases/tags/v0.9.0");
            then.status(200).json_body(serde_json::json!({
                "tag_name": "v0.9.0",
                "assets": [
                    {"name": "appflowy-linux-x64", "digest": format!("sha256:{PAYLOAD_SHA256}")}
                ]
            }));
        })
        .await;
    let release =
        update::fetch_release_by_tag(&reqwest::Client::new(), &server.base_url(), None, "v0.9.0")
            .await
            .unwrap();
    assert_eq!(release.tag, "v0.9.0");
    assert_eq!(
        release.sha256_digest("appflowy-linux-x64"),
        Some(PAYLOAD_SHA256)
    );
    by_tag.assert_async().await;
}

#[tokio::test]
async fn sends_bearer_token_on_api_requests() {
    let server = MockServer::start_async().await;
    // The mock only matches when the Authorization header is exactly the
    // token the caller threaded in — a request without it falls through to
    // the default 404 and fails the resolve.
    let authorized = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/weironz/appflowy_mcp/releases/latest")
                .header("Authorization", "Bearer secret-token");
            then.status(200)
                .json_body(serde_json::json!({"tag_name": "v9.9.9"}));
        })
        .await;
    let release = update::resolve_latest_tag(
        &reqwest::Client::new(),
        &server.base_url(),
        Some("secret-token"),
    )
    .await
    .unwrap();
    assert_eq!(release.tag, "v9.9.9");
    authorized.assert_async().await;
}

#[tokio::test]
async fn downloads_asset_bytes() {
    let server = MockServer::start_async().await;
    let payload = b"\x7fELF fake binary bytes";
    let download = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/weironz/appflowy_mcp/releases/download/v9.9.9/appflowy-linux-x64")
                // The download host must never see the API credentials.
                .header_missing("Authorization");
            then.status(200).body(payload.as_slice());
        })
        .await;
    let bytes = update::download_asset(
        &reqwest::Client::new(),
        &server.base_url(),
        "v9.9.9",
        "appflowy-linux-x64",
        None,
    )
    .await
    .unwrap();
    assert_eq!(bytes, payload);
    download.assert_async().await;
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
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("404"), "unexpected error: {err}");
    assert!(err.contains("build from source"), "unexpected error: {err}");
}

#[tokio::test]
async fn digest_match_passes() {
    let server = MockServer::start_async().await;
    let payload = b"\x7fELF fake binary bytes";
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/weironz/appflowy_mcp/releases/download/v9.9.9/appflowy-linux-x64");
            then.status(200).body(payload.as_slice());
        })
        .await;
    // GitHub writes digests lowercase; comparison stays case-insensitive so
    // a differently-cased source still verifies.
    let uppercase = PAYLOAD_SHA256.to_uppercase();
    let bytes = update::download_asset(
        &reqwest::Client::new(),
        &server.base_url(),
        "v9.9.9",
        "appflowy-linux-x64",
        Some(&uppercase),
    )
    .await
    .unwrap();
    assert_eq!(bytes, payload);
}

#[tokio::test]
async fn digest_mismatch_errors() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/weironz/appflowy_mcp/releases/download/v9.9.9/appflowy-linux-x64");
            then.status(200)
                .body(b"\x7fELF tampered binary bytes".as_slice());
        })
        .await;
    let err = update::download_asset(
        &reqwest::Client::new(),
        &server.base_url(),
        "v9.9.9",
        "appflowy-linux-x64",
        Some(OTHER_SHA256),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("checksum mismatch"), "unexpected error: {err}");
    // Both sides of the comparison are named so the user can see which one
    // to trust.
    assert!(err.contains(OTHER_SHA256), "unexpected error: {err}");
}

#[tokio::test]
async fn missing_digest_proceeds() {
    let server = MockServer::start_async().await;
    let payload = b"\x7fELF fake binary bytes";
    // A release that predates GitHub's digest field: the asset records no
    // sha256, so nothing is verifiable and the download proceeds, guarded
    // only by the `--version` smoke test.
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/weironz/appflowy_mcp/releases/tags/v0.8.0");
            then.status(200).json_body(serde_json::json!({
                "tag_name": "v0.8.0",
                "assets": [{"name": "appflowy-linux-x64"}]
            }));
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/weironz/appflowy_mcp/releases/download/v0.8.0/appflowy-linux-x64");
            then.status(200).body(payload.as_slice());
        })
        .await;
    let release =
        update::fetch_release_by_tag(&reqwest::Client::new(), &server.base_url(), None, "v0.8.0")
            .await
            .unwrap();
    let expected = release.sha256_digest("appflowy-linux-x64");
    assert_eq!(expected, None);
    let bytes = update::download_asset(
        &reqwest::Client::new(),
        &server.base_url(),
        "v0.8.0",
        "appflowy-linux-x64",
        expected,
    )
    .await
    .unwrap();
    assert_eq!(bytes, payload);
}

//! Streamable HTTP transport: serve `mcp_http_router` on an ephemeral
//! loopback port and drive it with real HTTP requests.

use appflowy_mcp::cli::mcp_http_router;
use appflowy_mcp::client::AppFlowyClient;
use appflowy_mcp::server::AppServer;
use httpmock::prelude::*;
use serde_json::json;

const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;

async fn spawn_server() -> String {
    spawn_server_with(AppServer::from_env()).await
}

async fn spawn_server_with(server: AppServer) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mcp_http_router(&server, false))
            .await
            .unwrap();
    });
    format!("http://{addr}/mcp")
}

fn post(client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
}

/// A response body is either plain JSON or an SSE stream. In SSE form each
/// event's payload is the `data:` lines joined per the SSE spec (a JSON value
/// may span several of them), events separated by blank lines.
fn rpc_json(body: &str) -> serde_json::Value {
    if body.trim_start().starts_with('{') {
        return serde_json::from_str(body).expect("valid JSON-RPC body");
    }
    for event in body.split("\n\n") {
        let data: Vec<&str> = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect();
        if data.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&data.join("\n")) {
            return value;
        }
    }
    panic!("no JSON-RPC event in body: {body:?}");
}

/// Run the initialize handshake and return the session id.
async fn open_session(client: &reqwest::Client, url: &str) -> String {
    let init = post(client, url).body(INIT).send().await.unwrap();
    assert_eq!(init.status(), 200);
    let session = init
        .headers()
        .get("mcp-session-id")
        .expect("session header")
        .to_str()
        .unwrap()
        .to_string();
    let init_body = rpc_json(&init.text().await.unwrap());
    assert_eq!(init_body["result"]["serverInfo"]["name"], "appflowy");

    let initialized = post(client, url)
        .header("mcp-session-id", &session)
        .body(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .send()
        .await
        .unwrap();
    assert!(initialized.status().is_success());
    session
}

#[tokio::test]
async fn http_initialize_and_list_tools() {
    let url = spawn_server().await;
    let client = reqwest::Client::new();
    let session = open_session(&client, &url).await;

    let list = post(&client, &url)
        .header("mcp-session-id", &session)
        .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200);
    let list_body = rpc_json(&list.text().await.unwrap());
    let tools = list_body["result"]["tools"].as_array().unwrap();
    // Loose bound instead of an exact count so adding a tool doesn't force an
    // edit here; duplicates/missing routers would still break other tests.
    assert!(
        tools.len() > 100,
        "expected 100+ tools, got {}",
        tools.len()
    );
}

#[tokio::test]
async fn http_tool_validation_error_is_tool_result() {
    // SEP-1303: input validation is a tool *execution* error — an isError
    // tool result the model can read and self-correct, not a -32602
    // protocol error that most clients render opaquely.
    let url = spawn_server().await;
    let client = reqwest::Client::new();
    let session = open_session(&client, &url).await;

    let call = post(&client, &url)
        .header("mcp-session-id", &session)
        .body(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"appflowy_get_row_details","arguments":{"workspace_id":"ws","database_id":"db","row_ids":"","with_doc":false}}}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(call.status(), 200);
    let body = rpc_json(&call.text().await.unwrap());
    assert!(
        body.get("error").is_none(),
        "validation must not be a protocol error: {body}"
    );
    let result = &body["result"];
    assert_eq!(result["isError"], json!(true), "body: {body}");
    let text = result["content"][0]["text"].as_str().expect("text content");
    assert_eq!(result["content"][0]["type"], json!("text"));
    assert!(
        text.contains("At least one row ID is required."),
        "original message must be preserved, got: {text}"
    );
}

#[tokio::test]
async fn http_api_error_stays_protocol_error() {
    // Non-validation failures (here an AppFlowy API rejection) keep the
    // JSON-RPC protocol error path with status/code context.
    let backend = MockServer::start_async().await;
    let mock = backend
        .mock_async(|when, then| {
            when.method(GET).path("/api/workspace");
            then.status(400)
                .json_body(json!({"code": 1000, "message": "boom"}));
        })
        .await;
    let server = AppServer {
        client: AppFlowyClient::new(None, None, backend.base_url()),
    };
    let url = spawn_server_with(server).await;
    let client = reqwest::Client::new();
    let session = open_session(&client, &url).await;

    let call = post(&client, &url)
        .header("mcp-session-id", &session)
        .body(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"appflowy_list_workspaces","arguments":{}}}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(call.status(), 200);
    let body = rpc_json(&call.text().await.unwrap());
    let error = &body["error"];
    assert_eq!(error["code"], json!(-32603), "body: {body}");
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("boom"), "message: {message}");
    assert!(message.contains("HTTP 400"), "message: {message}");
    assert!(body.get("result").is_none(), "body: {body}");
    mock.assert_calls_async(1).await;
}

#[tokio::test]
async fn http_rejects_foreign_host_header() {
    let url = spawn_server().await;
    let resp = post(&reqwest::Client::new(), &url)
        .header("Host", "evil.example.com")
        .body(INIT)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_client_error(),
        "foreign Host must be rejected, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn http_rejects_browser_origin() {
    let url = spawn_server().await;
    let resp = post(&reqwest::Client::new(), &url)
        .header("Origin", "https://evil.example.com")
        .body(INIT)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_client_error(),
        "browser Origin must be rejected, got {}",
        resp.status()
    );
}

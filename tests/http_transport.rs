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
async fn http_workspace_zip_export_without_task_id_reports_response() {
    // When the backend answers without `task_id`, the error message must
    // interpolate the actual response body — a literal "{data}" tells the
    // caller nothing. Regression for the missing `format!`.
    let backend = MockServer::start_async().await;
    let mock = backend
        .mock_async(|when, then| {
            when.method(POST).path("/api/export/workspace/ws");
            then.status(200)
                .json_body(json!({"detail": "unexpected envelope"}));
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
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"appflowy_export_workspace_zip","arguments":{"workspace_id":"ws"}}}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(call.status(), 200);
    let body = rpc_json(&call.text().await.unwrap());
    let message = body["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a protocol error, got: {body}"));
    assert!(
        message.contains("unexpected envelope"),
        "message: {message}"
    );
    assert!(!message.contains("{data}"), "message: {message}");
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

/// All SSE events of a response body as parsed JSON (empty if the body was
/// not an SSE stream — e.g. a plain JSON response).
fn sse_json_events(body: &str) -> Vec<serde_json::Value> {
    body.split("\n\n")
        .filter_map(|event| {
            let data: Vec<&str> = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                .collect();
            if data.is_empty() {
                return None;
            }
            serde_json::from_str::<serde_json::Value>(&data.join("\n")).ok()
        })
        .collect()
}

/// Backend for the progress e2e: one space with two document pages. The
/// page content is irrelevant — the fetches fail (404 both paths), so the
/// export records warnings, but the prefetch loop still ticks one
/// `notifications/progress` per page.
async fn export_progress_backend() -> (AppServer, MockServer) {
    let backend = MockServer::start_async().await;
    backend
        .mock_async(|when, then| {
            when.method(GET).path("/api/workspace/ws/folder");
            then.status(200).json_body(json!({
                "data": {
                    "view_id": "sp", "name": "Space", "is_space": true, "layout": 0,
                    "children": [
                        {"view_id": "p1", "name": "P1", "is_space": false, "layout": 0, "children": []},
                        {"view_id": "p2", "name": "P2", "is_space": false, "layout": 0, "children": []},
                    ],
                }
            }));
        })
        .await;
    for view_id in ["p1", "p2"] {
        for path in [
            format!("/api/workspace/ws/page-view/{view_id}"),
            format!("/api/workspace/v1/ws/collab/{view_id}"),
        ] {
            backend
                .mock_async(move |when, then| {
                    when.method(GET).path(path);
                    then.status(404)
                        .json_body(json!({"code": 1016, "message": "Collab not found"}));
                })
                .await;
        }
    }
    let server = AppServer {
        client: AppFlowyClient::new(None, None, backend.base_url()),
    };
    (server, backend)
}

fn export_space_call(id: u32, dir: &std::path::Path, with_token: bool) -> String {
    let mut params = json!({
        "name": "appflowy_export_space",
        "arguments": {
            "workspace_id": "ws",
            "space_view_id": "sp",
            "request": {"path": dir.to_string_lossy()},
        },
    });
    if with_token {
        params["_meta"] = json!({"progressToken": "export-1"});
    }
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params}).to_string()
}

fn fresh_export_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "af-progress-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[tokio::test]
async fn http_export_space_reports_progress_for_token() {
    let (server, _backend) = export_progress_backend().await;
    let url = spawn_server_with(server).await;
    let client = reqwest::Client::new();
    let session = open_session(&client, &url).await;

    let dir = fresh_export_dir("token");
    let call = post(&client, &url)
        .header("mcp-session-id", &session)
        .body(export_space_call(9, &dir, true))
        .send()
        .await
        .unwrap();
    assert_eq!(call.status(), 200);
    let body = call.text().await.unwrap();
    std::fs::remove_dir_all(&dir).unwrap();

    let events = sse_json_events(&body);
    let progresses: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| event["method"] == "notifications/progress")
        .collect();
    assert_eq!(
        progresses.len(),
        2,
        "one progress notification per page expected, body: {body}"
    );
    for (i, event) in progresses.iter().enumerate() {
        let params = &event["params"];
        assert_eq!(params["progressToken"], json!("export-1"));
        let done = i + 1;
        assert_eq!(params["progress"], json!(done as f64), "params: {params}");
        assert_eq!(params["total"], json!(2.0), "params: {params}");
        assert_eq!(
            params["message"],
            json!(format!("exporting {done}/2 pages")),
            "params: {params}"
        );
    }
    // The tool call itself still completes as a normal result event.
    let result = events
        .iter()
        .find(|event| event.get("id").is_some())
        .expect("tool result event");
    assert!(
        result.get("error").is_none(),
        "export must not be a protocol error: {result}"
    );
}

#[tokio::test]
async fn http_export_space_sends_no_progress_without_token() {
    // MCP progress is optional: without a `_meta.progressToken` the call
    // must behave exactly as before — zero notifications.
    let (server, _backend) = export_progress_backend().await;
    let url = spawn_server_with(server).await;
    let client = reqwest::Client::new();
    let session = open_session(&client, &url).await;

    let dir = fresh_export_dir("no-token");
    let call = post(&client, &url)
        .header("mcp-session-id", &session)
        .body(export_space_call(10, &dir, false))
        .send()
        .await
        .unwrap();
    assert_eq!(call.status(), 200);
    let body = call.text().await.unwrap();
    std::fs::remove_dir_all(&dir).unwrap();

    let events = sse_json_events(&body);
    assert!(
        events
            .iter()
            .all(|event| event["method"] != "notifications/progress"),
        "no progress without a token, body: {body}"
    );
    let result = events
        .iter()
        .find(|event| event.get("id").is_some())
        .expect("tool result event");
    assert!(result.get("error").is_none(), "body: {body}");
}

//! Streamable HTTP transport: serve `mcp_http_router` on an ephemeral
//! loopback port and drive it with real HTTP requests.

use appflowy_mcp::cli::mcp_http_router;
use appflowy_mcp::server::AppServer;

const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;

async fn spawn_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = AppServer::from_env();
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

#[tokio::test]
async fn http_initialize_and_list_tools() {
    let url = spawn_server().await;
    let client = reqwest::Client::new();

    let init = post(&client, &url).body(INIT).send().await.unwrap();
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

    let initialized = post(&client, &url)
        .header("mcp-session-id", &session)
        .body(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .send()
        .await
        .unwrap();
    assert!(initialized.status().is_success());

    let list = post(&client, &url)
        .header("mcp-session-id", &session)
        .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200);
    let list_body = rpc_json(&list.text().await.unwrap());
    let tools = list_body["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 114);
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

//! MCP server assembly.
//!
//! The Python `appflowy_mcp/server.py` builds a module-global client and
//! repeats ensure_authenticated() in its tools. Here the client performs
//! that lazily on 401 (see AppFlowyClient), and the server does one
//! best-effort login at startup when APPFLOWY_EMAIL/APPFLOWY_PASSWORD are
//! both set — a failure only logs a warning, the server still comes up.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{tool_handler, ServerHandler};

use crate::client::{AppFlowyClient, DEFAULT_BASE_URL};

/// The MCP server. `Clone` because the underlying client is `Arc`-backed.
#[derive(Clone)]
pub struct AppServer {
    pub client: AppFlowyClient,
}

impl AppServer {
    /// Build from the environment, mirroring Python `server.py` L53-62
    /// (`APPFLOWY_EMAIL` / `APPFLOWY_PASSWORD` / `APPFLOWY_BASE_URL`).
    pub fn from_env() -> Self {
        let base_url = std::env::var("APPFLOWY_BASE_URL")
            .ok()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Self {
            client: AppFlowyClient::new(
                std::env::var("APPFLOWY_EMAIL").ok(),
                std::env::var("APPFLOWY_PASSWORD").ok(),
                base_url,
            ),
        }
    }

    /// Best-effort startup login. Failure is logged, never fatal: the tools
    /// surface the same error (and the client retries auth lazily on 401).
    pub async fn warm_login(&self) {
        if self.client.email().await.is_none() || self.client.password().await.is_none() {
            tracing::info!("no APPFLOWY_EMAIL/APPFLOWY_PASSWORD set; skipping startup login");
            return;
        }
        match self.client.login().await {
            Ok(_) => tracing::info!("logged in to AppFlowy"),
            Err(e) => tracing::warn!("startup login failed: {e}"),
        }
    }

    /// Aggregate every per-module tool router. Each `tools/*.rs` module
    /// declares its tools on an `impl AppServer` block annotated with
    /// `#[tool_router(router = <name>, vis = "pub(crate)")]`; add the
    /// resulting `Self::<name>()` here.
    fn tool_router() -> ToolRouter<Self> {
        Self::auth_tool_router()
            + Self::workspace_tool_router()
            + Self::member_tool_router()
            + Self::database_tool_router()
            + Self::pages_tool_router()
            + Self::content_tool_router()
            + Self::publish_tool_router()
            + Self::search_tool_router()
            + Self::quicknotes_tool_router()
            + Self::chat_tool_router()
            + Self::upload_tool_router()
            + Self::export_tool_router()
            + Self::comments_tool_router()
            + Self::sharing_tool_router()
            + Self::docedit_tool_router()
            + Self::imports_tool_router()
            + Self::notifications_tool_router()
            + Self::meeting_tool_router()
    }
}

#[tool_handler]
impl ServerHandler for AppServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("appflowy", env!("CARGO_PKG_VERSION")))
            // Server-level usage guidance for the client's model, delivered in
            // the initialize response (the native MCP counterpart to the
            // per-tool descriptions).
            .with_instructions(include_str!("instructions.md"))
    }
}

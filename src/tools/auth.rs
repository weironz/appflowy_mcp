//! Authentication tools (`appflowy_login`, `appflowy_refresh_token`).

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::server::AppServer;
use crate::tools::util::{map_tool_error, ToolError};

/// Mirrors Python `models.py::LoginRequest`.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct LoginRequest {
    /// AppFlowy account email. Falls back to the APPFLOWY_EMAIL env var.
    pub email: Option<String>,
    /// AppFlowy account password. Falls back to the APPFLOWY_PASSWORD env var.
    pub password: Option<String>,
}

/// Mirrors Python `models.py::RefreshTokenRequest`.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct RefreshTokenRequest {
    /// Candidate refresh token to refresh with. It replaces the stored one
    /// only if the refresh succeeds; a failure keeps the previously stored
    /// token. Uses the stored token when omitted.
    pub refresh_token: Option<String>,
}

#[tool_router(router = auth_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_login",
        description = "Login to AppFlowy. Tokens are stored in the server; they are not returned to avoid leaking credentials into tool output.",
        annotations(title = "Login", open_world_hint = true)
    )]
    async fn appflowy_login(
        &self,
        Parameters(request): Parameters<LoginRequest>,
    ) -> Result<Json<Value>, ToolError> {
        if let Some(email) = request.email {
            self.client.set_email(Some(email)).await;
        }
        if let Some(password) = request.password {
            self.client.set_password(Some(password)).await;
        }
        if self.client.email().await.is_none() || self.client.password().await.is_none() {
            return Err(ToolError::validation("Email and password must be provided either in the request or via APPFLOWY_EMAIL and APPFLOWY_PASSWORD env vars"));
        }
        // Locked variant: serializes against 401-triggered reauth so the two
        // never race on the rotating refresh token.
        self.client.login_locked().await.map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "authenticated" })))
    }

    #[tool(
        name = "appflowy_refresh_token",
        description = "Refresh the access token. Uses the stored refresh token unless one is provided. Tokens are not returned.",
        annotations(title = "Refresh token", open_world_hint = true)
    )]
    async fn appflowy_refresh_token(
        &self,
        Parameters(request): Parameters<RefreshTokenRequest>,
    ) -> Result<Json<Value>, ToolError> {
        // Transactional: a provided token is tried as a candidate and the
        // previously stored token is restored when the refresh fails — a
        // mistyped or expired token must not destroy a still valid one.
        self.client
            .refresh_token_with(request.refresh_token)
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "refreshed" })))
    }
}

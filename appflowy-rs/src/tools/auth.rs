//! Authentication tools (`appflowy_login`, `appflowy_refresh_token`).

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, Json, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::AppServer;
use crate::tools::util::map_tool_error;

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
    /// Refresh token to store before refreshing. Uses the stored one if omitted.
    pub refresh_token: Option<String>,
}

#[tool_router(router = auth_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_login",
        description = "Login to AppFlowy. Tokens are stored in the server; they are not returned to avoid leaking credentials into tool output."
    )]
    async fn appflowy_login(
        &self,
        Parameters(request): Parameters<LoginRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        if let Some(email) = request.email {
            self.client.set_email(Some(email)).await;
        }
        if let Some(password) = request.password {
            self.client.set_password(Some(password)).await;
        }
        if self.client.email().await.is_none() || self.client.password().await.is_none() {
            return Err(ErrorData::invalid_params(
                "Email and password must be provided either in the request or via APPFLOWY_EMAIL and APPFLOWY_PASSWORD env vars",
                None,
            ));
        }
        self.client.login().await.map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "authenticated" })))
    }

    #[tool(
        name = "appflowy_refresh_token",
        description = "Refresh the access token. Uses the stored refresh token unless one is provided. Tokens are not returned."
    )]
    async fn appflowy_refresh_token(
        &self,
        Parameters(request): Parameters<RefreshTokenRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        if let Some(token) = request.refresh_token {
            self.client.set_refresh_token(Some(token)).await;
        }
        self.client.refresh_token().await.map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "refreshed" })))
    }
}

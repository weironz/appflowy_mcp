//! Notification hub tools (comments, mentions, share invites, and other collaboration events).

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::server::AppServer;
use crate::tools::util::map_tool_error;

/// Mirrors AppFlowy-Web `notification-api.ts::getNotifications` query.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ListNotificationsRequest {
    pub workspace_id: String,
    /// Only return unread notifications. Sent to the API only when true.
    pub unread_only: Option<bool>,
    /// Only return archived notifications. Sent to the API only when true.
    pub archived: Option<bool>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct WorkspaceNotificationsRequest {
    pub workspace_id: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct NotificationIdsRequest {
    pub workspace_id: String,
    /// Notification IDs to act on. Must be non-empty.
    pub ids: Vec<String>,
}

fn require_ids(request: &NotificationIdsRequest) -> Result<(), ErrorData> {
    if request.ids.is_empty() {
        return Err(ErrorData::invalid_params(
            "ids must be a non-empty list of notification IDs",
            None,
        ));
    }
    Ok(())
}

#[tool_router(router = notifications_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_list_notifications",
        description = "List notifications from the collaboration notification hub (comments, mentions, share invites, etc.) in a workspace. Optionally filter by unread/archived and paginate with offset/limit."
    )]
    async fn appflowy_list_notifications(
        &self,
        Parameters(request): Parameters<ListNotificationsRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        // The web client only sends unread_only/archived when they are true.
        let mut params: Vec<(&str, String)> = Vec::new();
        if request.unread_only == Some(true) {
            params.push(("unread_only", "true".to_string()));
        }
        if request.archived == Some(true) {
            params.push(("archived", "true".to_string()));
        }
        if let Some(offset) = request.offset {
            params.push(("offset", offset.to_string()));
        }
        if let Some(limit) = request.limit {
            params.push(("limit", limit.to_string()));
        }
        let params = if params.is_empty() {
            None
        } else {
            Some(&params[..])
        };
        let data = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/notifications", request.workspace_id),
                params,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_get_unread_count",
        description = "Get the unread notification count from the collaboration notification hub (comments, mentions, share invites, etc.) in a workspace."
    )]
    async fn appflowy_get_unread_count(
        &self,
        Parameters(request): Parameters<WorkspaceNotificationsRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let data = self
            .client
            .request(
                Method::GET,
                &format!(
                    "/api/workspace/{}/notifications/unread-count",
                    request.workspace_id
                ),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_mark_notifications_read",
        description = "Mark specific collaboration notification hub items (comments, mentions, share invites, etc.) as read. ids must be non-empty."
    )]
    async fn appflowy_mark_notifications_read(
        &self,
        Parameters(request): Parameters<NotificationIdsRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        require_ids(&request)?;
        self.client
            .request(
                Method::POST,
                &format!("/api/workspace/{}/notifications/read", request.workspace_id),
                None,
                Some(json!({ "ids": request.ids })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "marked_read", "ids": request.ids })))
    }

    #[tool(
        name = "appflowy_mark_all_notifications_read",
        description = "Mark all collaboration notification hub items (comments, mentions, share invites, etc.) as read in a workspace."
    )]
    async fn appflowy_mark_all_notifications_read(
        &self,
        Parameters(request): Parameters<WorkspaceNotificationsRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/notifications/read-all",
                    request.workspace_id
                ),
                None,
                Some(json!({})),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "marked_all_read" })))
    }

    #[tool(
        name = "appflowy_archive_notifications",
        description = "Archive specific collaboration notification hub items (comments, mentions, share invites, etc.). ids must be non-empty."
    )]
    async fn appflowy_archive_notifications(
        &self,
        Parameters(request): Parameters<NotificationIdsRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        require_ids(&request)?;
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/notifications/archive",
                    request.workspace_id
                ),
                None,
                Some(json!({ "ids": request.ids })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "archived", "ids": request.ids })))
    }

    #[tool(
        name = "appflowy_archive_all_notifications",
        description = "Archive all collaboration notification hub items (comments, mentions, share invites, etc.) in a workspace."
    )]
    async fn appflowy_archive_all_notifications(
        &self,
        Parameters(request): Parameters<WorkspaceNotificationsRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/notifications/archive-all",
                    request.workspace_id
                ),
                None,
                Some(json!({})),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "archived_all" })))
    }
}

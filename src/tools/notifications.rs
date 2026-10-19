//! Notification hub tools (comments, mentions, share invites, and other collaboration events).

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::server::AppServer;
use crate::tools::util::{map_tool_error, ToolError};

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

fn require_ids(request: &NotificationIdsRequest) -> Result<(), ToolError> {
    if request.ids.is_empty() {
        return Err(ToolError::validation(
            "ids must be a non-empty list of notification IDs",
        ));
    }
    Ok(())
}

/// `appflowy_list_notifications` output (appflowy_web
/// `AFNotificationListResponse`): one page of notifications plus the
/// page-through flag. `notifications` is guaranteed; unlisted keys are
/// allowed.
#[derive(Debug, rmcp::schemars::JsonSchema)]
pub struct ListNotificationsOutput {
    /// The current page of notifications.
    pub notifications: Vec<NotificationOutput>,
    /// True while more pages exist (advance with `offset`).
    pub has_more: Option<bool>,
}

/// One hub notification (comment, mention, share invite, ...). Only `id`
/// is guaranteed; unlisted keys are allowed.
#[derive(Debug, rmcp::schemars::JsonSchema)]
pub struct NotificationOutput {
    /// Notification id.
    pub id: String,
    pub workspace_id: Option<String>,
    /// Notification type, e.g. "comment" / "mention" (server-defined).
    #[schemars(rename = "type")]
    pub ty: Option<String>,
    /// View the notification refers to, when any.
    pub view_id: Option<String>,
    pub actor_uid: Option<i64>,
    /// Type-specific payload (comment text, invite details, ...).
    pub metadata: Option<Value>,
    pub is_read: Option<bool>,
    pub is_archived: Option<bool>,
    pub created_at: Option<String>,
    pub read_at: Option<String>,
}

#[tool_router(router = notifications_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_list_notifications",
        description = "List notifications from the collaboration notification hub (comments, mentions, share invites, etc.) in a workspace. Optionally filter by unread/archived and paginate with offset/limit.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ListNotificationsOutput>(),
        annotations(
            title = "List notifications",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_list_notifications(
        &self,
        Parameters(request): Parameters<ListNotificationsRequest>,
    ) -> Result<Json<Value>, ToolError> {
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
        description = "Get the unread notification count from the collaboration notification hub (comments, mentions, share invites, etc.) in a workspace.",
        annotations(
            title = "Get unread count",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_get_unread_count(
        &self,
        Parameters(request): Parameters<WorkspaceNotificationsRequest>,
    ) -> Result<Json<Value>, ToolError> {
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
        description = "Mark specific collaboration notification hub items (comments, mentions, share invites, etc.) as read. ids must be non-empty.",
        annotations(title = "Mark notifications read", open_world_hint = true)
    )]
    async fn appflowy_mark_notifications_read(
        &self,
        Parameters(request): Parameters<NotificationIdsRequest>,
    ) -> Result<Json<Value>, ToolError> {
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
        description = "Mark all collaboration notification hub items (comments, mentions, share invites, etc.) as read in a workspace.",
        annotations(title = "Mark all notifications read", open_world_hint = true)
    )]
    async fn appflowy_mark_all_notifications_read(
        &self,
        Parameters(request): Parameters<WorkspaceNotificationsRequest>,
    ) -> Result<Json<Value>, ToolError> {
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
        description = "Archive specific collaboration notification hub items (comments, mentions, share invites, etc.). ids must be non-empty.",
        annotations(
            title = "Archive notifications",
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_archive_notifications(
        &self,
        Parameters(request): Parameters<NotificationIdsRequest>,
    ) -> Result<Json<Value>, ToolError> {
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
        description = "Archive all collaboration notification hub items (comments, mentions, share invites, etc.) in a workspace.",
        annotations(
            title = "Archive all notifications",
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_archive_all_notifications(
        &self,
        Parameters(request): Parameters<WorkspaceNotificationsRequest>,
    ) -> Result<Json<Value>, ToolError> {
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

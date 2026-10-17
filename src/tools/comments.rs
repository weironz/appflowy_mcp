//! Document comment tools: inline (page-level) annotations, reactions, and
//! row-comment notifications.
//!
//! These cover the review/annotation comments attached to blocks of a
//! document page (AppFlowy-Web `inline-comment-api.ts`), which are distinct
//! from the public comments on published pages (the `published` tool domain).
//! Row-comment notifications (`row-comment-api.ts`) are grouped here because
//! they belong to the same commenting surface.

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::server::AppServer;
use crate::tools::util::{map_tool_error, ToolError};

fn inline_comment_base(workspace_id: &str, view_id: &str) -> String {
    format!("/api/workspace/{workspace_id}/document/{view_id}/inline-comment")
}

/// Comment-mutating endpoints expect a `device-id` header. The web client
/// sends one persistent id per device; an MCP session has no notion of device
/// state, so each call generates a fresh v4 UUID instead.
fn device_id_header() -> (String, String) {
    ("device-id".to_string(), uuid::Uuid::new_v4().to_string())
}

fn require_non_empty(value: &str, field: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(ToolError::validation(format!("{field} must not be empty")));
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ListInlineCommentsRequest {
    /// Workspace ID.
    pub workspace_id: String,
    /// View (document page) ID.
    pub view_id: String,
    /// Page size (default 100).
    pub limit: Option<u32>,
    /// Offset into the comment list (default 0).
    pub offset: Option<u32>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct AddInlineCommentRequest {
    /// Workspace ID.
    pub workspace_id: String,
    /// View (document page) ID.
    pub view_id: String,
    /// Comment body (required, non-empty).
    pub content: String,
    /// Block ID the comment is anchored to; omit for a page-level comment.
    pub block_id: Option<String>,
    /// Parent comment ID when this is a reply in an existing thread.
    pub reply_comment_id: Option<String>,
    /// UUIDs of users to @-mention in the comment.
    pub mentioned_user_uuids: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ResolveInlineCommentRequest {
    /// Workspace ID.
    pub workspace_id: String,
    /// View (document page) ID.
    pub view_id: String,
    /// Comment (thread) ID to resolve or reopen.
    pub comment_id: String,
    /// true marks the thread resolved; false reopens it.
    pub is_resolved: bool,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct DeleteInlineCommentRequest {
    /// Workspace ID.
    pub workspace_id: String,
    /// View (document page) ID.
    pub view_id: String,
    /// Comment ID to delete.
    pub comment_id: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ListInlineCommentReactionsRequest {
    /// Workspace ID.
    pub workspace_id: String,
    /// View (document page) ID.
    pub view_id: String,
    /// Restrict the result to one comment; omit for all comments of the page.
    pub comment_id: Option<String>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct InlineCommentReactionRequest {
    /// Workspace ID.
    pub workspace_id: String,
    /// View (document page) ID.
    pub view_id: String,
    /// Comment ID to react to.
    pub comment_id: String,
    /// Reaction type, e.g. an emoji string (required, non-empty).
    pub reaction_type: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct NotifyRowCommentRequest {
    /// Workspace ID.
    pub workspace_id: String,
    /// Database ID the row belongs to.
    pub database_id: String,
    /// Row ID the comment was posted on.
    pub row_id: String,
    /// Comment ID to notify about.
    pub comment_id: String,
    /// Comment body, used for the notification text (required, non-empty).
    pub content: String,
    /// Parent comment ID when notifying about a reply.
    pub parent_comment_id: Option<String>,
    /// UUIDs of users mentioned in the comment.
    pub mentioned_user_uuids: Option<Vec<String>>,
    /// UUIDs of existing thread participants to notify.
    pub reply_participant_uuids: Option<Vec<String>>,
    /// Display name of the view, included in the notification.
    pub view_name: Option<String>,
}

#[tool_router(router = comments_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_list_inline_comments",
        description = "List review/annotation comments of a document page (inline comments attached to blocks, as opposed to public comments on published pages). The response contains `comments`, `has_more`, and `next_offset`: while `has_more` is true, call again with `offset` set to the returned `next_offset` to page through the list."
    )]
    async fn appflowy_list_inline_comments(
        &self,
        Parameters(request): Parameters<ListInlineCommentsRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id")?;
        require_non_empty(&request.view_id, "view_id")?;
        let params = vec![
            ("limit", request.limit.unwrap_or(100).to_string()),
            ("offset", request.offset.unwrap_or(0).to_string()),
        ];
        self.client
            .request(
                Method::GET,
                &inline_comment_base(&request.workspace_id, &request.view_id),
                Some(&params),
                None,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_add_inline_comment",
        description = "Add a review/annotation comment to a document page: anchor it to a block via `block_id`, or reply to an existing thread via `reply_comment_id` (distinct from public comments on published pages). Returns `{comment_id}`; on servers without the v2 endpoint it falls back to the legacy endpoint and returns `{status: \"created\"}` instead."
    )]
    async fn appflowy_add_inline_comment(
        &self,
        Parameters(request): Parameters<AddInlineCommentRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id")?;
        require_non_empty(&request.view_id, "view_id")?;
        require_non_empty(&request.content, "content")?;
        let base = inline_comment_base(&request.workspace_id, &request.view_id);
        // The client drops top-level null values, so omitted options disappear.
        let payload = json!({
            "content": request.content,
            "block_id": request.block_id,
            "reply_comment_id": request.reply_comment_id,
            "mentioned_user_uuids": request.mentioned_user_uuids.unwrap_or_default(),
        });
        let (header_name, header_value) = device_id_header();
        let extra_headers: [(&str, &str); 1] = [(&header_name, &header_value)];
        let result = self
            .client
            .request_ext(
                Method::POST,
                &format!("{base}/v2"),
                None,
                Some(payload.clone()),
                &extra_headers,
            )
            .await;
        match result {
            Ok(data) => Ok(Json(data)),
            // Older AppFlowy servers only expose the legacy POST endpoint.
            Err(err) if matches!(err.status(), Some(404 | 405)) => {
                self.client
                    .request_ext(Method::POST, &base, None, Some(payload), &extra_headers)
                    .await
                    .map_err(map_tool_error)?;
                // The legacy endpoint returns no comment_id.
                Ok(Json(json!({ "status": "created" })))
            }
            Err(err) => Err(map_tool_error(err)),
        }
    }

    #[tool(
        name = "appflowy_resolve_inline_comment",
        description = "Mark a document-page review comment (thread) as resolved (is_resolved=true) or reopen it (is_resolved=false). Inline comments are page annotations, not public comments on published pages."
    )]
    async fn appflowy_resolve_inline_comment(
        &self,
        Parameters(request): Parameters<ResolveInlineCommentRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id")?;
        require_non_empty(&request.view_id, "view_id")?;
        require_non_empty(&request.comment_id, "comment_id")?;
        let (header_name, header_value) = device_id_header();
        let extra_headers: [(&str, &str); 1] = [(&header_name, &header_value)];
        self.client
            .request_ext(
                Method::PUT,
                &format!(
                    "{}/resolve",
                    inline_comment_base(&request.workspace_id, &request.view_id)
                ),
                None,
                Some(json!({
                    "comment_id": request.comment_id,
                    "is_resolved": request.is_resolved,
                })),
                &extra_headers,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_delete_inline_comment",
        description = "Delete a review/annotation comment from a document page (inline comment, not a public comment on a published page)."
    )]
    async fn appflowy_delete_inline_comment(
        &self,
        Parameters(request): Parameters<DeleteInlineCommentRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id")?;
        require_non_empty(&request.view_id, "view_id")?;
        require_non_empty(&request.comment_id, "comment_id")?;
        let (header_name, header_value) = device_id_header();
        let extra_headers: [(&str, &str); 1] = [(&header_name, &header_value)];
        self.client
            .request_ext(
                Method::DELETE,
                &inline_comment_base(&request.workspace_id, &request.view_id),
                None,
                Some(json!({ "comment_id": request.comment_id })),
                &extra_headers,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_list_inline_comment_reactions",
        description = "List emoji-style reactions on the review/annotation comments of a document page; pass `comment_id` to restrict to one thread. Returns `{reactions: [{reaction_type, react_users, comment_id}]}`."
    )]
    async fn appflowy_list_inline_comment_reactions(
        &self,
        Parameters(request): Parameters<ListInlineCommentReactionsRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id")?;
        require_non_empty(&request.view_id, "view_id")?;
        let params: Vec<(&str, String)> = request
            .comment_id
            .as_ref()
            .map(|comment_id| vec![("comment_id", comment_id.clone())])
            .unwrap_or_default();
        self.client
            .request(
                Method::GET,
                &format!(
                    "{}/reaction",
                    inline_comment_base(&request.workspace_id, &request.view_id)
                ),
                Some(&params),
                None,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_add_inline_comment_reaction",
        description = "Add an emoji-style reaction to a review/annotation comment on a document page."
    )]
    async fn appflowy_add_inline_comment_reaction(
        &self,
        Parameters(request): Parameters<InlineCommentReactionRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id")?;
        require_non_empty(&request.view_id, "view_id")?;
        require_non_empty(&request.comment_id, "comment_id")?;
        require_non_empty(&request.reaction_type, "reaction_type")?;
        let (header_name, header_value) = device_id_header();
        let extra_headers: [(&str, &str); 1] = [(&header_name, &header_value)];
        self.client
            .request_ext(
                Method::POST,
                &format!(
                    "{}/reaction",
                    inline_comment_base(&request.workspace_id, &request.view_id)
                ),
                None,
                Some(json!({
                    "reaction_type": request.reaction_type,
                    "comment_id": request.comment_id,
                })),
                &extra_headers,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_delete_inline_comment_reaction",
        description = "Remove an emoji-style reaction previously added to a review/annotation comment on a document page."
    )]
    async fn appflowy_delete_inline_comment_reaction(
        &self,
        Parameters(request): Parameters<InlineCommentReactionRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id")?;
        require_non_empty(&request.view_id, "view_id")?;
        require_non_empty(&request.comment_id, "comment_id")?;
        require_non_empty(&request.reaction_type, "reaction_type")?;
        let (header_name, header_value) = device_id_header();
        let extra_headers: [(&str, &str); 1] = [(&header_name, &header_value)];
        self.client
            .request_ext(
                Method::DELETE,
                &format!(
                    "{}/reaction",
                    inline_comment_base(&request.workspace_id, &request.view_id)
                ),
                None,
                Some(json!({
                    "reaction_type": request.reaction_type,
                    "comment_id": request.comment_id,
                })),
                &extra_headers,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_notify_row_comment",
        description = "Send the AppFlowy notification for a comment posted on a database row (mentions, replies, thread participants). Call this after creating the row comment itself so mentioned users and participants get notified."
    )]
    async fn appflowy_notify_row_comment(
        &self,
        Parameters(request): Parameters<NotifyRowCommentRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id")?;
        require_non_empty(&request.database_id, "database_id")?;
        require_non_empty(&request.row_id, "row_id")?;
        require_non_empty(&request.comment_id, "comment_id")?;
        require_non_empty(&request.content, "content")?;
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/database/{}/row/{}/comment/notification",
                    request.workspace_id, request.database_id, request.row_id
                ),
                None,
                // The client drops top-level null values; the two uuid arrays
                // default to empty, exactly like the web client.
                Some(json!({
                    "comment_id": request.comment_id,
                    "content": request.content,
                    "parent_comment_id": request.parent_comment_id,
                    "mentioned_user_uuids": request.mentioned_user_uuids.unwrap_or_default(),
                    "reply_participant_uuids": request.reply_participant_uuids.unwrap_or_default(),
                    "view_name": request.view_name,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "notified" })))
    }
}

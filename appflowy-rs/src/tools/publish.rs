//! Publish tools: published-page duplication, comments/reactions on published
//! views, publishing/unpublishing pages, and the workspace publish namespace.

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, Json, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::AppServer;
use crate::tools::util::{ensure_parent_is_not_workspace, map_tool_error, walk_views};

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct DuplicatePublishedPageParams {
    /// Workspace to duplicate the published page into.
    pub workspace_id: String,
    /// view_id of the published page or template to copy.
    pub published_view_id: String,
    /// Destination parent Space or Page (not the workspace_id).
    pub dest_view_id: String,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct GetPublishedOutlineParams {
    /// Publish namespace of the published workspace.
    pub publish_namespace: String,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct PublishedViewParams {
    /// view_id of the published view.
    pub view_id: String,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct AddPublishedCommentParams {
    /// view_id of the published view.
    pub view_id: String,
    /// Comment text.
    pub content: String,
    /// Comment to reply to, if this is a reply.
    pub reply_comment_id: Option<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct GetPublishedReactionsParams {
    /// view_id of the published view.
    pub view_id: String,
    /// Restrict the listing to reactions on this one comment.
    pub comment_id: Option<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct AddPublishedReactionParams {
    /// view_id of the published view.
    pub view_id: String,
    /// Comment to react to.
    pub comment_id: String,
    /// Emoji / reaction type, e.g. "+1".
    pub reaction_type: String,
}

/// Mirrors Python `models.py::PublishPageRequest`.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct PublishPageRequest {
    /// URL slug for the published page. Server picks one if omitted.
    pub publish_name: Option<String>,
    pub visible_database_view_ids: Option<Vec<String>>,
    pub comments_enabled: Option<bool>,
    pub duplicate_enabled: Option<bool>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct PublishPageParams {
    pub workspace_id: String,
    pub page_id: String,
    #[serde(flatten)]
    pub request: PublishPageRequest,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct WorkspacePageParams {
    pub workspace_id: String,
    pub page_id: String,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct WorkspaceParams {
    pub workspace_id: String,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct SetPublishNamespaceParams {
    pub workspace_id: String,
    /// New publish namespace (subdomain) for the workspace.
    pub new_namespace: String,
}

#[tool_router(router = publish_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_duplicate_published_page",
        description = "Duplicate a published page or template into a workspace. dest_view_id is the destination parent Space or Page (not the workspace_id)."
    )]
    async fn appflowy_duplicate_published_page(
        &self,
        Parameters(params): Parameters<DuplicatePublishedPageParams>,
    ) -> Result<Json<Value>, ErrorData> {
        ensure_parent_is_not_workspace(&params.workspace_id, &params.dest_view_id)
            .map_err(map_tool_error)?;
        let body = self
            .client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/published-duplicate",
                    params.workspace_id
                ),
                None,
                Some(json!({
                    "published_view_id": params.published_view_id,
                    "dest_view_id": params.dest_view_id,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(body))
    }

    #[tool(
        name = "appflowy_get_published_outline",
        description = "Get the full page tree of a published workspace by its publish namespace. Public: no membership in that workspace is required."
    )]
    async fn appflowy_get_published_outline(
        &self,
        Parameters(params): Parameters<GetPublishedOutlineParams>,
    ) -> Result<Json<Value>, ErrorData> {
        // Public endpoint: Python intentionally skips `ensure_authenticated`
        // here. The Rust tools have no per-tool auth mechanism, so issuing
        // the request directly matches that intent.
        let body = self
            .client
            .request(
                Method::GET,
                &format!(
                    "/api/workspace/published-outline/{}",
                    params.publish_namespace
                ),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(body))
    }

    #[tool(
        name = "appflowy_get_published_comments",
        description = "List comments on a published view."
    )]
    async fn appflowy_get_published_comments(
        &self,
        Parameters(params): Parameters<PublishedViewParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let body = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/published-info/{}/comment", params.view_id),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(body))
    }

    #[tool(
        name = "appflowy_add_published_comment",
        description = "Add a comment to a published view, optionally replying to another comment."
    )]
    async fn appflowy_add_published_comment(
        &self,
        Parameters(params): Parameters<AddPublishedCommentParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let body = self
            .client
            .request(
                Method::POST,
                &format!("/api/workspace/published-info/{}/comment", params.view_id),
                None,
                // The client strips the null `reply_comment_id` when absent.
                Some(json!({
                    "content": params.content,
                    "reply_comment_id": params.reply_comment_id,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(body))
    }

    #[tool(
        name = "appflowy_get_published_reactions",
        description = "List reactions on a published view, optionally filtered to one comment."
    )]
    async fn appflowy_get_published_reactions(
        &self,
        Parameters(params): Parameters<GetPublishedReactionsParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let query: Vec<(&str, String)> = params
            .comment_id
            .as_ref()
            .map(|id| vec![("comment_id", id.clone())])
            .unwrap_or_default();
        let body = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/published-info/{}/reaction", params.view_id),
                Some(&query),
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(body))
    }

    #[tool(
        name = "appflowy_add_published_reaction",
        description = "Add an emoji reaction to a comment on a published view."
    )]
    async fn appflowy_add_published_reaction(
        &self,
        Parameters(params): Parameters<AddPublishedReactionParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let body = self
            .client
            .request(
                Method::POST,
                &format!("/api/workspace/published-info/{}/reaction", params.view_id),
                None,
                Some(json!({
                    "reaction_type": params.reaction_type,
                    "comment_id": params.comment_id,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(body))
    }

    #[tool(
        name = "appflowy_publish_page",
        description = "Publish a page to the web so anyone with the link can view it."
    )]
    async fn appflowy_publish_page(
        &self,
        Parameters(params): Parameters<PublishPageParams>,
    ) -> Result<Json<Value>, ErrorData> {
        // Python defaults comments_enabled/duplicate_enabled to true in the
        // request model; honor that when the caller omits them.
        let mut payload = json!({
            "publish_name": params.request.publish_name,
            "visible_database_view_ids": params.request.visible_database_view_ids,
            "comments_enabled": params.request.comments_enabled.unwrap_or(true),
            "duplicate_enabled": params.request.duplicate_enabled.unwrap_or(true),
        });
        if let Value::Object(ref mut map) = payload {
            map.retain(|_, v| !v.is_null());
        }
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/page-view/{}/publish",
                    params.workspace_id, params.page_id
                ),
                None,
                Some(payload),
            )
            .await
            .map_err(map_tool_error)?;
        // Python follows the publish with a GET of the published info and
        // returns that; keep both calls and the return shape.
        let info = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/v1/published-info/{}", params.page_id),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(info))
    }

    #[tool(
        name = "appflowy_unpublish_page",
        description = "Unpublish a page from the web."
    )]
    async fn appflowy_unpublish_page(
        &self,
        Parameters(params): Parameters<WorkspacePageParams>,
    ) -> Result<Json<Value>, ErrorData> {
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/page-view/{}/unpublish",
                    params.workspace_id, params.page_id
                ),
                None,
                Some(json!({})),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "unpublished" })))
    }

    #[tool(
        name = "appflowy_get_publish_namespace",
        description = "Get the workspace publish namespace (the subdomain published pages live under)."
    )]
    async fn appflowy_get_publish_namespace(
        &self,
        Parameters(params): Parameters<WorkspaceParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let body = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/publish-namespace", params.workspace_id),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "namespace": body })))
    }

    #[tool(
        name = "appflowy_set_publish_namespace",
        description = "Rename the workspace publish namespace."
    )]
    async fn appflowy_set_publish_namespace(
        &self,
        Parameters(params): Parameters<SetPublishNamespaceParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let current = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/publish-namespace", params.workspace_id),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        self.client
            .request(
                Method::PUT,
                &format!("/api/workspace/{}/publish-namespace", params.workspace_id),
                None,
                Some(json!({
                    "old_namespace": current,
                    "new_namespace": params.new_namespace,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "namespace": params.new_namespace })))
    }

    #[tool(
        name = "appflowy_list_published_views",
        description = "List all published pages in a workspace."
    )]
    async fn appflowy_list_published_views(
        &self,
        Parameters(params): Parameters<WorkspaceParams>,
    ) -> Result<Json<Value>, ErrorData> {
        match self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/published-info", params.workspace_id),
                None,
                None,
            )
            .await
        {
            Ok(body) => Ok(Json(body)),
            // Older deployments (e.g. beta.appflowy.cloud) lack this endpoint;
            // fall back to walking the folder tree for is_published views.
            // Python matched on `"404" not in str(e)`; branch on the structured
            // HTTP status instead.
            Err(err) if err.status() != Some(404) => Err(map_tool_error(err)),
            Err(_) => self.list_published_views_fallback(&params.workspace_id).await,
        }
    }
}

impl AppServer {
    /// Fallback for [`AppServer::appflowy_list_published_views`]: walk the
    /// folder tree (depth 10) and keep views flagged `is_published`.
    ///
    /// Intentional divergence from Python: the fallback's error message is
    /// prefixed with "folder-tree fallback" so a failure here is
    /// distinguishable from a failure of the primary published-info endpoint
    /// (Python used the identical message for both).
    async fn list_published_views_fallback(
        &self,
        workspace_id: &str,
    ) -> Result<Json<Value>, ErrorData> {
        let query = [("depth", "10".to_string())];
        let root = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/{workspace_id}/folder"),
                Some(&query),
                None,
            )
            .await
            .map_err(|err| {
                map_tool_error(crate::error::Error::Other(format!(
                    "Failed to list published views (folder-tree fallback): {err}"
                )))
            })?;
        let mut published = Vec::new();
        for view in walk_views(&root) {
            if view.get("is_published").and_then(Value::as_bool) == Some(true) {
                published.push(json!({
                    "view_id": view.get("view_id"),
                    "name": view.get("name"),
                    "layout": view.get("layout"),
                    "publish_name": view
                        .get("extra")
                        .filter(|extra| extra.is_object())
                        .and_then(|extra| extra.get("publish_name")),
                }));
            }
        }
        Ok(Json(Value::Array(published)))
    }
}

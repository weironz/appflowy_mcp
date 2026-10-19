//! Workspace tools (workspaces, folder tree, spaces).

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::server::AppServer;
use crate::tools::util::{map_tool_error, walk_views, ToolError};

/// Python truthiness for `view.get("is_space")`: bools and numbers test as
/// themselves, empty strings / null / missing are false, anything else true.
fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn default_space_icon() -> String {
    "interface_essential/home-3".into()
}

fn default_space_icon_color() -> String {
    "0xFFA34AFD".into()
}

fn default_depth() -> i64 {
    2
}

/// pydantic `Field(0, ge=0, le=1)` equivalent, checked before any network I/O.
fn validate_space_permission(space_permission: i64) -> Result<(), ToolError> {
    if !(0..=1).contains(&space_permission) {
        return Err(ToolError::validation(
            "space_permission must be between 0 and 1.",
        ));
    }
    Ok(())
}

/// Space name guard: `Option<String>` alone only blocks `None`, while
/// `Some("")` / whitespace would reach the API (and rename a space to
/// nothing). Same filter + clear-message shape as member.rs's workspace_name.
fn require_space_name(name: Option<String>) -> Result<String, ToolError> {
    name.filter(|name| !name.trim().is_empty())
        .ok_or_else(|| ToolError::validation("name is required and must not be empty."))
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct GetWorkspaceFolderParams {
    /// The workspace whose folder tree to fetch.
    pub workspace_id: String,
    /// Recursion depth of the folder tree. Omitted for the server default.
    pub depth: Option<i64>,
    /// Expand from this space/page view_id instead of the workspace root.
    pub root_view_id: Option<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ListSpacesParams {
    /// The workspace whose spaces to list.
    pub workspace_id: String,
    /// Recursion depth of the folder tree scan.
    #[serde(default = "default_depth")]
    pub depth: i64,
}

impl Default for ListSpacesParams {
    fn default() -> Self {
        Self {
            workspace_id: String::new(),
            depth: default_depth(),
        }
    }
}

/// Mirrors Python `models.py::CreateSpaceRequest` plus `workspace_id`.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct CreateSpaceRequest {
    /// The workspace to create the space in.
    pub workspace_id: String,
    /// Space name (required).
    pub name: Option<String>,
    /// 0 = private, 1 = public.
    pub space_permission: i64,
    #[serde(default = "default_space_icon")]
    pub space_icon: String,
    #[serde(default = "default_space_icon_color")]
    pub space_icon_color: String,
    /// Optional explicit view_id for the new space.
    pub view_id: Option<String>,
}

/// Mirrors Python `models.py::UpdateSpaceRequest` plus ids.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct UpdateSpaceRequest {
    /// The workspace owning the space.
    pub workspace_id: String,
    /// The space to update.
    pub space_id: String,
    /// Space name (required).
    pub name: Option<String>,
    /// 0 = private, 1 = public.
    pub space_permission: i64,
    #[serde(default = "default_space_icon")]
    pub space_icon: String,
    #[serde(default = "default_space_icon_color")]
    pub space_icon_color: String,
}

/// Output element of `appflowy_list_workspaces`: one workspace record as
/// returned by `GET /api/workspace` (AppFlowy-Cloud `AFWorkspace`), passed
/// through verbatim. Only `workspace_id` is guaranteed; unlisted keys are
/// allowed.
#[derive(Debug, rmcp::schemars::JsonSchema)]
pub struct WorkspaceRecordOutput {
    /// Workspace id (uuid).
    pub workspace_id: String,
    pub workspace_name: Option<String>,
    /// 0 = private workspace, 2 = standard.
    pub workspace_type: Option<i64>,
    pub database_storage_id: Option<String>,
    pub owner_uid: Option<i64>,
    pub owner_name: Option<String>,
    pub owner_email: Option<String>,
    pub created_at: Option<String>,
    pub icon: Option<String>,
    pub member_count: Option<i64>,
    /// Role of the requesting user in this workspace (e.g. 20 = Owner).
    pub role: Option<i64>,
}

/// One folder-tree node (AppFlowy-Cloud `FolderView`), the recursive shape
/// of `appflowy_get_workspace_folder`'s tree and of each element of
/// `appflowy_list_spaces`. Only `view_id` is guaranteed; spaces may omit
/// `children` beyond the fetched depth, and unlisted keys are allowed.
#[derive(Debug, rmcp::schemars::JsonSchema)]
pub struct FolderViewOutput {
    /// View id of the folder entry (uuid).
    pub view_id: String,
    pub parent_view_id: Option<String>,
    pub name: Option<String>,
    pub is_space: Option<bool>,
    pub is_private: Option<bool>,
    pub is_published: Option<bool>,
    pub is_favorite: Option<bool>,
    /// 0 = document, 1 = grid, 2 = board, 3 = calendar.
    pub layout: Option<i64>,
    pub created_at: Option<String>,
    pub last_edited_time: Option<String>,
    pub icon: Option<Value>,
    /// Space extras as a JSON object or a JSON-encoded string.
    pub extra: Option<Value>,
    /// Child views, up to the requested/exposed tree depth.
    pub children: Option<Vec<FolderViewOutput>>,
}

#[tool_router(router = workspace_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_list_workspaces",
        description = "List all workspaces for the authenticated user.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<Vec<WorkspaceRecordOutput>>(),
        annotations(
            title = "List workspaces",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_list_workspaces(&self) -> Result<Json<Value>, ToolError> {
        // Python: `client._request("GET", "/api/workspace").get("data", [])`.
        // The client unwraps a success envelope's top-level `data` key, so
        // `Null` here is the no-data case and keeps the empty-array semantics.
        let data = self
            .client
            .request(Method::GET, "/api/workspace", None, None)
            .await
            .map_err(map_tool_error)?;
        let data = match data {
            Value::Null => json!([]),
            other => other,
        };
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_get_workspace_folder",
        description = "Get a workspace folder tree. Use root_view_id to expand a specific space/page and depth to control recursion.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<FolderViewOutput>(),
        annotations(
            title = "Get workspace folder",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_get_workspace_folder(
        &self,
        Parameters(params): Parameters<GetWorkspaceFolderParams>,
    ) -> Result<Json<Value>, ToolError> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(depth) = params.depth {
            query.push(("depth", depth.to_string()));
        }
        // Python: `if root_view_id:` — a truthiness check, so empty is skipped.
        if let Some(root_view_id) = params.root_view_id.as_deref().filter(|s| !s.is_empty()) {
            query.push(("root_view_id", root_view_id.to_string()));
        }
        let data = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/folder", params.workspace_id),
                Some(&query),
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_list_spaces",
        description = "List all spaces in a workspace. Spaces are folder views where is_space is true.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<Vec<FolderViewOutput>>(),
        annotations(
            title = "List spaces",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_list_spaces(
        &self,
        Parameters(params): Parameters<ListSpacesParams>,
    ) -> Result<Json<Value>, ToolError> {
        let root = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/folder", params.workspace_id),
                Some(&[("depth", params.depth.to_string())]),
                None,
            )
            .await
            .map_err(map_tool_error)?;
        let spaces: Vec<Value> = walk_views(&root)
            .into_iter()
            .filter(|view| is_truthy(view.get("is_space")))
            .cloned()
            .collect();
        Ok(Json(Value::Array(spaces)))
    }

    #[tool(
        name = "appflowy_create_space",
        description = "Create a space in a workspace.",
        annotations(title = "Create space", open_world_hint = true)
    )]
    async fn appflowy_create_space(
        &self,
        Parameters(request): Parameters<CreateSpaceRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let name = require_space_name(request.name)?;
        validate_space_permission(request.space_permission)?;
        let data = self
            .client
            .request(
                Method::POST,
                &format!("/api/workspace/{}/space", request.workspace_id),
                None,
                // The client strips top-level null values from the payload,
                // matching pydantic's `model_dump(exclude_none=True)`.
                Some(json!({
                    "name": name,
                    "space_permission": request.space_permission,
                    "space_icon": request.space_icon,
                    "space_icon_color": request.space_icon_color,
                    "view_id": request.view_id,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_update_space",
        description = "Update a space name, icon, color, or permission.",
        annotations(title = "Update space", open_world_hint = true)
    )]
    async fn appflowy_update_space(
        &self,
        Parameters(request): Parameters<UpdateSpaceRequest>,
    ) -> Result<Json<Value>, ToolError> {
        // Same as create: a blank name must not reach the API (it would
        // rename the space to empty whitespace).
        let name = require_space_name(request.name)?;
        validate_space_permission(request.space_permission)?;
        let data = self
            .client
            .request(
                Method::PATCH,
                &format!(
                    "/api/workspace/{}/space/{}",
                    request.workspace_id, request.space_id
                ),
                None,
                Some(json!({
                    "name": name,
                    "space_permission": request.space_permission,
                    "space_icon": request.space_icon,
                    "space_icon_color": request.space_icon_color,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_name_rejects_missing_and_blank() {
        // `ok_or_else` alone only blocked None; Some("") used to reach the
        // API and create/rename a space to nothing.
        let rejected = [None, Some(String::new()), Some("   ".into())];
        for name in rejected {
            assert!(
                require_space_name(name.clone()).is_err(),
                "{name:?} should be rejected"
            );
        }
        let accepted = require_space_name(Some("My Space".into()));
        assert!(matches!(accepted, Ok(name) if name == "My Space"));
    }
}

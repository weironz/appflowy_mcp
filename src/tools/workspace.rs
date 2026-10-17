//! Workspace tools (workspaces, folder tree, spaces).

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::server::AppServer;
use crate::tools::util::{map_tool_error, walk_views};

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
fn validate_space_permission(space_permission: i64) -> Result<(), ErrorData> {
    if !(0..=1).contains(&space_permission) {
        return Err(ErrorData::invalid_params(
            "space_permission must be between 0 and 1.",
            None,
        ));
    }
    Ok(())
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

#[tool_router(router = workspace_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_list_workspaces",
        description = "List all workspaces for the authenticated user."
    )]
    async fn appflowy_list_workspaces(&self) -> Result<Json<Value>, ErrorData> {
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
        description = "Get a workspace folder tree. Use root_view_id to expand a specific space/page and depth to control recursion."
    )]
    async fn appflowy_get_workspace_folder(
        &self,
        Parameters(params): Parameters<GetWorkspaceFolderParams>,
    ) -> Result<Json<Value>, ErrorData> {
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
        description = "List all spaces in a workspace. Spaces are folder views where is_space is true."
    )]
    async fn appflowy_list_spaces(
        &self,
        Parameters(params): Parameters<ListSpacesParams>,
    ) -> Result<Json<Value>, ErrorData> {
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
        description = "Create a space in a workspace."
    )]
    async fn appflowy_create_space(
        &self,
        Parameters(request): Parameters<CreateSpaceRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let name = request
            .name
            .ok_or_else(|| ErrorData::invalid_params("name is required.", None))?;
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
        description = "Update a space name, icon, color, or permission."
    )]
    async fn appflowy_update_space(
        &self,
        Parameters(request): Parameters<UpdateSpaceRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let name = request
            .name
            .ok_or_else(|| ErrorData::invalid_params("name is required.", None))?;
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

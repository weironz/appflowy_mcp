//! Sharing and access-management tools: sharing pages by email, revoking
//! access, access details, access requests, and listing pages shared with the
//! current user.

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, Json, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::AppServer;
use crate::tools::util::map_tool_error;

/// Access levels exactly as the AppFlowy-Web frontend defines them in
/// `src/application/types.ts`: `ReadOnly = 10`, `ReadAndComment = 20`,
/// `ReadAndWrite = 30`, `FullAccess = 50`.
const ACCESS_LEVELS: [i64; 4] = [10, 20, 30, 50];
const DEFAULT_ACCESS_LEVEL: i64 = 10;

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct SharePageParams {
    /// Workspace that owns the page.
    pub workspace_id: Option<String>,
    /// view_id of the page to share.
    pub view_id: Option<String>,
    /// Emails of the people to grant access to. Must not be empty.
    pub emails: Vec<String>,
    /// AccessLevel as an integer: 10 = ReadOnly (default), 20 = ReadAndComment,
    /// 30 = ReadAndWrite, 50 = FullAccess.
    pub access_level: Option<i64>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct RevokePageAccessParams {
    /// Workspace that owns the page.
    pub workspace_id: Option<String>,
    /// view_id of the page to revoke access from.
    pub view_id: Option<String>,
    /// Emails of the people whose access is revoked. Must not be empty.
    pub emails: Vec<String>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct PageAccessDetailsParams {
    /// Workspace that owns the page.
    pub workspace_id: Option<String>,
    /// view_id of the page whose access details are requested.
    pub view_id: Option<String>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct RequestAccessParams {
    /// Workspace that owns the page.
    pub workspace_id: Option<String>,
    /// view_id of the page to request access to.
    pub view_id: Option<String>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct AccessRequestParams {
    /// ID of the access request.
    pub request_id: Option<String>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct SharedWithMeParams {
    /// Workspace to list shared pages for.
    pub workspace_id: Option<String>,
}

fn required(value: Option<String>, field: &str) -> Result<String, ErrorData> {
    match value {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(ErrorData::invalid_params(
            format!("`{field}` is required and must not be empty"),
            None,
        )),
    }
}

/// DFS the folder tree for the path of view ids from the root down to the
/// direct parent of `target` (root first, `target` itself excluded). Returns
/// `None` when the target is not in the tree; the root itself yields `Some([])`.
fn find_view_ancestors<'a>(
    node: &'a Value,
    target: &str,
    trail: &mut Vec<&'a str>,
) -> Option<Vec<String>> {
    let id = node.get("view_id").and_then(Value::as_str);
    if id == Some(target) {
        return Some(trail.iter().map(|s| (*s).to_string()).collect());
    }
    let mut found = None;
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        if let Some(id) = id {
            trail.push(id);
        }
        for child in children {
            if let Some(path) = find_view_ancestors(child, target, trail) {
                found = Some(path);
                break;
            }
        }
        if id.is_some() {
            trail.pop();
        }
    }
    found
}

#[tool_router(router = sharing_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_share_page",
        description = "Share a page with specific people by email, granting them access to it inside the workspace. This differs from publishing a public link (use the publish tools for that). access_level uses the AppFlowy AccessLevel ints (AppFlowy-Web types.ts): 10 = ReadOnly (default), 20 = ReadAndComment, 30 = ReadAndWrite, 50 = FullAccess."
    )]
    async fn appflowy_share_page(
        &self,
        Parameters(params): Parameters<SharePageParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let workspace_id = required(params.workspace_id, "workspace_id")?;
        let view_id = required(params.view_id, "view_id")?;
        if params.emails.is_empty() {
            return Err(ErrorData::invalid_params(
                "`emails` must contain at least one email address",
                None,
            ));
        }
        let access_level = params.access_level.unwrap_or(DEFAULT_ACCESS_LEVEL);
        if !ACCESS_LEVELS.contains(&access_level) {
            return Err(ErrorData::invalid_params(
                "`access_level` must be one of 10 (ReadOnly), 20 (ReadAndComment), 30 (ReadAndWrite), 50 (FullAccess)",
                None,
            ));
        }
        self.client
            .request(
                Method::PUT,
                &format!("/api/sharing/workspace/{workspace_id}/view"),
                None,
                Some(json!({
                    "view_id": view_id,
                    "emails": params.emails,
                    "access_level": access_level,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({
            "status": "shared",
            "view_id": view_id,
            "emails": params.emails,
            "access_level": access_level,
        })))
    }

    #[tool(
        name = "appflowy_revoke_page_access",
        description = "Revoke access to a page for specific people by email."
    )]
    async fn appflowy_revoke_page_access(
        &self,
        Parameters(params): Parameters<RevokePageAccessParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let workspace_id = required(params.workspace_id, "workspace_id")?;
        let view_id = required(params.view_id, "view_id")?;
        if params.emails.is_empty() {
            return Err(ErrorData::invalid_params(
                "`emails` must contain at least one email address",
                None,
            ));
        }
        self.client
            .request(
                Method::POST,
                &format!("/api/sharing/workspace/{workspace_id}/view/{view_id}/revoke-access"),
                None,
                Some(json!({ "emails": params.emails })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({
            "status": "revoked",
            "emails": params.emails,
        })))
    }

    #[tool(
        name = "appflowy_get_page_access_details",
        description = "Get who has access to a page (people, groups, and the current user's permission). Tries the v2 endpoint first and falls back to the legacy access-details endpoint on HTTP 404/405, computing the page's ancestor view chain from the workspace folder tree. The API response is passed through."
    )]
    async fn appflowy_get_page_access_details(
        &self,
        Parameters(params): Parameters<PageAccessDetailsParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let workspace_id = required(params.workspace_id, "workspace_id")?;
        let view_id = required(params.view_id, "view_id")?;
        match self
            .client
            .request(
                Method::GET,
                &format!("/api/sharing/workspace/{workspace_id}/access-details/v2"),
                Some(&[("type", "page".to_string()), ("page_id", view_id.clone())]),
                None,
            )
            .await
        {
            Ok(body) => Ok(Json(body)),
            // Older deployments lack the v2 endpoint; the legacy one needs the
            // page's ancestor view ids, walked out of the folder tree.
            Err(err) if matches!(err.status(), Some(404) | Some(405)) => {
                let tree = self
                    .client
                    .request(
                        Method::GET,
                        &format!("/api/workspace/{workspace_id}/folder"),
                        Some(&[("depth", "10".to_string())]),
                        None,
                    )
                    .await
                    .map_err(map_tool_error)?;
                let mut trail = Vec::new();
                let ancestor_view_ids =
                    find_view_ancestors(&tree, &view_id, &mut trail).ok_or_else(|| {
                        ErrorData::internal_error(
                            format!("view '{view_id}' was not found in the workspace folder tree; cannot build ancestor_view_ids for the legacy access-details endpoint"),
                            None,
                        )
                    })?;
                let body = self
                    .client
                    .request(
                        Method::POST,
                        &format!(
                            "/api/sharing/workspace/{workspace_id}/view/{view_id}/access-details"
                        ),
                        None,
                        Some(json!({ "ancestor_view_ids": ancestor_view_ids })),
                    )
                    .await
                    .map_err(map_tool_error)?;
                Ok(Json(body))
            }
            Err(err) => Err(map_tool_error(err)),
        }
    }

    #[tool(
        name = "appflowy_request_access",
        description = "Request access to a page in a workspace (asks the page's owners for permission)."
    )]
    async fn appflowy_request_access(
        &self,
        Parameters(params): Parameters<RequestAccessParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let workspace_id = required(params.workspace_id, "workspace_id")?;
        let view_id = required(params.view_id, "view_id")?;
        self.client
            .request(
                Method::POST,
                "/api/access-request",
                None,
                Some(json!({
                    "workspace_id": workspace_id,
                    "view_id": view_id,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "requested" })))
    }

    #[tool(
        name = "appflowy_get_access_request",
        description = "Get the details/status of an access request. The API response is passed through."
    )]
    async fn appflowy_get_access_request(
        &self,
        Parameters(params): Parameters<AccessRequestParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let request_id = required(params.request_id, "request_id")?;
        let body = self
            .client
            .request(
                Method::GET,
                &format!("/api/access-request/{request_id}"),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(body))
    }

    #[tool(
        name = "appflowy_approve_access_request",
        description = "Approve a pending access request, granting the requester access to the page."
    )]
    async fn appflowy_approve_access_request(
        &self,
        Parameters(params): Parameters<AccessRequestParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let request_id = required(params.request_id, "request_id")?;
        self.client
            .request(
                Method::POST,
                &format!("/api/access-request/{request_id}/approve"),
                None,
                Some(json!({ "is_approved": true })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({
            "status": "approved",
            "request_id": request_id,
        })))
    }

    #[tool(
        name = "appflowy_list_shared_with_me",
        description = "List pages shared with the current user in a workspace (the 'Shared with me' section). The API response is passed through."
    )]
    async fn appflowy_list_shared_with_me(
        &self,
        Parameters(params): Parameters<SharedWithMeParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let workspace_id = required(params.workspace_id, "workspace_id")?;
        // Upstream (AppFlowy-Web access-api.ts) really does use the
        // workspace_id for both path segments here; not a typo.
        let body = self
            .client
            .request(
                Method::GET,
                &format!("/api/sharing/workspace/{workspace_id}/view/{workspace_id}"),
                Some(&[("depth", "50".to_string())]),
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(body))
    }
}

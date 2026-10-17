//! Workspace lifecycle, member, and invitation tools.

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::server::AppServer;
use crate::tools::util::map_tool_error;

/// `WorkspaceRole = Literal["Owner", "Member", "Guest"]` from Python
/// `models.py` as a closed enum: serde rejects any other value during
/// parameter deserialization, so the network is never hit with a bad role.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, rmcp::schemars::JsonSchema)]
pub enum WorkspaceRole {
    Owner,
    Member,
    Guest,
}

fn default_role() -> WorkspaceRole {
    WorkspaceRole::Member
}

/// Mirrors Python `models.py::CreateWorkspaceRequest`.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct CreateWorkspaceRequest {
    /// Name of the new workspace (required, must not be empty).
    pub workspace_name: Option<String>,
    pub workspace_icon: Option<String>,
}

/// Mirrors Python `models.py::UpdateWorkspaceRequest` plus `workspace_id`.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct UpdateWorkspaceRequest {
    /// The workspace to update.
    pub workspace_id: String,
    pub workspace_name: Option<String>,
    pub workspace_icon: Option<String>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct WorkspaceIdParams {
    /// The workspace this call applies to.
    pub workspace_id: String,
}

/// Mirrors Python `models.py::UpdateWorkspaceSettingsRequest` plus
/// `workspace_id`.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct UpdateWorkspaceSettingsRequest {
    /// The workspace whose settings to update.
    pub workspace_id: String,
    pub disable_search_indexing: Option<bool>,
    pub ai_model: Option<String>,
}

/// Mirrors Python `models.py::UpdateMemberRequest` plus `workspace_id`.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct UpdateMemberRequest {
    /// The workspace the member belongs to.
    pub workspace_id: String,
    /// Email of the member to update (required, must not be empty).
    pub email: Option<String>,
    /// New role for the member.
    pub role: Option<WorkspaceRole>,
    /// New display name for the member.
    pub name: Option<String>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct RemoveMembersParams {
    /// The workspace to remove members from.
    pub workspace_id: String,
    /// Emails of the members to remove (at least one).
    pub emails: Vec<String>,
}

/// Mirrors Python `models.py::InviteMembersRequest` plus `workspace_id`.
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct InviteMembersRequest {
    /// The workspace to invite into.
    pub workspace_id: String,
    /// Emails to invite (at least one).
    pub emails: Vec<String>,
    #[serde(default = "default_role")]
    pub role: WorkspaceRole,
    pub skip_email_send: bool,
}

impl Default for InviteMembersRequest {
    fn default() -> Self {
        Self {
            workspace_id: String::new(),
            emails: Vec::new(),
            role: default_role(),
            skip_email_send: false,
        }
    }
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ListInvitationsParams {
    /// Filter by invitation status (Pending, Accepted, Rejected).
    pub status: Option<String>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct AcceptInvitationParams {
    /// The invitation to accept.
    pub invite_id: String,
}

#[tool_router(router = member_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_create_workspace",
        description = "Create a new workspace."
    )]
    async fn appflowy_create_workspace(
        &self,
        Parameters(request): Parameters<CreateWorkspaceRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let workspace_name = request
            .workspace_name
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                ErrorData::invalid_params("workspace_name is required and must not be empty.", None)
            })?;
        let data = self
            .client
            .request(
                Method::POST,
                "/api/workspace",
                None,
                Some(json!({
                    "workspace_name": workspace_name,
                    "workspace_icon": request.workspace_icon,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_update_workspace",
        description = "Update a workspace name and/or icon."
    )]
    async fn appflowy_update_workspace(
        &self,
        Parameters(request): Parameters<UpdateWorkspaceRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        // Python quirk: workspace_id travels in the JSON body, not the URL
        // (unlike appflowy_delete_workspace, which uses a path parameter).
        self.client
            .request(
                Method::PATCH,
                "/api/workspace",
                None,
                // The client strips top-level null values from the payload,
                // matching pydantic's `model_dump(exclude_none=True)`.
                Some(json!({
                    "workspace_id": request.workspace_id,
                    "workspace_name": request.workspace_name,
                    "workspace_icon": request.workspace_icon,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "updated" })))
    }

    #[tool(
        name = "appflowy_delete_workspace",
        description = "Permanently delete a workspace and everything in it. Irreversible."
    )]
    async fn appflowy_delete_workspace(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Result<Json<Value>, ErrorData> {
        self.client
            .request(
                Method::DELETE,
                &format!("/api/workspace/{}", params.workspace_id),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "deleted" })))
    }

    #[tool(
        name = "appflowy_leave_workspace",
        description = "Leave a workspace you are a member of."
    )]
    async fn appflowy_leave_workspace(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Result<Json<Value>, ErrorData> {
        // This endpoint expects a literal JSON null body.
        self.client
            .request_content(
                Method::POST,
                &format!("/api/workspace/{}/leave", params.workspace_id),
                b"null",
                "application/json",
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "left" })))
    }

    #[tool(
        name = "appflowy_get_workspace_settings",
        description = "Get workspace settings (search indexing, AI model)."
    )]
    async fn appflowy_get_workspace_settings(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let data = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/settings", params.workspace_id),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_update_workspace_settings",
        description = "Update workspace settings: disable_search_indexing and/or ai_model."
    )]
    async fn appflowy_update_workspace_settings(
        &self,
        Parameters(request): Parameters<UpdateWorkspaceSettingsRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let data = self
            .client
            .request(
                Method::POST,
                &format!("/api/workspace/{}/settings", request.workspace_id),
                None,
                Some(json!({
                    "disable_search_indexing": request.disable_search_indexing,
                    "ai_model": request.ai_model,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_get_workspace_usage",
        description = "Get workspace storage usage (consumed capacity in bytes)."
    )]
    async fn appflowy_get_workspace_usage(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let data = self
            .client
            .request(
                Method::GET,
                &format!("/api/file_storage/{}/usage", params.workspace_id),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_list_members",
        description = "List members of a workspace with their roles."
    )]
    async fn appflowy_list_members(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let data = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/member", params.workspace_id),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_update_member",
        description = "Update a workspace member's role (Owner, Member, Guest) or name."
    )]
    async fn appflowy_update_member(
        &self,
        Parameters(request): Parameters<UpdateMemberRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let email = request
            .email
            .filter(|email| !email.is_empty())
            .ok_or_else(|| {
                ErrorData::invalid_params("email is required and must not be empty.", None)
            })?;
        self.client
            .request(
                Method::PUT,
                &format!("/api/workspace/{}/member", request.workspace_id),
                None,
                Some(json!({
                    "email": email,
                    "role": request.role,
                    "name": request.name,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "updated" })))
    }

    #[tool(
        name = "appflowy_remove_members",
        description = "Remove members from a workspace by email."
    )]
    async fn appflowy_remove_members(
        &self,
        Parameters(params): Parameters<RemoveMembersParams>,
    ) -> Result<Json<Value>, ErrorData> {
        // Python raises this outside the try block; it is a caller bug, so
        // invalid_params rather than an internal error.
        if params.emails.is_empty() {
            return Err(ErrorData::invalid_params(
                "At least one email is required.",
                None,
            ));
        }
        // The API takes a top-level JSON array body; the client passes
        // non-object bodies through untouched.
        self.client
            .request(
                Method::DELETE,
                &format!("/api/workspace/{}/member", params.workspace_id),
                None,
                Some(Value::Array(
                    params.emails.iter().map(|email| json!(email)).collect(),
                )),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(
            json!({ "status": "removed", "emails": params.emails }),
        ))
    }

    #[tool(
        name = "appflowy_invite_members",
        description = "Invite people to a workspace by email with a role (Owner, Member, Guest)."
    )]
    async fn appflowy_invite_members(
        &self,
        Parameters(request): Parameters<InviteMembersRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        if request.emails.is_empty() {
            return Err(ErrorData::invalid_params(
                "emails must contain at least one item.",
                None,
            ));
        }
        let invitations: Vec<Value> = request
            .emails
            .iter()
            .map(|email| {
                json!({
                    "email": email,
                    "role": request.role,
                    "skip_email_send": request.skip_email_send,
                })
            })
            .collect();
        self.client
            .request(
                Method::POST,
                &format!("/api/workspace/{}/invite", request.workspace_id),
                None,
                Some(Value::Array(invitations)),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({
            "status": "invited",
            "emails": request.emails,
            "role": request.role,
        })))
    }

    #[tool(
        name = "appflowy_list_invitations",
        description = "List workspace invitations sent to the authenticated user, optionally filtered by status (Pending, Accepted, Rejected)."
    )]
    async fn appflowy_list_invitations(
        &self,
        Parameters(params): Parameters<ListInvitationsParams>,
    ) -> Result<Json<Value>, ErrorData> {
        // Python: `params = {"status": status} if status else None`.
        let query: Vec<(&str, String)> = params
            .status
            .as_deref()
            .filter(|status| !status.is_empty())
            .map(|status| vec![("status", status.to_string())])
            .unwrap_or_default();
        let data = self
            .client
            .request(Method::GET, "/api/workspace/invite", Some(&query), None)
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_accept_invitation",
        description = "Accept a workspace invitation by invite_id."
    )]
    async fn appflowy_accept_invitation(
        &self,
        Parameters(params): Parameters<AcceptInvitationParams>,
    ) -> Result<Json<Value>, ErrorData> {
        // This endpoint expects a literal JSON null body.
        self.client
            .request_content(
                Method::POST,
                &format!("/api/workspace/accept-invite/{}", params.invite_id),
                b"null",
                "application/json",
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "accepted" })))
    }

    #[tool(
        name = "appflowy_get_user_profile",
        description = "Get the authenticated user's profile (uid, email, name, latest workspace)."
    )]
    async fn appflowy_get_user_profile(&self) -> Result<Json<Value>, ErrorData> {
        let data = self
            .client
            .request(Method::GET, "/api/user/profile", None, None)
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }
}

//! AI chat tools.

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::server::AppServer;
use crate::tools::util::{map_tool_error, ToolError};

/// Mirrors Python `models.py::CreateChatRequest` (workspace_id flattened in).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct CreateChatRequest {
    pub workspace_id: String,
    /// Chat display name (min length 1).
    pub name: String,
    /// View ids of pages the AI may use as RAG context. Empty list means no page context.
    #[serde(default)]
    pub rag_ids: Vec<String>,
    /// Chat UUID. Generated automatically if omitted.
    pub chat_id: Option<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ChatIdRequest {
    pub workspace_id: String,
    pub chat_id: String,
}

/// Mirrors Python `models.py::UpdateChatSettingsRequest` (path ids flattened in).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct UpdateChatSettingsRequest {
    pub workspace_id: String,
    pub chat_id: String,
    pub name: Option<String>,
    /// Replace the set of RAG context view ids.
    pub rag_ids: Option<Vec<String>>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ListChatMessagesRequest {
    pub workspace_id: String,
    pub chat_id: String,
    /// Page size cap (default 20).
    pub limit: Option<i64>,
    /// Page backwards: only messages before this id are returned.
    pub before_message_id: Option<i64>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ChatAskRequest {
    pub workspace_id: String,
    pub chat_id: String,
    pub question: String,
}

#[tool_router(router = chat_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_create_chat",
        description = "Create an AI chat in a workspace. Optionally pass rag_ids (page view ids) the AI should use as context. Returns the chat_id for follow-up calls.",
        annotations(title = "Create chat", open_world_hint = true)
    )]
    async fn appflowy_create_chat(
        &self,
        Parameters(request): Parameters<CreateChatRequest>,
    ) -> Result<Json<Value>, ToolError> {
        if request.name.is_empty() {
            return Err(ToolError::validation("Chat name must not be empty."));
        }
        // Python: `request.chat_id or str(uuid.uuid4())` — an empty string
        // chat_id also falls back to a generated uuid.
        let chat_id = request
            .chat_id
            .filter(|chat_id| !chat_id.is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        self.client
            .request(
                Method::POST,
                &format!("/api/chat/{}", request.workspace_id),
                None,
                Some(json!({
                    "chat_id": chat_id,
                    "name": request.name,
                    "rag_ids": request.rag_ids,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({
            "chat_id": chat_id,
            "name": request.name,
            "rag_ids": request.rag_ids,
        })))
    }

    #[tool(
        name = "appflowy_delete_chat",
        description = "Delete an AI chat.",
        annotations(title = "Delete chat", destructive_hint = true, open_world_hint = true)
    )]
    async fn appflowy_delete_chat(
        &self,
        Parameters(request): Parameters<ChatIdRequest>,
    ) -> Result<Json<Value>, ToolError> {
        self.client
            .request(
                Method::DELETE,
                &format!("/api/chat/{}/{}", request.workspace_id, request.chat_id),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "deleted": true, "chat_id": request.chat_id })))
    }

    #[tool(
        name = "appflowy_get_chat_settings",
        description = "Get an AI chat's settings (name, rag_ids, metadata).",
        annotations(
            title = "Get chat settings",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_get_chat_settings(
        &self,
        Parameters(request): Parameters<ChatIdRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let data = self
            .client
            .request(
                Method::GET,
                &format!(
                    "/api/chat/{}/{}/settings",
                    request.workspace_id, request.chat_id
                ),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_update_chat_settings",
        description = "Update an AI chat's name, rag_ids, or metadata.",
        annotations(title = "Update chat settings", open_world_hint = true)
    )]
    async fn appflowy_update_chat_settings(
        &self,
        Parameters(request): Parameters<UpdateChatSettingsRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let settings_path = format!(
            "/api/chat/{}/{}/settings",
            request.workspace_id, request.chat_id
        );
        // The client drops top-level nulls from object bodies, which is
        // exactly Python's `request.model_dump(exclude_none=True)`.
        self.client
            .request(
                Method::POST,
                &settings_path,
                None,
                Some(json!({
                    "name": request.name,
                    "rag_ids": request.rag_ids,
                    "metadata": request.metadata,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        let data = self
            .client
            .request(Method::GET, &settings_path, None, None)
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_list_chat_messages",
        description = "List messages in an AI chat, newest first. Use limit to bound the page size and before_message_id to page backwards through history. Returns personal data (names/emails); share within workspace permissions only.",
        annotations(
            title = "List chat messages",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_list_chat_messages(
        &self,
        Parameters(request): Parameters<ListChatMessagesRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let mut params: Vec<(&str, String)> =
            vec![("limit", request.limit.unwrap_or(20).to_string())];
        if let Some(before_message_id) = request.before_message_id {
            params.push(("before", before_message_id.to_string()));
        }
        let data = self
            .client
            .request(
                Method::GET,
                &format!(
                    "/api/chat/{}/{}/message",
                    request.workspace_id, request.chat_id
                ),
                Some(&params),
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_chat_ask",
        description = "Ask the workspace AI a question in an existing chat and wait for the answer (non-streaming). Returns both the question message id and the AI answer. Create a chat first with appflowy_create_chat.",
        annotations(title = "Chat ask", open_world_hint = true)
    )]
    async fn appflowy_chat_ask(
        &self,
        Parameters(request): Parameters<ChatAskRequest>,
    ) -> Result<Json<Value>, ToolError> {
        // message_type is a u8 enum: 0=System, 1=User.
        let question_message = self
            .client
            .request(
                Method::POST,
                &format!(
                    "/api/chat/{}/{}/message/question",
                    request.workspace_id, request.chat_id
                ),
                None,
                Some(json!({ "content": request.question, "message_type": 1 })),
            )
            .await
            .map_err(map_tool_error)?;
        let question_id = question_message
            .get("message_id")
            .cloned()
            .filter(|message_id| !message_id.is_null())
            .ok_or_else(|| ToolError::internal("Question response did not include message_id."))?;
        // AppFlowy message ids are integers; keep strings verbatim and render
        // anything else without quotes for the path segment.
        let question_id_segment = match &question_id {
            Value::String(id) => id.clone(),
            other => other.to_string(),
        };
        let answer = self
            .client
            .request(
                Method::GET,
                &format!(
                    "/api/chat/{}/{}/{question_id_segment}/answer",
                    request.workspace_id, request.chat_id
                ),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({
            "question_message_id": question_id,
            "answer_message_id": answer.get("message_id"),
            "answer": answer.get("content"),
        })))
    }
}

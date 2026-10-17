//! Quick note tools.

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::server::AppServer;
use crate::tools::util::{map_tool_error, ToolError};

/// Faithful port of Python `server.py::quick_note_data`: raw `data` wins over
/// `text`; text becomes one `paragraph` block per `splitlines()` line. An
/// empty string still yields a single empty paragraph, matching Python's
/// `[""]` fallback for empty text.
fn quick_note_data(text: Option<&str>, data: Option<&Value>) -> Option<Value> {
    if let Some(data) = data {
        return Some(data.clone());
    }
    let text = text?;
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        lines.push("");
    }
    Some(Value::Array(
        lines
            .into_iter()
            .map(|line| json!({"type": "paragraph", "delta": [{"insert": line}]}))
            .collect(),
    ))
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ListQuickNotesRequest {
    pub workspace_id: String,
    /// Filter notes by a search term.
    pub search_term: Option<String>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

/// Mirrors Python `models.py::CreateQuickNoteRequest` (workspace_id flattened in).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct CreateQuickNoteRequest {
    pub workspace_id: String,
    /// Plain text convenience; converted to paragraph blocks.
    pub text: Option<String>,
    /// Raw quick-note JSON data. Takes precedence over text.
    pub data: Option<Value>,
}

/// Mirrors Python `models.py::UpdateQuickNoteRequest` (path ids flattened in).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct UpdateQuickNoteRequest {
    pub workspace_id: String,
    pub quick_note_id: String,
    /// Plain text convenience; converted to paragraph blocks.
    pub text: Option<String>,
    /// Raw quick-note JSON data. Takes precedence over text.
    pub data: Option<Value>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct DeleteQuickNoteRequest {
    pub workspace_id: String,
    pub quick_note_id: String,
}

#[tool_router(router = quicknotes_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_list_quick_notes",
        description = "List quick notes in a workspace, with optional search and pagination."
    )]
    async fn appflowy_list_quick_notes(
        &self,
        Parameters(request): Parameters<ListQuickNotesRequest>,
    ) -> Result<Json<Value>, ToolError> {
        // Python only sends params whose value is not None.
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(search_term) = &request.search_term {
            params.push(("search_term", search_term.clone()));
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
                &format!("/api/workspace/{}/quick-note", request.workspace_id),
                params,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_create_quick_note",
        description = "Create a quick note from plain text (or raw block data)."
    )]
    async fn appflowy_create_quick_note(
        &self,
        Parameters(request): Parameters<CreateQuickNoteRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let data = self
            .client
            .request(
                Method::POST,
                &format!("/api/workspace/{}/quick-note", request.workspace_id),
                None,
                Some(json!({
                    "data": quick_note_data(request.text.as_deref(), request.data.as_ref()),
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_update_quick_note",
        description = "Replace a quick note's content with plain text (or raw block data)."
    )]
    async fn appflowy_update_quick_note(
        &self,
        Parameters(request): Parameters<UpdateQuickNoteRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let data = quick_note_data(request.text.as_deref(), request.data.as_ref())
            .ok_or_else(|| ToolError::validation("Either text or data is required."))?;
        self.client
            .request(
                Method::PUT,
                &format!(
                    "/api/workspace/{}/quick-note/{}",
                    request.workspace_id, request.quick_note_id
                ),
                None,
                Some(json!({"data": data})),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "updated" })))
    }

    #[tool(
        name = "appflowy_delete_quick_note",
        description = "Delete a quick note."
    )]
    async fn appflowy_delete_quick_note(
        &self,
        Parameters(request): Parameters<DeleteQuickNoteRequest>,
    ) -> Result<Json<Value>, ToolError> {
        self.client
            .request(
                Method::DELETE,
                &format!(
                    "/api/workspace/{}/quick-note/{}",
                    request.workspace_id, request.quick_note_id
                ),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "deleted" })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_takes_precedence_over_text() {
        let data = json!([{"type": "todo"}]);
        let result = quick_note_data(Some("hello"), Some(&data)).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn text_becomes_paragraph_blocks() {
        let result = quick_note_data(Some("a\nb\n"), None).unwrap();
        assert_eq!(
            result,
            json!([
                {"type": "paragraph", "delta": [{"insert": "a"}]},
                {"type": "paragraph", "delta": [{"insert": "b"}]},
            ])
        );
    }

    #[test]
    fn empty_text_yields_one_empty_paragraph() {
        let result = quick_note_data(Some(""), None).unwrap();
        assert_eq!(
            result,
            json!([{"type": "paragraph", "delta": [{"insert": ""}]}])
        );
    }

    #[test]
    fn neither_text_nor_data_is_none() {
        assert!(quick_note_data(None, None).is_none());
    }
}

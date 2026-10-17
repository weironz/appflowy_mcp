//! Page content & import tools.
//!
//! Ports of `server.py` L672-885. Unlike Python (which takes `workspace_id`
//! and `page_id` as separate tool arguments), rmcp tools accept a single
//! parameters object, so those fields are flattened into each request struct.

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::importer::MarkdownImporter;
use crate::markdown::{parse_content_to_blocks, parse_markdown_to_blocks};
use crate::server::AppServer;
use crate::tools::util::{
    create_page_with_blocks, ensure_parent_is_not_workspace, map_tool_error, ToolError,
};

/// Python `Field(..., min_length=1)` on strings, checked after trimming.
fn require_non_empty(value: &str, field: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(ToolError::validation(format!("{field} must not be empty")));
    }
    Ok(())
}

/// Python `Field(0, ge=0, le=4)` on layout.
fn validate_layout(layout: i64) -> Result<(), ToolError> {
    if !(0..=4).contains(&layout) {
        return Err(ToolError::validation(format!(
            "layout must be between 0 and 4, got {layout}"
        )));
    }
    Ok(())
}

fn default_block_type() -> String {
    "paragraph".into()
}

fn default_content_format() -> String {
    "markdown".into()
}

fn default_upload_assets() -> bool {
    true
}

/// Mirrors Python `AppendBlocksRequest` (plus the tool-level ids).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct AppendBlocksRequest {
    pub workspace_id: String,
    pub page_id: String,
    /// Raw AppFlowy document blocks to append.
    pub blocks: Vec<Value>,
}

/// Mirrors Python `AppendTextRequest` (plus the tool-level ids).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct AppendTextRequest {
    pub workspace_id: String,
    pub page_id: String,
    /// Plain text strings; each becomes one block.
    pub texts: Vec<String>,
    /// Block type used for every appended block.
    #[serde(default = "default_block_type")]
    pub block_type: String,
}

/// Mirrors Python `CreateMarkdownPageRequest` (plus the tool-level id).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct CreateMarkdownPageRequest {
    pub workspace_id: String,
    #[schemars(
        description = "view_id of the parent Space or existing Page to create under. \
        Must NOT be the workspace_id: AppFlowy's hierarchy is workspace -> space -> page, and a \
        view created directly under the workspace root becomes a Space, not a document. Call \
        appflowy_list_spaces to pick a space (or appflowy_create_space to make one) and pass its \
        view_id here."
    )]
    pub parent_view_id: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub layout: i64,
    pub view_id: Option<String>,
    pub collab_id: Option<String>,
}

/// Mirrors Python `AppendMarkdownRequest` (plus the tool-level ids).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct AppendMarkdownRequest {
    pub workspace_id: String,
    pub page_id: String,
    pub content: String,
}

/// Mirrors community PR `appflowy_append_page_link` (`AppendPageLinkRequest`).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct AppendPageLinkRequest {
    pub workspace_id: String,
    pub page_id: String,
    pub target_page_id: String,
}

/// Mirrors Python `SavePageRequest` (plus the tool-level id).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct SavePageRequest {
    pub workspace_id: String,
    #[schemars(
        description = "view_id of the parent Space or existing Page to create under. \
        Must NOT be the workspace_id: AppFlowy's hierarchy is workspace -> space -> page, and a \
        view created directly under the workspace root becomes a Space, not a document. Call \
        appflowy_list_spaces to pick a space (or appflowy_create_space to make one) and pass its \
        view_id here."
    )]
    pub parent_view_id: String,
    pub title: String,
    pub content: String,
    #[serde(default = "default_content_format")]
    pub content_format: String,
    #[serde(default)]
    pub layout: i64,
    pub view_id: Option<String>,
    pub collab_id: Option<String>,
}

/// Mirrors Python `AppendPageContentRequest` (plus the tool-level ids).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct AppendPageContentRequest {
    pub workspace_id: String,
    pub page_id: String,
    pub content: String,
    #[serde(default = "default_content_format")]
    pub content_format: String,
}

/// Mirrors Python `ImportMarkdownFileRequest` (plus the tool-level id).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ImportMarkdownFileRequest {
    pub workspace_id: String,
    #[schemars(
        description = "view_id of the parent Space or existing Page to create under. \
        Must NOT be the workspace_id: AppFlowy's hierarchy is workspace -> space -> page, and a \
        view created directly under the workspace root becomes a Space, not a document. Call \
        appflowy_list_spaces to pick a space (or appflowy_create_space to make one) and pass its \
        view_id here."
    )]
    pub parent_view_id: String,
    pub path: String,
    pub title: Option<String>,
    #[serde(default = "default_upload_assets")]
    pub upload_assets: bool,
}

/// Mirrors Python `ImportMarkdownDirectoryRequest` (plus the tool-level id).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ImportMarkdownDirectoryRequest {
    pub workspace_id: String,
    #[schemars(
        description = "view_id of the parent Space or existing Page to create under. \
        Must NOT be the workspace_id: AppFlowy's hierarchy is workspace -> space -> page, and a \
        view created directly under the workspace root becomes a Space, not a document. Call \
        appflowy_list_spaces to pick a space (or appflowy_create_space to make one) and pass its \
        view_id here."
    )]
    pub parent_view_id: String,
    pub path: String,
    #[serde(default = "default_upload_assets")]
    pub upload_assets: bool,
}

#[tool_router(router = content_tool_router, vis = "pub(crate)")]
impl AppServer {
    /// Send one `append-block` request per top-level block, in input order,
    /// after reversing each block's direct `children` array.
    ///
    /// The `page-view/{id}/append-block` endpoint prepends what it receives:
    /// the top-level blocks of a single multi-block call are inserted at the
    /// front of the page (so a 9-block call lands reversed, last block first),
    /// and each block is inserted by pushing its `children` into the parent
    /// front-first one by one — at EVERY nesting level (a table's rows come
    /// out reversed AND each row's cells come out reversed, as verified live).
    /// Sending top-level blocks one request at a time in the caller's order,
    /// and recursively pre-reversing every `children` array, undoes both
    /// reversals (community-verified fix for scrambled append order).
    ///
    /// Returns the response of the last request and the number of requests
    /// sent. Aborts on the first failure, reporting how many blocks had
    /// already been sent.
    async fn append_blocks_ordered(
        &self,
        workspace_id: &str,
        page_id: &str,
        blocks: Vec<Value>,
    ) -> Result<(Value, usize), ToolError> {
        fn server_order(mut block: Value) -> Value {
            if let Some(children) = block.get_mut("children").and_then(Value::as_array_mut) {
                *children = children.drain(..).rev().map(server_order).collect();
            }
            block
        }

        let total = blocks.len();
        let mut last_response = Value::Null;
        for (index, block) in blocks.into_iter().enumerate() {
            let block = server_order(block);
            last_response = self
                .client
                .request(
                    Method::POST,
                    &format!("/api/workspace/{workspace_id}/page-view/{page_id}/append-block"),
                    None,
                    Some(json!({ "blocks": [block] })),
                )
                .await
                .map_err(|err| match map_tool_error(err) {
                    ToolError::Validation(message) => ToolError::validation(format!(
                        "append-block aborted after {index}/{total} block(s) sent: {message}"
                    )),
                    ToolError::Protocol(data) => ToolError::protocol(ErrorData::new(
                        data.code,
                        format!(
                            "append-block aborted after {index}/{total} block(s) sent: {}",
                            data.message
                        ),
                        data.data,
                    )),
                })?;
        }
        Ok((last_response, total))
    }

    #[tool(
        name = "appflowy_append_blocks_to_page",
        description = "Append raw AppFlowy document blocks to the end of a page."
    )]
    async fn appflowy_append_blocks_to_page(
        &self,
        Parameters(request): Parameters<AppendBlocksRequest>,
    ) -> Result<Json<Value>, ToolError> {
        if request.blocks.is_empty() {
            return Err(ToolError::validation("blocks must not be empty"));
        }
        let (data, calls) = self
            .append_blocks_ordered(&request.workspace_id, &request.page_id, request.blocks)
            .await?;
        Ok(Json(json!({ "result": data, "calls": calls })))
    }

    #[tool(
        name = "appflowy_append_text_to_page",
        description = "Append one or more plain-text paragraph blocks to the end of a page."
    )]
    async fn appflowy_append_text_to_page(
        &self,
        Parameters(request): Parameters<AppendTextRequest>,
    ) -> Result<Json<Value>, ToolError> {
        if request.texts.is_empty() {
            return Err(ToolError::validation("texts must not be empty"));
        }
        require_non_empty(&request.block_type, "block_type")?;
        let blocks: Vec<Value> = request
            .texts
            .iter()
            .map(|text| {
                json!({
                    "type": request.block_type,
                    "data": {"delta": [{"insert": text}]},
                })
            })
            .collect();
        let (data, calls) = self
            .append_blocks_ordered(&request.workspace_id, &request.page_id, blocks)
            .await?;
        Ok(Json(json!({ "result": data, "calls": calls })))
    }

    #[tool(
        name = "appflowy_create_markdown_page",
        description = "Create a document page from Markdown content. Prefer appflowy_save_page for general AI answer or note saving."
    )]
    async fn appflowy_create_markdown_page(
        &self,
        Parameters(request): Parameters<CreateMarkdownPageRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.title, "title")?;
        require_non_empty(&request.content, "content")?;
        validate_layout(request.layout)?;
        let blocks = parse_markdown_to_blocks(&request.content, None);
        let block_count = blocks.len();
        let data = create_page_with_blocks(
            &self.client,
            &request.workspace_id,
            &request.parent_view_id,
            &request.title,
            blocks,
            request.layout,
            request.view_id,
            request.collab_id,
        )
        .await
        .map_err(map_tool_error)?;
        Ok(Json(json!({ "page": data, "block_count": block_count })))
    }

    #[tool(
        name = "appflowy_append_markdown_to_page",
        description = "Append Markdown content to an existing document page."
    )]
    async fn appflowy_append_markdown_to_page(
        &self,
        Parameters(request): Parameters<AppendMarkdownRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.content, "content")?;
        let blocks = parse_markdown_to_blocks(&request.content, None);
        let block_count = blocks.len();
        let blocks: Vec<Value> = blocks
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()
            .map_err(|err| ToolError::internal(err.to_string()))?;
        let (data, calls) = self
            .append_blocks_ordered(&request.workspace_id, &request.page_id, blocks)
            .await?;
        Ok(Json(
            json!({ "result": data, "block_count": block_count, "calls": calls }),
        ))
    }

    #[tool(
        name = "appflowy_save_page",
        description = "Default tool for saving AI answers, notes, summaries, or generated content as a new AppFlowy document page. The content_format defaults to markdown, so Markdown headings, lists, links, code blocks, and rich text are converted to AppFlowy blocks unless content_format is explicitly set to plain_text."
    )]
    async fn appflowy_save_page(
        &self,
        Parameters(request): Parameters<SavePageRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.title, "title")?;
        require_non_empty(&request.content, "content")?;
        validate_layout(request.layout)?;
        let blocks = parse_content_to_blocks(&request.content, &request.content_format)
            .map_err(ToolError::validation)?;
        let block_count = blocks.len();
        let data = create_page_with_blocks(
            &self.client,
            &request.workspace_id,
            &request.parent_view_id,
            &request.title,
            blocks,
            request.layout,
            request.view_id,
            request.collab_id,
        )
        .await
        .map_err(map_tool_error)?;
        Ok(Json(json!({
            "page": data,
            "block_count": block_count,
            "content_format": request.content_format,
        })))
    }

    #[tool(
        name = "appflowy_append_page_content",
        description = "Default tool for appending AI answers, notes, summaries, or generated content to an existing AppFlowy document page. The content_format defaults to markdown."
    )]
    async fn appflowy_append_page_content(
        &self,
        Parameters(request): Parameters<AppendPageContentRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.content, "content")?;
        let blocks = parse_content_to_blocks(&request.content, &request.content_format)
            .map_err(ToolError::validation)?;
        let block_count = blocks.len();
        let blocks: Vec<Value> = blocks
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()
            .map_err(|err| ToolError::internal(err.to_string()))?;
        let (data, calls) = self
            .append_blocks_ordered(&request.workspace_id, &request.page_id, blocks)
            .await?;
        Ok(Json(json!({
            "result": data,
            "block_count": block_count,
            "calls": calls,
            "content_format": request.content_format,
        })))
    }

    #[tool(
        name = "appflowy_append_page_link",
        description = "Append a native Link-to-page block to an AppFlowy document page. This is a real AppFlowy block (not a Markdown hyperlink): in the AppFlowy GUI it renders as a clickable link that jumps straight to the target page."
    )]
    async fn appflowy_append_page_link(
        &self,
        Parameters(request): Parameters<AppendPageLinkRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.target_page_id, "target_page_id")?;
        let data = self
            .client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/page-view/{}/append-block",
                    request.workspace_id, request.page_id
                ),
                None,
                Some(json!({
                    "blocks": [{
                        "type": "linked_page",
                        "data": { "view_id": request.target_page_id },
                    }],
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({
            "result": data,
            "page_id": request.page_id,
            "target_page_id": request.target_page_id,
            "block_type": "linked_page",
        })))
    }

    #[tool(
        name = "appflowy_import_markdown_file",
        description = "Import one local Markdown file as an AppFlowy page. Local image references in Markdown are resolved relative to the file, uploaded to AppFlowy, and replaced with AppFlowy file URLs when upload_assets is true."
    )]
    async fn appflowy_import_markdown_file(
        &self,
        Parameters(request): Parameters<ImportMarkdownFileRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.path, "path")?;
        ensure_parent_is_not_workspace(&request.workspace_id, &request.parent_view_id)
            .map_err(map_tool_error)?;
        // Fresh importer per call: its accumulation state is per-import.
        let mut importer = MarkdownImporter::new(self.client.clone(), request.workspace_id.clone());
        let summary = importer
            .import_file(
                &request.path,
                &request.parent_view_id,
                request.title.as_deref(),
                request.upload_assets,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!(summary)))
    }

    #[tool(
        name = "appflowy_import_markdown_directory",
        description = "Recursively import a local Markdown directory into AppFlowy. Every local folder becomes an AppFlowy page, Markdown files become child pages, and local image references are uploaded and inserted in place."
    )]
    async fn appflowy_import_markdown_directory(
        &self,
        Parameters(request): Parameters<ImportMarkdownDirectoryRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.path, "path")?;
        ensure_parent_is_not_workspace(&request.workspace_id, &request.parent_view_id)
            .map_err(map_tool_error)?;
        // Fresh importer per call: its accumulation state is per-import.
        let mut importer = MarkdownImporter::new(self.client.clone(), request.workspace_id.clone());
        let summary = importer
            .import_directory(
                &request.path,
                &request.parent_view_id,
                request.upload_assets,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!(summary)))
    }
}

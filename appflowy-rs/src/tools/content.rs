//! Page content & import tools.
//!
//! Ports of `server.py` L672-885. Unlike Python (which takes `workspace_id`
//! and `page_id` as separate tool arguments), rmcp tools accept a single
//! parameters object, so those fields are flattened into each request struct.

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, Json, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::importer::MarkdownImporter;
use crate::markdown::{parse_content_to_blocks, parse_markdown_to_blocks};
use crate::server::AppServer;
use crate::tools::util::{create_page_with_blocks, ensure_parent_is_not_workspace, map_tool_error};

/// Python `Field(..., min_length=1)` on strings, checked after trimming.
fn require_non_empty(value: &str, field: &str) -> Result<(), ErrorData> {
    if value.trim().is_empty() {
        return Err(ErrorData::invalid_params(
            format!("{field} must not be empty"),
            None,
        ));
    }
    Ok(())
}

/// Python `Field(0, ge=0, le=4)` on layout.
fn validate_layout(layout: i64) -> Result<(), ErrorData> {
    if !(0..=4).contains(&layout) {
        return Err(ErrorData::invalid_params(
            format!("layout must be between 0 and 4, got {layout}"),
            None,
        ));
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
    #[schemars(description = "view_id of the parent Space or existing Page to create under. \
        Must NOT be the workspace_id: AppFlowy's hierarchy is workspace -> space -> page, and a \
        view created directly under the workspace root becomes a Space, not a document. Call \
        appflowy_list_spaces to pick a space (or appflowy_create_space to make one) and pass its \
        view_id here.")]
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

/// Mirrors Python `SavePageRequest` (plus the tool-level id).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct SavePageRequest {
    pub workspace_id: String,
    #[schemars(description = "view_id of the parent Space or existing Page to create under. \
        Must NOT be the workspace_id: AppFlowy's hierarchy is workspace -> space -> page, and a \
        view created directly under the workspace root becomes a Space, not a document. Call \
        appflowy_list_spaces to pick a space (or appflowy_create_space to make one) and pass its \
        view_id here.")]
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
    #[schemars(description = "view_id of the parent Space or existing Page to create under. \
        Must NOT be the workspace_id: AppFlowy's hierarchy is workspace -> space -> page, and a \
        view created directly under the workspace root becomes a Space, not a document. Call \
        appflowy_list_spaces to pick a space (or appflowy_create_space to make one) and pass its \
        view_id here.")]
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
    #[schemars(description = "view_id of the parent Space or existing Page to create under. \
        Must NOT be the workspace_id: AppFlowy's hierarchy is workspace -> space -> page, and a \
        view created directly under the workspace root becomes a Space, not a document. Call \
        appflowy_list_spaces to pick a space (or appflowy_create_space to make one) and pass its \
        view_id here.")]
    pub parent_view_id: String,
    pub path: String,
    #[serde(default = "default_upload_assets")]
    pub upload_assets: bool,
}

#[tool_router(router = content_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_append_blocks_to_page",
        description = "Append raw AppFlowy document blocks to the end of a page."
    )]
    async fn appflowy_append_blocks_to_page(
        &self,
        Parameters(request): Parameters<AppendBlocksRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        if request.blocks.is_empty() {
            return Err(ErrorData::invalid_params("blocks must not be empty", None));
        }
        let data = self
            .client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/page-view/{}/append-block",
                    request.workspace_id, request.page_id
                ),
                None,
                Some(json!({ "blocks": request.blocks })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_append_text_to_page",
        description = "Append one or more plain-text paragraph blocks to the end of a page."
    )]
    async fn appflowy_append_text_to_page(
        &self,
        Parameters(request): Parameters<AppendTextRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        if request.texts.is_empty() {
            return Err(ErrorData::invalid_params("texts must not be empty", None));
        }
        require_non_empty(&request.block_type, "block_type")?;
        let blocks: Vec<Value> = request
            .texts
            .iter()
            .map(|text| json!({
                "type": request.block_type,
                "data": {"delta": [{"insert": text}]},
            }))
            .collect();
        let data = self
            .client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/page-view/{}/append-block",
                    request.workspace_id, request.page_id
                ),
                None,
                Some(json!({ "blocks": blocks })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_create_markdown_page",
        description = "Create a document page from Markdown content. Prefer appflowy_save_page for general AI answer or note saving."
    )]
    async fn appflowy_create_markdown_page(
        &self,
        Parameters(request): Parameters<CreateMarkdownPageRequest>,
    ) -> Result<Json<Value>, ErrorData> {
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
    ) -> Result<Json<Value>, ErrorData> {
        require_non_empty(&request.content, "content")?;
        let blocks = parse_markdown_to_blocks(&request.content, None);
        let block_count = blocks.len();
        let data = self
            .client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/page-view/{}/append-block",
                    request.workspace_id, request.page_id
                ),
                None,
                Some(json!({ "blocks": blocks })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "result": data, "block_count": block_count })))
    }

    #[tool(
        name = "appflowy_save_page",
        description = "Default tool for saving AI answers, notes, summaries, or generated content as a new AppFlowy document page. The content_format defaults to markdown, so Markdown headings, lists, links, code blocks, and rich text are converted to AppFlowy blocks unless content_format is explicitly set to plain_text."
    )]
    async fn appflowy_save_page(
        &self,
        Parameters(request): Parameters<SavePageRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        require_non_empty(&request.title, "title")?;
        require_non_empty(&request.content, "content")?;
        validate_layout(request.layout)?;
        let blocks = parse_content_to_blocks(&request.content, &request.content_format)
            .map_err(|message| ErrorData::invalid_params(message, None))?;
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
    ) -> Result<Json<Value>, ErrorData> {
        require_non_empty(&request.content, "content")?;
        let blocks = parse_content_to_blocks(&request.content, &request.content_format)
            .map_err(|message| ErrorData::invalid_params(message, None))?;
        let block_count = blocks.len();
        let data = self
            .client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/page-view/{}/append-block",
                    request.workspace_id, request.page_id
                ),
                None,
                Some(json!({ "blocks": blocks })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({
            "result": data,
            "block_count": block_count,
            "content_format": request.content_format,
        })))
    }

    #[tool(
        name = "appflowy_import_markdown_file",
        description = "Import one local Markdown file as an AppFlowy page. Local image references in Markdown are resolved relative to the file, uploaded to AppFlowy, and replaced with AppFlowy file URLs when upload_assets is true."
    )]
    async fn appflowy_import_markdown_file(
        &self,
        Parameters(request): Parameters<ImportMarkdownFileRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        require_non_empty(&request.path, "path")?;
        ensure_parent_is_not_workspace(&request.workspace_id, &request.parent_view_id)
            .map_err(map_tool_error)?;
        // Fresh importer per call: its accumulation state is per-import.
        let mut importer =
            MarkdownImporter::new(self.client.clone(), request.workspace_id.clone());
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
    ) -> Result<Json<Value>, ErrorData> {
        require_non_empty(&request.path, "path")?;
        ensure_parent_is_not_workspace(&request.workspace_id, &request.parent_view_id)
            .map_err(map_tool_error)?;
        // Fresh importer per call: its accumulation state is per-import.
        let mut importer =
            MarkdownImporter::new(self.client.clone(), request.workspace_id.clone());
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

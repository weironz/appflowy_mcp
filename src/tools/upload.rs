//! File upload tool (`appflowy_upload_file`).

use std::io::ErrorKind;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, Json};
use serde::Deserialize;
use serde_json::Value;

use crate::client::MULTIPART_CHUNK_SIZE_BYTES;
use crate::server::AppServer;
// Shared expand_tilde (tools/util.rs): the private copy that used to live
// here diverged from it on bare `~` (it passed `~` through instead of
// expanding to the home dir, unlike Python's Path.expanduser).
use crate::tools::util::{expand_tilde, map_tool_error, ToolError};

/// Mirrors Python `models.py::UploadFileRequest` (workspace_id flattened in).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct UploadFileRequest {
    pub workspace_id: String,
    /// Local file path to upload.
    pub path: String,
    /// Directory key the blob is stored under; AppFlowy convention is the
    /// view_id of the page the file belongs to.
    pub parent_dir: String,
    /// MIME type. Guessed from the file name if omitted.
    pub content_type: Option<String>,
}

#[tool_router(router = upload_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_upload_file",
        description = "Upload a local file to AppFlowy file storage and return its URL, which can be referenced from image or file blocks in pages. Files of 5 MB or larger are uploaded as multipart chunks automatically (parallel parts with per-part retries); smaller files go in a single request.",
        annotations(title = "Upload file", open_world_hint = true)
    )]
    async fn appflowy_upload_file(
        &self,
        Parameters(request): Parameters<UploadFileRequest>,
    ) -> Result<Json<Value>, ToolError> {
        if request.path.is_empty() {
            return Err(ToolError::validation("Local file path to upload."));
        }
        let path = expand_tilde(&request.path);
        let content = tokio::fs::read(&path).await.map_err(|err| {
            let message = if err.kind() == ErrorKind::NotFound {
                format!("Local file to upload not found: {}", path.display())
            } else {
                format!(
                    "Cannot read local file to upload ({}): {err}",
                    path.display()
                )
            };
            ToolError::validation(message)
        })?;
        // Python: `request.content_type or mimetypes.guess_type(path.name)[0]
        // or "application/octet-stream"` — an empty content_type is falsy and
        // also falls through to the guess.
        let content_type = request
            .content_type
            .filter(|content_type| !content_type.is_empty())
            .or_else(|| mime_guess::from_path(&path).first_raw().map(str::to_string))
            .unwrap_or_else(|| "application/octet-stream".to_string());
        // AppFlowy-Web `MULTIPART_THRESHOLD` equals `CHUNK_SIZE` (5MB): at or
        // above it, upload in parallel chunks; below it stay on the
        // single-shot blob v1 endpoint. Both return the same response shape.
        let data = if content.len() >= MULTIPART_CHUNK_SIZE_BYTES {
            self.client
                .upload_file_multipart(
                    &request.workspace_id,
                    &request.parent_dir,
                    &content,
                    &content_type,
                )
                .await
        } else {
            self.client
                .upload_file_blob_v1(
                    &request.workspace_id,
                    &request.parent_dir,
                    &content,
                    &content_type,
                )
                .await
        }
        .map_err(map_tool_error)?;
        Ok(Json(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // expand_tilde itself now lives in tools/util.rs with its own tests
    // (including the bare-`~` and missing-home-var cases). Keep a smoke test
    // here to pin the behavior this tool relies on.

    #[test]
    fn upload_paths_without_tilde_pass_through() {
        assert_eq!(expand_tilde("/tmp/a.txt"), PathBuf::from("/tmp/a.txt"));
        assert_eq!(expand_tilde("~user/a.txt"), PathBuf::from("~user/a.txt"));
    }

    #[test]
    fn upload_tilde_expands_to_home() {
        // Whatever the home dir is, `~/x` must become an absolute-looking
        // join of it and "x", never a literal tilde.
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        if let Some(home) = std::env::var_os(home_var) {
            assert_eq!(expand_tilde("~/x"), PathBuf::from(&home).join("x"),);
        }
    }
}

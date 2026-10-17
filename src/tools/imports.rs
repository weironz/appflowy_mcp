//! Import tools: CSV-to-database import (`appflowy_import_csv`,
//! `appflowy_get_csv_import_status`, `appflowy_cancel_csv_import`),
//! document file import (`appflowy_import_document`,
//! `appflowy_get_document_import_status`) and Notion zip import
//! (`appflowy_import_notion_zip`, `appflowy_get_import_task`,
//! `appflowy_cancel_import_task`).
//!
//! Payloads mirror AppFlowy-Web `src/application/types.ts`
//! (`DatabaseCsvImportRequest`, `DocumentFileImportRequest`) and the call
//! flow in `src/application/services/js-services/http/import-api.ts`:
//! create the import task (`X-Host` header), then PUT the file bytes to the
//! returned presigned URL with a bare client (no Authorization header) and
//! the exact Content-Type the URL was signed for.

use std::io::ErrorKind;
use std::path::Path;

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{Error, Result as AppResult};
use crate::server::AppServer;
use crate::tools::util::{
    ensure_parent_is_not_workspace, map_tool_error, presigned_upload_client, ToolError,
};

/// Content type the server signs each document-import presigned URL for;
/// the PUT must carry the same value or S3 rejects the signature.
/// Mirrors AppFlowy-Web `DOCUMENT_FILE_CONTENT_TYPES`.
fn document_import_content_type(format: &str) -> &'static str {
    match format {
        "doc" => "application/msword",
        "html" => "text/html",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => "application/pdf",
    }
}

/// Read a local import file. Missing files are caller bugs (invalid params);
/// other I/O failures surface as internal errors.
async fn read_import_file(path: &Path) -> AppResult<Vec<u8>> {
    tokio::fs::read(path).await.map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            Error::Validation(format!(
                "Local file to import not found: {}",
                path.display()
            ))
        } else {
            Error::Other(format!(
                "Cannot read local file to import ({}): {err}",
                path.display()
            ))
        }
    })
}

/// PUT file bytes to a presigned URL. Presigned URLs carry the signature in
/// the URL itself, so this goes through the shared presigned-upload client,
/// which never sends the Authorization header. 2xx means success
/// (AppFlowy-Web accepts 200/204).
async fn put_presigned(presigned_url: &str, content_type: &str, bytes: Vec<u8>) -> AppResult<()> {
    let response = presigned_upload_client()
        .put(presigned_url)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(bytes)
        .send()
        .await
        .map_err(|err| Error::Other(format!("Presigned URL upload failed: {err}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Other(format!(
            "Presigned URL upload rejected with HTTP {status}"
        )));
    }
    Ok(())
}

/// PUT one multipart chunk to a presigned part URL and return the ETag the
/// storage assigns it. Same shared, no-auth client as [`put_presigned`];
/// presigned part URLs do not sign a Content-Type, so none is sent. S3
/// returns the ETag in a response header, passed verbatim into the
/// complete-multipart payload.
async fn put_presigned_part(presigned_url: &str, bytes: &[u8]) -> AppResult<String> {
    let response = presigned_upload_client()
        .put(presigned_url)
        .body(bytes.to_vec())
        .send()
        .await
        .map_err(|err| Error::Other(format!("Presigned part upload failed: {err}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Other(format!(
            "Presigned part upload rejected with HTTP {status}"
        )));
    }
    response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .filter(|etag| !etag.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Other("Presigned part upload response is missing ETag".into()))
}

/// Multipart branch of the Notion import flow: slice the zip into
/// `ceil(len / part_count)` chunks and PUT each to its presigned part URL,
/// sorted by part_number, then complete via
/// `POST /api/import/complete-multipart`.
async fn complete_import_multipart(
    server: &AppServer,
    multipart: &Value,
    bytes: &[u8],
) -> AppResult<()> {
    let upload_id = multipart
        .get("upload_id")
        .and_then(Value::as_str)
        .filter(|upload_id| !upload_id.is_empty())
        .ok_or_else(|| {
            Error::Other("Import task multipart response is missing upload_id".into())
        })?;
    let s3_key = multipart
        .get("s3_key")
        .and_then(Value::as_str)
        .filter(|s3_key| !s3_key.is_empty())
        .ok_or_else(|| Error::Other("Import task multipart response is missing s3_key".into()))?;
    let part_urls = multipart
        .get("part_presigned_urls")
        .and_then(Value::as_array)
        .filter(|urls| !urls.is_empty())
        .ok_or_else(|| {
            Error::Other("Import task multipart response is missing part_presigned_urls".into())
        })?;
    // Collect (part_number, presigned_url) pairs, then sort by part_number
    // BEFORE slicing byte ranges: the ranges are assigned by position below,
    // so if the server ever lists the parts out of order, the bytes would
    // land in the wrong parts and the assembled zip would be silently
    // corrupt. Stable sort keeps equal numbers in arrival order.
    let mut ordered: Vec<(u64, &str)> = Vec::with_capacity(part_urls.len());
    for (index, part) in part_urls.iter().enumerate() {
        let part_number = part
            .get("part_number")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                Error::Other(format!(
                    "Import task multipart part {index} is missing part_number"
                ))
            })?;
        let presigned_url = part
            .get("presigned_url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| {
                Error::Other(format!(
                    "Import task multipart part {part_number} is missing presigned_url"
                ))
            })?;
        ordered.push((part_number, presigned_url));
    }
    ordered.sort_by_key(|&(part_number, _)| part_number);
    let chunk_size = bytes.len().div_ceil(ordered.len()).max(1);
    let mut parts = Vec::with_capacity(ordered.len());
    for (index, (part_number, presigned_url)) in ordered.iter().enumerate() {
        let start = index * chunk_size;
        let end = ((index + 1) * chunk_size).min(bytes.len());
        let chunk = if start < end {
            &bytes[start..end]
        } else {
            &[][..]
        };
        let e_tag = put_presigned_part(presigned_url, chunk).await?;
        parts.push(json!({ "e_tag": e_tag, "part_number": part_number }));
    }
    server
        .client
        .request(
            Method::POST,
            "/api/import/complete-multipart",
            None,
            Some(json!({
                "s3_key": s3_key,
                "upload_id": upload_id,
                "parts": parts,
            })),
        )
        .await?;
    Ok(())
}

/// Presigned-URL upload via the standalone client, returning the create
/// response paired with `"uploaded": true`.
async fn create_and_upload(
    server: &AppServer,
    path: &str,
    payload: Value,
    presigned_content_type: &str,
    bytes: Vec<u8>,
) -> AppResult<Value> {
    let base_url = server.client.base_url().await;
    let headers: Vec<(&str, &str)> = vec![("X-Host", &base_url)];
    let task = server
        .client
        .request_ext(Method::POST, path, None, Some(payload), &headers)
        .await?;
    let presigned_url = task
        .get("presigned_url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| {
            Error::Other("Import task creation response is missing presigned_url".into())
        })?;
    put_presigned(presigned_url, presigned_content_type, bytes).await?;
    Ok(json!({ "task": task, "uploaded": true }))
}

/// Mirrors `DatabaseCsvImportRequest` from AppFlowy-Web
/// `src/application/types.ts` (csv options flattened in with defaults).
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ImportCsvRequest {
    /// Workspace importing the CSV.
    pub workspace_id: String,
    /// Local path of the .csv file to import.
    pub path: String,
    /// Import mode: "create" (default, new database), "append" or "replace"
    /// (both require database_id).
    pub mode: Option<String>,
    /// Parent page (view_id) the new database is created under. Optional;
    /// the server places it at the workspace root when omitted.
    pub parent_view_id: Option<String>,
    /// Target database id. Required for "append" / "replace" modes.
    pub database_id: Option<String>,
    /// Name of the imported database. Defaults to the file name.
    pub name: Option<String>,
    /// Database layout: "grid" (default), "board" or "calendar".
    pub layout: Option<String>,
    /// Whether the first row is a header row (default true).
    pub has_header: Option<bool>,
    /// CSV field delimiter (default ",").
    pub delimiter: Option<String>,
    /// CSV quote character (default "\"").
    pub quote: Option<String>,
    /// CSV escape character (optional).
    pub escape: Option<String>,
    /// CSV file encoding (default "utf-8").
    pub encoding: Option<String>,
    /// Trim whitespace around fields (default false).
    pub trim: Option<bool>,
}

/// Status polling / cancellation request for CSV imports.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct CsvImportTaskRequest {
    /// Workspace the import task belongs to.
    pub workspace_id: String,
    /// Task id returned by appflowy_import_csv.
    pub task_id: String,
}

/// Mirrors `DocumentFileImportRequest` from AppFlowy-Web
/// `src/application/types.ts` (file_name / content_length / format are
/// derived from the local file).
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ImportDocumentRequest {
    /// Workspace importing the document.
    pub workspace_id: String,
    /// Local path of the file to import. Supported extensions: .doc, .html,
    /// .docx, .pdf.
    pub path: String,
    /// Parent page (view_id) the imported document page is created under.
    pub parent_view_id: String,
    /// Name of the imported page. Defaults to the file name stem.
    pub name: Option<String>,
}

/// Status polling request for document imports.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct DocumentImportTaskRequest {
    /// Workspace the import task belongs to.
    pub workspace_id: String,
    /// Task id returned by appflowy_import_document.
    pub task_id: String,
}

/// Notion zip import request. The create-task payload mirrors AppFlowy-Web
/// `import-api.ts` (`{content_length, md5_base64}`; `md5_base64` is left out
/// because this crate has no md5 dependency — the client strips top-level
/// nulls).
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ImportNotionZipRequest {
    /// Workspace importing the Notion zip.
    pub workspace_id: String,
    /// Parent page (view_id) the imported Notion pages are created under;
    /// sent as the `page_id` query parameter.
    pub parent_view_id: String,
    /// Local path of the .zip file exported from Notion.
    pub path: String,
    /// If true, wait for the server-side import to finish. Not implemented:
    /// the call always returns once the zip is staged; poll
    /// appflowy_get_import_task to track progress.
    pub wait: Option<bool>,
}

/// Status polling / cancellation request for import tasks. The endpoints
/// (`GET /api/import`, `POST /api/import/tasks/{task_id}/cancel`) are not
/// scoped to a workspace, so only the task id is needed.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ImportTaskRequest {
    /// Task id returned by appflowy_import_notion_zip (or any other import
    /// flow managed by GET /api/import).
    pub task_id: String,
}

fn require_non_empty(value: &str, field: &str) -> AppResult<()> {
    if value.is_empty() {
        return Err(Error::Validation(format!("{field} must not be empty")));
    }
    Ok(())
}

#[tool_router(router = imports_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_import_csv",
        description = "Import a local .csv file into an AppFlowy database. Asynchronous task: the server-side import completes in the background after the file is staged to the returned presigned URL; poll with appflowy_get_csv_import_status."
    )]
    async fn appflowy_import_csv(
        &self,
        Parameters(request): Parameters<ImportCsvRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id").map_err(map_tool_error)?;
        require_non_empty(&request.path, "path").map_err(map_tool_error)?;
        let path = Path::new(&request.path);
        let extension = path
            .extension()
            .map(|ext| ext.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if extension != "csv" {
            return Err(ToolError::validation(format!(
                "Only .csv files are supported for database import, got: {}",
                request.path
            )));
        }
        let mode = request
            .mode
            .as_deref()
            .filter(|mode| !mode.is_empty())
            .unwrap_or("create");
        if !["create", "append", "replace"].contains(&mode) {
            return Err(ToolError::validation(format!(
                "mode must be one of \"create\", \"append\", \"replace\", got \"{mode}\""
            )));
        }
        if mode != "create" && request.database_id.as_deref().is_none_or(str::is_empty) {
            return Err(ToolError::validation(format!(
                "database_id is required for mode \"{mode}\""
            )));
        }
        let layout = request
            .layout
            .as_deref()
            .filter(|layout| !layout.is_empty())
            .unwrap_or("grid");
        if !["grid", "board", "calendar"].contains(&layout) {
            return Err(ToolError::validation(format!(
                "layout must be one of \"grid\", \"board\", \"calendar\", got \"{layout}\""
            )));
        }
        // Same guard the content tools apply (util.rs): the workspace root
        // view_id equals the workspace_id, and anything created directly
        // under it becomes a Space instead of landing inside one — reject
        // it before staging the file.
        if let Some(parent_view_id) = request
            .parent_view_id
            .as_deref()
            .filter(|parent_view_id| !parent_view_id.is_empty())
        {
            ensure_parent_is_not_workspace(&request.workspace_id, parent_view_id)
                .map_err(map_tool_error)?;
        }
        let bytes = read_import_file(path).await.map_err(map_tool_error)?;
        let name = request
            .name
            .filter(|name| !name.is_empty())
            .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()));
        let payload = json!({
            "content_length": bytes.len(),
            "mode": mode,
            "parent_view_id": request.parent_view_id,
            "database_id": request.database_id,
            "name": name,
            "layout": layout,
            "csv": {
                "has_header": request.has_header.unwrap_or(true),
                "delimiter": request.delimiter.filter(|d| !d.is_empty()).unwrap_or_else(|| ",".into()),
                "quote": request.quote.filter(|q| !q.is_empty()).unwrap_or_else(|| "\"".into()),
                "escape": request.escape.filter(|e| !e.is_empty()),
                "encoding": request.encoding.filter(|e| !e.is_empty()).unwrap_or_else(|| "utf-8".into()),
                "trim": request.trim.unwrap_or(false),
            },
        });
        create_and_upload(
            self,
            &format!(
                "/api/workspace/{}/database/import/csv",
                request.workspace_id
            ),
            payload,
            "text/csv",
            bytes,
        )
        .await
        .map(Json)
        .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_get_csv_import_status",
        description = "Poll the status of a CSV database import task created by appflowy_import_csv. The import runs asynchronously on the server; call repeatedly until status is Completed, Failed, Expire or Cancel."
    )]
    async fn appflowy_get_csv_import_status(
        &self,
        Parameters(request): Parameters<CsvImportTaskRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id").map_err(map_tool_error)?;
        require_non_empty(&request.task_id, "task_id").map_err(map_tool_error)?;
        self.client
            .request(
                Method::GET,
                &format!(
                    "/api/workspace/{}/database/import/csv/{}",
                    request.workspace_id, request.task_id
                ),
                None,
                None,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_cancel_csv_import",
        description = "Cancel a running CSV database import task created by appflowy_import_csv."
    )]
    async fn appflowy_cancel_csv_import(
        &self,
        Parameters(request): Parameters<CsvImportTaskRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id").map_err(map_tool_error)?;
        require_non_empty(&request.task_id, "task_id").map_err(map_tool_error)?;
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/database/import/csv/{}/cancel",
                    request.workspace_id, request.task_id
                ),
                None,
                None,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_import_document",
        description = "Import a local document file (.doc, .html, .docx or .pdf) as a new AppFlowy document page under a parent page. Asynchronous task: the server converts the file in the background after it is staged to the returned presigned URL; poll with appflowy_get_document_import_status."
    )]
    async fn appflowy_import_document(
        &self,
        Parameters(request): Parameters<ImportDocumentRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id").map_err(map_tool_error)?;
        require_non_empty(&request.parent_view_id, "parent_view_id").map_err(map_tool_error)?;
        require_non_empty(&request.path, "path").map_err(map_tool_error)?;
        let path = Path::new(&request.path);
        let format = path
            .extension()
            .map(|ext| ext.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !["doc", "html", "docx", "pdf"].contains(&format.as_str()) {
            return Err(ToolError::validation(format!(
                    "Unsupported document format: \"{format}\". Supported formats: doc, html, docx, pdf"
                )));
        }
        let bytes = read_import_file(path).await.map_err(map_tool_error)?;
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("import.{format}"));
        let name = request.name.filter(|name| !name.is_empty());
        let payload = json!({
            "content_length": bytes.len(),
            "file_name": file_name,
            "format": format,
            "parent_view_id": request.parent_view_id,
            "name": name,
        });
        create_and_upload(
            self,
            &format!("/api/import/{}/document", request.workspace_id),
            payload,
            document_import_content_type(&format),
            bytes,
        )
        .await
        .map(Json)
        .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_get_document_import_status",
        description = "Poll the status of a document file import task created by appflowy_import_document. The conversion runs asynchronously on the server; call repeatedly until status is Completed, Failed, Expire or Cancel."
    )]
    async fn appflowy_get_document_import_status(
        &self,
        Parameters(request): Parameters<DocumentImportTaskRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id").map_err(map_tool_error)?;
        require_non_empty(&request.task_id, "task_id").map_err(map_tool_error)?;
        self.client
            .request(
                Method::GET,
                &format!(
                    "/api/import/{}/document/{}",
                    request.workspace_id, request.task_id
                ),
                None,
                None,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_import_notion_zip",
        description = "Import a local .zip file exported from Notion into an AppFlowy workspace under a parent page. The task is created via POST /api/import/{workspace_id}/notion and the zip is staged to the returned presigned URL (single PUT, or per-part PUTs plus complete-multipart when the server requests multipart). The server-side import then runs in the background; poll with appflowy_get_import_task."
    )]
    async fn appflowy_import_notion_zip(
        &self,
        Parameters(request): Parameters<ImportNotionZipRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.workspace_id, "workspace_id").map_err(map_tool_error)?;
        require_non_empty(&request.parent_view_id, "parent_view_id").map_err(map_tool_error)?;
        require_non_empty(&request.path, "path").map_err(map_tool_error)?;
        let path = Path::new(&request.path);
        let extension = path
            .extension()
            .map(|ext| ext.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if extension != "zip" {
            return Err(ToolError::validation(format!(
                "Only .zip files are supported for Notion import, got: {}",
                request.path
            )));
        }
        let bytes = read_import_file(path).await.map_err(map_tool_error)?;
        let base_url = self.client.base_url().await;
        let headers: Vec<(&str, &str)> = vec![("X-Host", &base_url)];
        let params = [("page_id", request.parent_view_id.clone())];
        let task = self
            .client
            .request_ext(
                Method::POST,
                &format!("/api/import/{}/notion", request.workspace_id),
                Some(&params),
                // md5_base64 is optional in AppFlowy-Web's payload; this
                // crate has no md5 dependency, so it is left out (the client
                // strips top-level nulls from the JSON body).
                Some(json!({
                    "content_length": bytes.len(),
                    "md5_base64": Value::Null,
                })),
                &headers,
            )
            .await
            .map_err(map_tool_error)?;
        match task.get("multipart").filter(|value| !value.is_null()) {
            Some(multipart) => complete_import_multipart(self, multipart, &bytes)
                .await
                .map_err(map_tool_error)?,
            None => {
                let presigned_url = task
                    .get("presigned_url")
                    .and_then(Value::as_str)
                    .filter(|url| !url.is_empty())
                    .ok_or_else(|| {
                        map_tool_error(Error::Other(
                            "Notion import task creation response is missing presigned_url".into(),
                        ))
                    })?;
                put_presigned(presigned_url, "application/zip", bytes)
                    .await
                    .map_err(map_tool_error)?;
            }
        }
        Ok(Json(json!({
            "task": task,
            "uploaded": true,
            "note": "server-side import runs in background; poll appflowy_get_import_task",
        })))
    }

    #[tool(
        name = "appflowy_get_import_task",
        description = "Get the status of an import task (e.g. one created by appflowy_import_notion_zip). The public client exposes no per-task status endpoint, so this calls GET /api/import and filters the returned task list by task_id; the task entry is returned verbatim with `found: true`. The `status` field may be an enum number or a string — its values are passed through as-is and the enumeration shown by the server (e.g. pending/uploading/importing/succeeded/failed) is not exhaustively documented; treat the server as authoritative. If the task is absent from the list, returns {task_id, status: \"unknown\", found: false}."
    )]
    async fn appflowy_get_import_task(
        &self,
        Parameters(request): Parameters<ImportTaskRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.task_id, "task_id").map_err(map_tool_error)?;
        let response = self
            .client
            .request(Method::GET, "/api/import", None, None)
            .await
            .map_err(map_tool_error)?;
        let tasks = response
            .get("tasks")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                map_tool_error(Error::Other(
                    "GET /api/import response is missing the tasks list".into(),
                ))
            })?;
        let found = tasks.iter().find(|entry| {
            entry.get("task_id").and_then(Value::as_str) == Some(request.task_id.as_str())
        });
        match found {
            Some(Value::Object(entry)) => {
                let mut entry = entry.clone();
                entry.insert("found".into(), json!(true));
                Ok(Json(Value::Object(entry)))
            }
            Some(other) => Ok(Json(json!({ "task": other.clone(), "found": true }))),
            None => Ok(Json(json!({
                "task_id": request.task_id,
                "status": "unknown",
                "found": false,
            }))),
        }
    }

    #[tool(
        name = "appflowy_cancel_import_task",
        description = "Cancel a running import task (e.g. one created by appflowy_import_notion_zip) via POST /api/import/tasks/{task_id}/cancel."
    )]
    async fn appflowy_cancel_import_task(
        &self,
        Parameters(request): Parameters<ImportTaskRequest>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&request.task_id, "task_id").map_err(map_tool_error)?;
        self.client
            .request(
                Method::POST,
                &format!("/api/import/tasks/{}/cancel", request.task_id),
                None,
                None,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }
}

//! Import tools: CSV-to-database import (`appflowy_import_csv`,
//! `appflowy_get_csv_import_status`, `appflowy_cancel_csv_import`) and
//! document file import (`appflowy_import_document`,
//! `appflowy_get_document_import_status`).
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
use rmcp::{ErrorData, Json, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Error, Result as AppResult};
use crate::server::AppServer;
use crate::tools::util::map_tool_error;

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
            Error::Validation(format!("Local file to import not found: {}", path.display()))
        } else {
            Error::Other(format!(
                "Cannot read local file to import ({}): {err}",
                path.display()
            ))
        }
    })
}

/// PUT file bytes to a presigned URL. Presigned URLs carry the signature in
/// the URL itself, so this uses a standalone client with no Authorization
/// header. 2xx means success (AppFlowy-Web accepts 200/204).
async fn put_presigned(presigned_url: &str, content_type: &str, bytes: Vec<u8>) -> AppResult<()> {
    let response = reqwest::Client::new()
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
    ) -> Result<Json<Value>, ErrorData> {
        require_non_empty(&request.workspace_id, "workspace_id").map_err(map_tool_error)?;
        require_non_empty(&request.path, "path").map_err(map_tool_error)?;
        let path = Path::new(&request.path);
        let extension = path
            .extension()
            .map(|ext| ext.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if extension != "csv" {
            return Err(ErrorData::invalid_params(
                format!(
                    "Only .csv files are supported for database import, got: {}",
                    request.path
                ),
                None,
            ));
        }
        let mode = request
            .mode
            .as_deref()
            .filter(|mode| !mode.is_empty())
            .unwrap_or("create");
        if !["create", "append", "replace"].contains(&mode) {
            return Err(ErrorData::invalid_params(
                format!("mode must be one of \"create\", \"append\", \"replace\", got \"{mode}\""),
                None,
            ));
        }
        if mode != "create" && request.database_id.as_deref().is_none_or(str::is_empty) {
            return Err(ErrorData::invalid_params(
                format!("database_id is required for mode \"{mode}\""),
                None,
            ));
        }
        let layout = request
            .layout
            .as_deref()
            .filter(|layout| !layout.is_empty())
            .unwrap_or("grid");
        if !["grid", "board", "calendar"].contains(&layout) {
            return Err(ErrorData::invalid_params(
                format!("layout must be one of \"grid\", \"board\", \"calendar\", got \"{layout}\""),
                None,
            ));
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
            &format!("/api/workspace/{}/database/import/csv", request.workspace_id),
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
    ) -> Result<Json<Value>, ErrorData> {
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
    ) -> Result<Json<Value>, ErrorData> {
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
    ) -> Result<Json<Value>, ErrorData> {
        require_non_empty(&request.workspace_id, "workspace_id").map_err(map_tool_error)?;
        require_non_empty(&request.parent_view_id, "parent_view_id").map_err(map_tool_error)?;
        require_non_empty(&request.path, "path").map_err(map_tool_error)?;
        let path = Path::new(&request.path);
        let format = path
            .extension()
            .map(|ext| ext.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !["doc", "html", "docx", "pdf"].contains(&format.as_str()) {
            return Err(ErrorData::invalid_params(
                format!(
                    "Unsupported document format: \"{format}\". Supported formats: doc, html, docx, pdf"
                ),
                None,
            ));
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
    ) -> Result<Json<Value>, ErrorData> {
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
}

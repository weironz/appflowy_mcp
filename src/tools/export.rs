//! Export tools (`appflowy_export_page`, `appflowy_export_space`,
//! `appflowy_export_workspace`) plus the fetch/prefetch helpers they share.
//!
//! Port of Python `server.py` L1687-2023. Two deliberate behavior upgrades
//! over the Python original:
//!
//! - `fetch_page_markdown` records every failed attempt (page-view retries
//!   *and* the raw-collab fallback); the Python `except Exception: continue`
//!   swallowed them. A total failure now carries the root cause plus a
//!   per-path attempt summary instead of the last exception alone.
//! - `appflowy_export_page` reuses the full `fetch_page_markdown`
//!   retry + fallback chain instead of Python's weaker one-shot page-view
//!   fetch, and `appflowy_export_space` / `appflowy_export_workspace`
//!   validate the destination directory (as `invalid_params`) *before* any
//!   network request — Python rewrapped that check as a generic error from
//!   inside its own `try` block.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, Json, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::b64::base64_decode;
use crate::client::AppFlowyClient;
use crate::collab::decode_document;
use crate::error::{Error, Result};
use crate::render::{document_to_markdown, sanitize_filename};
use crate::server::AppServer;
use crate::tools::util::{map_tool_error, unique_path};

/// Upper bound on concurrent page fetches during a tree export.
/// Python: `EXPORT_MAX_WORKERS = 8`.
pub const EXPORT_MAX_WORKERS: usize = 8;

/// Python `fetch_page_markdown(..., retries=2, backoff=0.4)` defaults.
pub const DEFAULT_PAGE_RETRIES: u32 = 2;
pub const DEFAULT_PAGE_BACKOFF_SECS: f64 = 0.4;

/// Per-page outcome of a prefetch: rendered Markdown or the error message.
/// Python: `dict[str, str | Exception]`.
pub type PrefetchCache = HashMap<String, std::result::Result<String, String>>;

/// Mirrors Python `models.py::ExportPageRequest`.
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ExportPageRequest {
    /// Local destination file path for the Markdown export. '.md' is appended if missing. Parent directories are created.
    pub path: String,
}

/// Mirrors Python `models.py::ExportTreeRequest`.
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ExportTreeRequest {
    /// Local destination directory. Created if missing; must be empty or not yet exist so existing files are never overwritten.
    pub path: String,
}

/// Parameters of `appflowy_export_page` (workspace_id / page_id / request,
/// as in the Python tool signature).
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ExportPageParams {
    /// Workspace containing the page.
    pub workspace_id: String,
    /// View id of the page to export.
    pub page_id: String,
    /// Export options.
    pub request: ExportPageRequest,
}

/// Parameters of `appflowy_export_space`.
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ExportSpaceParams {
    /// Workspace containing the space.
    pub workspace_id: String,
    /// View id of the space (or page subtree root) to export.
    pub space_view_id: String,
    /// Export options.
    pub request: ExportTreeRequest,
}

/// Parameters of `appflowy_export_workspace`.
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ExportWorkspaceParams {
    /// Workspace to export.
    pub workspace_id: String,
    /// Export options.
    pub request: ExportTreeRequest,
}

/// Python truthiness for a JSON value (`or`-chain semantics).
fn is_json_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !*b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

/// `data.get(a) or data.get(b)` for a list of keys.
fn first_truthy<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .find(|v| !is_json_falsy(v))
}

/// Port of Python `_encoded_collab_bytes`: normalize an encoded-collab
/// payload to raw bytes.
///
/// The page-view endpoint returns the raw yjs update as a list of ints,
/// while the raw-collab endpoint wraps it as
/// `EncodedCollab {state_vector, doc_state}`; `doc_state` may itself be a
/// list of ints or a base64 string. Returns `None` on any unusable shape
/// (Python also returns an empty list for an empty payload; both are falsy
/// and the callers here treat empty as missing).
pub fn encoded_collab_bytes(value: &Value) -> Option<Vec<u8>> {
    let mut current = value;
    if let Value::Object(map) = value {
        current = match map.get("doc_state") {
            Some(v) if !is_json_falsy(v) => v,
            _ => map.get("encoded_collab_v1")?,
        };
    }
    match current {
        Value::String(s) => base64_decode(s),
        Value::Array(items) => {
            let bytes: Option<Vec<u8>> = items
                .iter()
                .map(|item| item.as_u64().and_then(|n| u8::try_from(n).ok()))
                .collect();
            bytes.filter(|b| !b.is_empty())
        }
        _ => None,
    }
}

/// Port of Python `_markdown_body_is_empty`: true if the rendered Markdown
/// has only its title heading, no body.
///
/// AppFlowy loads document collabs lazily; under bulk load a content page
/// can momentarily come back with an empty document that renders to just
/// its title.
pub fn markdown_body_is_empty(markdown: &str, title: Option<&str>) -> bool {
    let mut text = markdown.trim();
    if let Some(title) = title.filter(|t| !t.is_empty()) {
        let heading = format!("# {title}");
        let heading = heading.trim();
        if let Some(rest) = text.strip_prefix(heading) {
            text = rest;
        }
    }
    text.trim().is_empty()
}

/// One page-view fetch + decode, shared by [`fetch_page_markdown`]'s retry
/// loop. Python inlines this in the loop body.
async fn fetch_page_view_markdown(
    client: &AppFlowyClient,
    workspace_id: &str,
    view_id: &str,
    title: Option<&str>,
) -> Result<String> {
    let data = client
        .request(
            Method::GET,
            &format!("/api/workspace/{workspace_id}/page-view/{view_id}"),
            None,
            None,
        )
        .await?;
    let encoded = data
        .get("data")
        .and_then(|d| d.get("encoded_collab"))
        .filter(|v| !is_json_falsy(v))
        .ok_or_else(|| Error::Other("Page response did not include encoded_collab.".into()))?;
    // Python feeds the payload to decode_document directly, which requires a
    // list of ints; routing it through `encoded_collab_bytes` additionally
    // tolerates a base64 string (Python would have errored and retried).
    let bytes = encoded_collab_bytes(encoded)
        .ok_or_else(|| Error::Other("Page response encoded_collab is not a byte payload.".into()))?;
    let document = decode_document(&bytes)?;
    Ok(document_to_markdown(&document, title))
}

/// Port of Python `fetch_collab_markdown` with error trails: fetch a
/// document via the raw collab endpoint, or `None` if unavailable.
///
/// `GET /api/workspace/v1/{ws}/collab/{object_id}` reads collab storage
/// directly, bypassing the lazy page-view path where "Collab not found" and
/// empty-document transients originate. collab_type for a document is tried
/// as both the numeric and the named form. The returned `Vec<String>` is a
/// per-attempt failure log (Python swallowed it with
/// `except Exception: continue`).
pub async fn fetch_collab_markdown(
    client: &AppFlowyClient,
    workspace_id: &str,
    object_id: &str,
    title: Option<&str>,
) -> (Option<String>, Vec<String>) {
    let path = format!("/api/workspace/v1/{workspace_id}/collab/{object_id}");
    let mut attempts = Vec::new();
    for collab_type in ["0", "Document"] {
        let params = [("collab_type", collab_type.to_string())];
        let data = match client.request(Method::GET, &path, Some(&params), None).await {
            Ok(data) => data,
            Err(e) => {
                attempts.push(format!(
                    "raw collab GET {path} (collab_type={collab_type}): request failed: {e}"
                ));
                continue;
            }
        };
        let encoded = first_truthy(&data, &["encode_collab", "encoded_collab"])
            .and_then(encoded_collab_bytes)
            .filter(|b| !b.is_empty());
        let Some(bytes) = encoded else {
            attempts.push(format!(
                "raw collab GET {path} (collab_type={collab_type}): response had no usable encoded collab payload"
            ));
            continue;
        };
        match decode_document(&bytes) {
            Ok(document) => {
                return (Some(document_to_markdown(&document, title)), attempts);
            }
            Err(e) => {
                attempts.push(format!(
                    "raw collab GET {path} (collab_type={collab_type}): failed to decode collab: {e}"
                ));
                continue;
            }
        }
    }
    (None, attempts)
}

/// Port of Python `fetch_page_markdown` with error trails.
///
/// AppFlowy serves document collabs lazily, so under a large export a page
/// that actually has content can transiently 404 ("Collab not found") or
/// return an empty document. Retry both; if page-view still can't produce
/// content, fall back to the raw collab endpoint, which bypasses that path.
///
/// Unlike Python (which re-raised only the last page-view exception), a
/// total failure reports the root cause plus a summary of every attempt on
/// both paths.
pub async fn fetch_page_markdown(
    client: &AppFlowyClient,
    workspace_id: &str,
    view_id: &str,
    title: Option<&str>,
    retries: u32,
    backoff_secs: f64,
) -> Result<String> {
    let mut traces: Vec<String> = Vec::new();
    let mut page_markdown: Option<String> = None;
    let mut page_error: Option<Error> = None;
    for attempt in 0..=retries {
        match fetch_page_view_markdown(client, workspace_id, view_id, title).await {
            Ok(markdown) => {
                page_error = None;
                if !markdown_body_is_empty(&markdown, title) {
                    return Ok(markdown);
                }
                traces.push(format!(
                    "page-view GET /api/workspace/{workspace_id}/page-view/{view_id} (attempt {}): document body was empty",
                    attempt + 1
                ));
                page_markdown = Some(markdown);
            }
            Err(e) => {
                traces.push(format!(
                    "page-view GET /api/workspace/{workspace_id}/page-view/{view_id} (attempt {}): {e}",
                    attempt + 1
                ));
                page_error = Some(e);
            }
        }
        if attempt < retries {
            let delay = backoff_secs * 2f64.powi(attempt as i32);
            tokio::time::sleep(std::time::Duration::from_secs_f64(delay.max(0.0))).await;
        }
    }

    // page-view exhausted (empty or failing) -> try the raw collab endpoint.
    let (fallback, mut collab_traces) =
        fetch_collab_markdown(client, workspace_id, view_id, title).await;
    traces.append(&mut collab_traces);
    if let Some(md) = &fallback {
        if !markdown_body_is_empty(md, title) {
            return Ok(md.clone());
        }
    }
    if let Some(md) = page_markdown {
        return Ok(md); // genuinely empty page
    }
    if let Some(md) = fallback {
        return Ok(md);
    }
    let root_cause = page_error
        .map(|e| e.to_string())
        .unwrap_or_else(|| "no page-view error recorded".to_string());
    Err(Error::Other(format!(
        "Could not fetch page {view_id}: root cause: {root_cause}; attempts: {}",
        traces.join(" | ")
    )))
}

/// Port of Python `_collect_document_views`: flatten the tree to the
/// document views (non-space, document layout).
pub fn collect_document_views(views: &[Value]) -> Vec<&Value> {
    fn is_document(view: &Value) -> bool {
        !view.get("is_space").and_then(Value::as_bool).unwrap_or(false)
            && view.get("layout").and_then(Value::as_i64).unwrap_or(0) == 0
            && view
                .get("view_id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
    }
    fn flatten<'a>(views: &'a [Value], out: &mut Vec<&'a Value>) {
        for view in views {
            if is_document(view) {
                out.push(view);
            }
            if let Some(children) = view.get("children").and_then(Value::as_array) {
                flatten(children, out);
            }
        }
    }
    let mut out = Vec::new();
    flatten(views, &mut out);
    out
}

/// Port of Python `_prefetch_page_markdowns`: fetch every document's
/// Markdown concurrently, bounded by [`EXPORT_MAX_WORKERS`].
///
/// Export is otherwise one serial round-trip per page. Failures are stored
/// per page (as the error string) and surface as warnings, same as Python.
/// Python used a `ThreadPoolExecutor`; here a `JoinSet` gated by a
/// `Semaphore`.
pub async fn prefetch_page_markdowns(
    client: &AppFlowyClient,
    workspace_id: &str,
    views: &[Value],
) -> PrefetchCache {
    let docs: Vec<(String, String)> = collect_document_views(views)
        .iter()
        .map(|view| {
            let view_id = view
                .get("view_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // Python: `v.get("name") or ""`.
            let name = view
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            (view_id, name)
        })
        .collect();
    let mut cache = HashMap::new();
    if docs.is_empty() {
        return cache;
    }
    let workers = EXPORT_MAX_WORKERS.min(docs.len());
    let semaphore = Arc::new(Semaphore::new(workers));
    let mut set = JoinSet::new();
    for (view_id, name) in docs {
        // Acquired before spawning, so at most `workers` tasks ever run.
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closed");
        let client = client.clone();
        let workspace_id = workspace_id.to_string();
        set.spawn(async move {
            let _permit = permit;
            let result = fetch_page_markdown(
                &client,
                &workspace_id,
                &view_id,
                Some(&name),
                DEFAULT_PAGE_RETRIES,
                DEFAULT_PAGE_BACKOFF_SECS,
            )
            .await
            .map_err(|e| e.to_string());
            (view_id, result)
        });
    }
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((view_id, result)) => {
                cache.insert(view_id, result);
            }
            Err(e) => tracing::warn!(%e, "prefetch task failed to join"),
        }
    }
    cache
}

/// Recursive core of [`export_views_to_directory`]; the prefetched cache is
/// always present here (Python threads an optional `cache` parameter
/// through the same recursion). `async fn` can't recurse, hence the boxed
/// future.
#[allow(clippy::too_many_arguments)]
fn export_views_recursive<'a>(
    client: &'a AppFlowyClient,
    workspace_id: &'a str,
    views: &'a [Value],
    directory: &'a Path,
    exported: &'a mut Vec<String>,
    warnings: &'a mut Vec<String>,
    cache: &'a PrefetchCache,
) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        for view in views {
            let name = view.get("name").and_then(Value::as_str).unwrap_or_default();
            let children: &[Value] = view
                .get("children")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            // Python: `str(view.get("view_id", ""))[:8]`.
            let fallback: String = view
                .get("view_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(8)
                .collect();
            let filename = sanitize_filename(name, &fallback);

            let layout = view.get("layout").and_then(Value::as_i64).unwrap_or(0);
            if layout != 0 {
                let layout_display = view
                    .get("layout")
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_else(|| "0".to_string());
                warnings.push(format!(
                    "skipped non-document view '{name}' (layout {layout_display})"
                ));
                continue;
            }

            let mut markdown: Option<String> = None;
            if !view.get("is_space").and_then(Value::as_bool).unwrap_or(false) {
                match view.get("view_id").and_then(Value::as_str) {
                    Some(view_id) => match cache.get(view_id) {
                        Some(Err(e)) => {
                            warnings.push(format!("failed to export '{name}': {e}"));
                        }
                        Some(Ok(md)) => markdown = Some(md.clone()),
                        None => {
                            match fetch_page_markdown(
                                client,
                                workspace_id,
                                view_id,
                                Some(name),
                                DEFAULT_PAGE_RETRIES,
                                DEFAULT_PAGE_BACKOFF_SECS,
                            )
                            .await
                            {
                                Ok(md) => markdown = Some(md),
                                Err(e) => {
                                    warnings.push(format!("failed to export '{name}': {e}"));
                                }
                            }
                        }
                    },
                    // Python would crash the whole export with a KeyError
                    // here; record a warning and carry on instead.
                    None => warnings.push(format!("failed to export '{name}': missing view_id")),
                }
            }

            if !children.is_empty() {
                let subdir = unique_path(&directory.join(&filename));
                if let Err(e) = std::fs::create_dir(&subdir) {
                    warnings.push(format!("failed to create folder for '{name}': {e}"));
                    continue;
                }
                if let Some(md) = &markdown {
                    let readme = subdir.join("README.md");
                    match std::fs::write(&readme, md) {
                        Ok(()) => exported.push(readme.display().to_string()),
                        Err(e) => {
                            warnings.push(format!("failed to write '{name}/README.md': {e}"));
                        }
                    }
                }
                export_views_recursive(
                    client,
                    workspace_id,
                    children,
                    &subdir,
                    &mut *exported,
                    &mut *warnings,
                    cache,
                )
                .await;
            } else if let Some(md) = &markdown {
                let target = unique_path(&directory.join(format!("{filename}.md")));
                match std::fs::write(&target, md) {
                    Ok(()) => exported.push(target.display().to_string()),
                    Err(e) => {
                        warnings.push(format!("failed to write '{name}.md': {e}"));
                    }
                }
            }
        }
    })
}

/// Port of Python `export_views_to_directory`: mirror a view tree into a
/// directory of Markdown files — a page with children becomes a folder
/// whose own content is `README.md`, leaf pages become `.md` files.
///
/// All page contents are prefetched in parallel first; the recursive walk
/// then just reads from the cache. Every per-view failure becomes a
/// warning (never aborts the export), so this only errors on nothing —
/// the `Result` keeps the signature future-proof for callers.
/// File IO is synchronous `std::fs`: the payloads are small Markdown files
/// and it keeps the recursive walk simple.
pub async fn export_views_to_directory(
    client: &AppFlowyClient,
    workspace_id: &str,
    views: &[Value],
    directory: &Path,
    exported: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let cache = prefetch_page_markdowns(client, workspace_id, views).await;
    export_views_recursive(
        client,
        workspace_id,
        views,
        directory,
        exported,
        warnings,
        &cache,
    )
    .await;
    Ok(())
}

/// `Path(request.path).expanduser()`: expand a leading `~` via `USERPROFILE`
/// on Windows, `HOME` otherwise.
fn expand_tilde(raw: &str) -> PathBuf {
    fn home_dir() -> Option<PathBuf> {
        #[cfg(windows)]
        {
            std::env::var_os("USERPROFILE").map(PathBuf::from)
        }
        #[cfg(not(windows))]
        {
            std::env::var_os("HOME").map(PathBuf::from)
        }
    }
    if raw == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

/// Shared destination-directory preparation for `appflowy_export_space` /
/// `appflowy_export_workspace`: expanduser -> reject if it exists and is
/// non-empty -> create it. Meant to run *before* any network request, so
/// the not-empty check surfaces as `Error::Validation` (invalid_params)
/// instead of Python's post-hoc generic rewrap.
fn prepare_destination_directory(raw: &str) -> Result<PathBuf> {
    let directory = expand_tilde(raw);
    if directory.exists() {
        let mut entries = std::fs::read_dir(&directory).map_err(|e| {
            Error::Other(format!(
                "Failed to read destination directory {}: {e}",
                directory.display()
            ))
        })?;
        if entries.next().is_some() {
            return Err(Error::Validation(format!(
                "Destination directory is not empty: {}",
                directory.display()
            )));
        }
    }
    std::fs::create_dir_all(&directory)
        .map_err(|e| Error::Other(format!("Failed to create directory {}: {e}", directory.display())))?;
    Ok(directory)
}

/// Wrap tool-body failures the way Python did:
/// `raise Exception(f"Failed to export {what}: {e}")`.
fn export_error(what: &str, e: Error) -> ErrorData {
    ErrorData::internal_error(format!("Failed to export {what}: {e}"), None)
}

#[tool_router(router = export_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_export_page",
        description = "Export a single page as a local Markdown file, preserving inline formatting (bold, italic, links, code). Child pages are not included; use appflowy_export_space for a whole subtree."
    )]
    async fn appflowy_export_page(
        &self,
        Parameters(params): Parameters<ExportPageParams>,
    ) -> std::result::Result<Json<Value>, ErrorData> {
        let ExportPageParams {
            workspace_id,
            page_id,
            request,
        } = params;
        let run = async {
            // Resolve the page name first (Python read it from the same
            // page-view response), then reuse the full fetch_page_markdown
            // retry + fallback chain for the content.
            let data = self
                .client
                .request(
                    Method::GET,
                    &format!("/api/workspace/{workspace_id}/page-view/{page_id}"),
                    None,
                    None,
                )
                .await?;
            let name = data
                .get("view")
                .and_then(|v| v.get("name"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&page_id)
                .to_string();
            let markdown = fetch_page_markdown(
                &self.client,
                &workspace_id,
                &page_id,
                Some(&name),
                DEFAULT_PAGE_RETRIES,
                DEFAULT_PAGE_BACKOFF_SECS,
            )
            .await?;

            let mut path = expand_tilde(&request.path);
            let is_markdown = path
                .extension()
                .map(|e| {
                    let ext = e.to_string_lossy().to_lowercase();
                    ext == "md" || ext == "markdown"
                })
                .unwrap_or(false);
            if !is_markdown {
                if let Some(file_name) = path.file_name() {
                    let mut file_name = file_name.to_os_string();
                    file_name.push(".md");
                    path.set_file_name(file_name);
                } else {
                    path.push(".md");
                }
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(Error::Io)?;
            }
            let path = unique_path(&path);
            std::fs::write(&path, markdown).map_err(Error::Io)?;
            Ok::<Value, Error>(json!({
                "page_id": page_id,
                "name": name,
                "path": path.display().to_string(),
            }))
        };
        run.await.map(Json).map_err(|e| export_error("page", e))
    }

    #[tool(
        name = "appflowy_export_space",
        description = "Export a space (or any page subtree) to a local directory of Markdown files, mirroring appflowy_import_markdown_directory: a page with children becomes a folder whose own content is README.md, leaf pages become .md files. Non-document views (grids, boards) are skipped with warnings."
    )]
    async fn appflowy_export_space(
        &self,
        Parameters(params): Parameters<ExportSpaceParams>,
    ) -> std::result::Result<Json<Value>, ErrorData> {
        // Destination validation is a parameter error and must fire before
        // any network request (Python rewrapped it as a generic error).
        let directory = prepare_destination_directory(&params.request.path)
            .map_err(map_tool_error)?;
        let run = async {
            let params_qs = [
                ("depth", "10".to_string()),
                ("root_view_id", params.space_view_id.clone()),
            ];
            let root = self
                .client
                .request(
                    Method::GET,
                    &format!("/api/workspace/{}/folder", params.workspace_id),
                    Some(&params_qs),
                    None,
                )
                .await?;
            let mut exported = Vec::new();
            let mut warnings = Vec::new();
            export_views_to_directory(
                &self.client,
                &params.workspace_id,
                std::slice::from_ref(&root),
                &directory,
                &mut exported,
                &mut warnings,
            )
            .await?;
            Ok::<Value, Error>(json!({
                "root_view_id": params.space_view_id,
                "path": directory.display().to_string(),
                "exported_files": exported,
                "warnings": warnings,
            }))
        };
        run.await.map(Json).map_err(|e| export_error("space", e))
    }

    #[tool(
        name = "appflowy_export_workspace",
        description = "Export every space in a workspace to a local directory tree of Markdown files. Each space becomes a top-level folder; pages follow the appflowy_export_space layout."
    )]
    async fn appflowy_export_workspace(
        &self,
        Parameters(params): Parameters<ExportWorkspaceParams>,
    ) -> std::result::Result<Json<Value>, ErrorData> {
        let directory = prepare_destination_directory(&params.request.path)
            .map_err(map_tool_error)?;
        let run = async {
            let params_qs = [("depth", "10".to_string())];
            let root = self
                .client
                .request(
                    Method::GET,
                    &format!("/api/workspace/{}/folder", params.workspace_id),
                    Some(&params_qs),
                    None,
                )
                .await?;
            let children: &[Value] = root
                .get("children")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let mut exported = Vec::new();
            let mut warnings = Vec::new();
            export_views_to_directory(
                &self.client,
                &params.workspace_id,
                children,
                &directory,
                &mut exported,
                &mut warnings,
            )
            .await?;
            Ok::<Value, Error>(json!({
                "workspace_id": params.workspace_id,
                "path": directory.display().to_string(),
                "exported_files": exported,
                "warnings": warnings,
            }))
        };
        run.await
            .map(Json)
            .map_err(|e| export_error("workspace", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_collab_bytes_normalizes_variants() {
        assert_eq!(
            encoded_collab_bytes(&json!([72, 105])),
            Some(b"Hi".to_vec())
        );
        // EncodedCollab {state_vector, doc_state} with a list doc_state.
        assert_eq!(
            encoded_collab_bytes(&json!({"state_vector": [0], "doc_state": [72, 105]})),
            Some(b"Hi".to_vec())
        );
        // doc_state as a base64 string.
        assert_eq!(
            encoded_collab_bytes(&json!({"doc_state": "SGk="})),
            Some(b"Hi".to_vec())
        );
        // Top-level base64 string (Python `_encoded_collab_bytes(b64)`).
        assert_eq!(encoded_collab_bytes(&json!("aGk=")), Some(b"hi".to_vec()));
        // falsy doc_state falls back to encoded_collab_v1 (Python `or`).
        assert_eq!(
            encoded_collab_bytes(&json!({"doc_state": [], "encoded_collab_v1": [72, 105]})),
            Some(b"Hi".to_vec())
        );
        assert_eq!(encoded_collab_bytes(&json!([])), None);
        assert_eq!(encoded_collab_bytes(&json!(42)), None);
        assert_eq!(encoded_collab_bytes(&json!({})), None);
        // out-of-range byte values reject the whole payload.
        assert_eq!(encoded_collab_bytes(&json!([256])), None);
    }

    #[test]
    fn markdown_body_emptiness_matches_python() {
        assert!(markdown_body_is_empty("# Title", Some("Title")));
        assert!(markdown_body_is_empty("  # Title\n\n  ", Some("Title")));
        assert!(!markdown_body_is_empty("# Title\n\nbody", Some("Title")));
        assert!(!markdown_body_is_empty("# Title", Some("Other")));
        assert!(markdown_body_is_empty("", None));
        // Empty title is falsy in Python (`if title:`) -> heading not stripped.
        assert!(!markdown_body_is_empty("# Title", Some("")));
    }

    #[test]
    fn collect_document_views_flattens_to_documents() {
        let views = json!([
            {"view_id": "s", "name": "Space", "is_space": true, "layout": 0,
             "children": [
                {"view_id": "p1", "name": "Doc", "layout": 0},
                {"view_id": "g", "name": "Grid", "layout": 2},
                {"view_id": "", "name": "NoId", "layout": 0},
             ]},
        ]);
        let docs = collect_document_views(views.as_array().unwrap());
        let ids: Vec<&str> = docs
            .iter()
            .filter_map(|v| v.get("view_id").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, ["p1"]);
    }

    #[test]
    fn destination_directory_rejects_non_empty_before_creating() {
        let base = std::env::temp_dir().join(format!(
            "appflowy-export-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("existing.txt"), "x").unwrap();
        let err = prepare_destination_directory(base.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, Error::Validation(ref m) if m.contains("Destination directory is not empty")));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn export_walks_spaces_and_skips_non_documents_offline() {
        // No document views -> prefetch does no network IO.
        let views = json!([
            {"view_id": "s", "name": "My Space", "is_space": true, "layout": 0,
             "children": [
                {"view_id": "g", "name": "Board", "is_space": false, "layout": 3},
             ]},
        ]);
        let base = std::env::temp_dir().join(format!(
            "appflowy-export-walk-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let client = AppFlowyClient::new(None, None, "http://127.0.0.1:1".to_string());
        let mut exported = Vec::new();
        let mut warnings = Vec::new();
        export_views_to_directory(
            &client,
            "ws",
            views.as_array().unwrap(),
            &base,
            &mut exported,
            &mut warnings,
        )
        .await
        .unwrap();
        assert!(base.join("My Space").is_dir());
        assert!(exported.is_empty());
        assert_eq!(
            warnings,
            vec!["skipped non-document view 'Board' (layout 3)".to_string()]
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn fetch_page_markdown_failure_reports_all_attempts() {
        let server = httpmock::MockServer::start_async().await;
        for path in [
            "/api/workspace/ws/page-view/p",
            "/api/workspace/v1/ws/collab/p",
        ] {
            server
                .mock_async(|when, then| {
                    when.method(httpmock::Method::GET).path(path);
                    then.status(404)
                        .json_body(json!({"code": 1000, "message": "Collab not found"}));
                })
                .await;
        }
        let client = AppFlowyClient::new(Some("e".into()), Some("p".into()), server.base_url());
        client.set_access_token(Some("t".into())).await;
        client.set_refresh_token(Some("r".into())).await;
        let err = fetch_page_markdown(&client, "ws", "p", Some("T"), 1, 0.0)
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Could not fetch page p"), "{message}");
        assert!(message.contains("root cause:"), "{message}"); // last page-view error kept
        assert!(message.contains("page-view GET"), "{message}");
        assert!(message.contains("attempt 1"), "{message}");
        assert!(message.contains("attempt 2"), "{message}"); // retries honored
        assert!(message.contains("collab_type=0"), "{message}");
        assert!(message.contains("collab_type=Document"), "{message}");
    }
}

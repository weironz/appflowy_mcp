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
//!   validate the destination directory (as a tool execution error) *before* any
//!   network request — Python rewrapped that check as a generic error from
//!   inside its own `try` block.
//!
//! # Incremental export
//!
//! `export_views_to_directory` writes a `.appflowy-export-manifest.json`
//! into the target directory recording every exported page's
//! `last_edited_time`, the file it was written to, and its title. A later
//! export into the same directory skips a page entirely — no page-view
//! request, no file write — when the manifest record, the current
//! `last_edited_time` and the computed target path all agree and the file
//! is still on disk. The incremental pass is conservative: it never
//! deletes files, so stale or foreign files in the directory are left in
//! place, and anything the manifest cannot vouch for is re-exported. A
//! destination without a usable manifest must still be empty, as before;
//! a destination holding a valid manifest for the same workspace is
//! accepted as an incremental re-export.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use percent_encoding::percent_decode_str;
use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_router, Json, RoleServer};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::b64::base64_decode;
use crate::client::AppFlowyClient;
use crate::collab::decode_document;
use crate::error::{Error, Result};
use crate::render::{document_to_markdown, sanitize_filename};
use crate::server::AppServer;
use crate::tools::util::{map_tool_error, unique_path, ToolError, ToolProgress};

/// Upper bound on concurrent page fetches during a tree export.
/// Python: `EXPORT_MAX_WORKERS = 8`.
pub const EXPORT_MAX_WORKERS: usize = 8;

/// Python `fetch_page_markdown(..., retries=2, backoff=0.4)` defaults.
pub const DEFAULT_PAGE_RETRIES: u32 = 2;
pub const DEFAULT_PAGE_BACKOFF_SECS: f64 = 0.4;

/// Depth of the `/folder` tree fetch for space/workspace exports. The API
/// truncates deeper subtrees, and those pages silently disappear from the
/// export; the constant is surfaced in the tool's warnings so the limit is
/// at least visible.
pub const EXPORT_TREE_DEPTH: &str = "10";

/// Per-page outcome of a prefetch: rendered Markdown or the error message.
/// Python: `dict[str, str | Exception]`.
pub type PrefetchCache = HashMap<String, std::result::Result<String, String>>;

/// Manifest file maintained at the root of an export target directory so a
/// later export of the same tree can skip unchanged pages.
pub const MANIFEST_FILE_NAME: &str = ".appflowy-export-manifest.json";
pub const MANIFEST_VERSION: u32 = 1;

/// Fingerprint of one exported page: the `last_edited_time` seen at export
/// time, the Markdown file it was written to (relative to the export
/// directory), and its title (diagnostic only — the path already encodes
/// the name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPage {
    pub last_edited_time: String,
    pub path: String,
    pub title: String,
}

/// On-disk `.appflowy-export-manifest.json`. A second export of the same
/// tree consults this to skip pages whose `last_edited_time` and computed
/// target path are unchanged and whose file is still on disk. A missing,
/// unparseable, foreign or stale-version manifest always degrades to a
/// full export; files not referenced by the manifest are never deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportManifest {
    pub tool: String,
    pub manifest_version: u32,
    pub workspace_id: String,
    #[serde(default)]
    pub pages: BTreeMap<String, ManifestPage>,
}

impl ExportManifest {
    fn empty(workspace_id: &str) -> Self {
        Self {
            tool: "appflowy".to_string(),
            manifest_version: MANIFEST_VERSION,
            workspace_id: workspace_id.to_string(),
            pages: BTreeMap::new(),
        }
    }

    /// Only trust a manifest written by this tool, at the supported
    /// version, for the workspace being exported.
    pub fn is_usable_for(&self, workspace_id: &str) -> bool {
        self.tool == "appflowy"
            && self.manifest_version == MANIFEST_VERSION
            && self.workspace_id == workspace_id
    }
}

/// Load `directory`'s manifest; any failure (missing file, bad JSON,
/// foreign workspace) yields an empty manifest, i.e. a full export.
fn load_manifest(
    directory: &Path,
    workspace_id: &str,
    warnings: &mut Vec<String>,
) -> ExportManifest {
    let text = match std::fs::read_to_string(directory.join(MANIFEST_FILE_NAME)) {
        Ok(text) => text,
        Err(_) => return ExportManifest::empty(workspace_id),
    };
    match serde_json::from_str::<ExportManifest>(&text) {
        Ok(manifest) if manifest.is_usable_for(workspace_id) => manifest,
        Ok(_) | Err(_) => {
            warnings.push(
                "existing export manifest is invalid or from another workspace; \
                 exporting everything"
                    .to_string(),
            );
            ExportManifest::empty(workspace_id)
        }
    }
}

/// Persist the manifest after an export. Failure only costs the next
/// export its incremental hits, so it degrades to a warning, never an
/// error.
fn save_manifest(
    directory: &Path,
    workspace_id: &str,
    pages: BTreeMap<String, ManifestPage>,
) -> std::io::Result<()> {
    let manifest = ExportManifest {
        tool: "appflowy".to_string(),
        manifest_version: MANIFEST_VERSION,
        workspace_id: workspace_id.to_string(),
        pages,
    };
    let json = serde_json::to_string_pretty(&manifest).map_err(std::io::Error::other)?;
    std::fs::write(directory.join(MANIFEST_FILE_NAME), json)
}

/// Lets the non-empty-destination validation pass when the directory
/// recognizably holds a previous export of the same workspace.
fn directory_has_export_manifest(directory: &Path, workspace_id: &str) -> bool {
    std::fs::read_to_string(directory.join(MANIFEST_FILE_NAME))
        .ok()
        .and_then(|text| serde_json::from_str::<ExportManifest>(&text).ok())
        .map(|manifest| manifest.is_usable_for(workspace_id))
        .unwrap_or(false)
}

/// Best-effort `last_edited_time` of a folder-tree view: top-level field
/// (string or numeric) first, then the JSON blob carried in `extra`.
/// `None` (a server that omits the field) disables skipping for the view —
/// it is always re-exported, never falsely skipped.
pub fn view_last_edited_time(view: &Value) -> Option<String> {
    fn as_timestamp(value: &Value) -> Option<String> {
        match value {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }
    if let Some(ts) = view.get("last_edited_time").and_then(as_timestamp) {
        return Some(ts);
    }
    view.get("extra")
        .and_then(Value::as_str)
        .and_then(|extra| serde_json::from_str::<Value>(extra).ok())
        .and_then(|extra| as_timestamp(extra.get("last_edited_time")?))
}

/// Mirrors Python `models.py::ExportPageRequest`.
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ExportPageRequest {
    /// Local destination file path for the Markdown export. '.md' is appended if missing. Parent directories are created.
    pub path: String,
}

/// Mirrors Python `models.py::ExportTreeRequest`.
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct ExportTreeRequest {
    /// Local destination directory. Created if missing; must be empty or not yet exist so existing files are never overwritten — unless it already holds a previous export's manifest for the same workspace, in which case unchanged pages are skipped (incremental export).
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

/// Options of `appflowy_export_pdf` (server-side PDF rendering of one page).
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ExportPdfRequest {
    /// Local destination file for the PDF. '.pdf' is appended if the suffix
    /// differs; parent directories are created; an existing file is never
    /// overwritten (' (n)' appended instead).
    pub path: String,
    /// Render nested child pages into the same PDF. Default true.
    pub include_nested: Option<bool>,
    /// Render database views (grids/boards). Default true.
    pub include_database: Option<bool>,
    /// Embed images in the PDF. Default true.
    pub include_images: Option<bool>,
    /// Maximum nesting depth to render. Default 2.
    pub max_depth: Option<u32>,
}

/// Parameters of `appflowy_export_pdf`.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ExportPdfParams {
    /// Workspace containing the page.
    pub workspace_id: String,
    /// View id of the page to render as PDF.
    pub page_id: String,
    /// Export options (`path` is required).
    pub request: ExportPdfRequest,
}

/// Options of `appflowy_export_workspace_zip`.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ExportWorkspaceZipRequest {
    /// Include uploaded file attachments in the archive. Default true.
    pub include_file_attachments: Option<bool>,
}

/// Parameters of `appflowy_export_workspace_zip`.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ExportWorkspaceZipParams {
    /// Workspace to back up.
    pub workspace_id: String,
    /// Backup options.
    pub request: ExportWorkspaceZipRequest,
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
    let bytes = encoded_collab_bytes(encoded).ok_or_else(|| {
        Error::Other("Page response encoded_collab is not a byte payload.".into())
    })?;
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
        let data = match client
            .request(Method::GET, &path, Some(&params), None)
            .await
        {
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
    // The empty markdown from the LAST successful attempt; an empty result is
    // only trustworthy when nothing errored after it — otherwise a transient
    // empty render followed by real failures would pin an empty file (and the
    // unchanged last_edited_time in the manifest would make the emptiness
    // permanent for incremental re-exports).
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
                // An error after an earlier empty render invalidates it: keep
                // the error, drop the stale empty result.
                page_markdown = None;
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
        return Ok(md); // the LAST attempt rendered an empty document
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
        !view
            .get("is_space")
            .and_then(Value::as_bool)
            .unwrap_or(false)
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
    prefetch_docs(client, workspace_id, &docs, &ToolProgress::inactive()).await
}

/// Concurrency core of [`prefetch_page_markdowns`], parameterized on the
/// explicit page list so incremental exports can prefetch only the pages
/// that actually changed. One progress tick fires per joined page fetch
/// (`exporting done/total pages`); an inert `progress` (no client token)
/// keeps the loop notification-free.
async fn prefetch_docs(
    client: &AppFlowyClient,
    workspace_id: &str,
    docs: &[(String, String)],
    progress: &ToolProgress,
) -> PrefetchCache {
    let mut cache = HashMap::new();
    if docs.is_empty() {
        return cache;
    }
    let workers = EXPORT_MAX_WORKERS.min(docs.len());
    let semaphore = Arc::new(Semaphore::new(workers));
    let mut set = JoinSet::new();
    for (view_id, name) in docs.iter().cloned() {
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
    let total = docs.len();
    let mut done = 0usize;
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((view_id, result)) => {
                cache.insert(view_id, result);
            }
            Err(e) => tracing::warn!(%e, "prefetch task failed to join"),
        }
        done += 1;
        progress.report(done, total, "exporting").await;
    }
    cache
}

/// Export plan for one view: where it lands on disk, whether the manifest
/// says it is unchanged (skip fetch + write), and the record to carry into
/// the next manifest. Built up front by [`plan_views`] so the prefetch
/// only covers pages that actually need (re)exporting.
struct PagePlan<'a> {
    view: &'a Value,
    /// Subdirectory holding the view's exported children (spaces and pages
    /// with children); `None` for leaves.
    subdir: Option<PathBuf>,
    /// File this document view's content is written to (leaf `.md` or a
    /// page-with-children's `README.md`); `None` for spaces.
    file: Option<PathBuf>,
    /// Manifest, `last_edited_time` and the on-disk file all agree — no
    /// fetch, no write for this page.
    skip: bool,
    /// Manifest record to write after a successful export of this page
    /// (unchanged for `skip`).
    record: Option<ManifestPage>,
    children: Vec<PagePlan<'a>>,
}

/// `" (n)"` variants of `path` (same naming as [`unique_path`]) until one
/// is neither already planned this run nor present on disk.
fn free_variant(path: &Path, occupancy: &HashSet<PathBuf>) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let suffix = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let mut counter = 2u32;
    loop {
        let candidate = path.with_file_name(format!("{stem} ({counter}){suffix}"));
        if !occupancy.contains(&candidate) && !candidate.exists() {
            return candidate;
        }
        counter += 1;
    }
}

/// Plan the whole export without any network IO: mirror the old recursive
/// walk's naming (page with children -> folder + `README.md`, leaf page ->
/// `<name>.md`) and decide per page whether the previous export already
/// covers it. `occupancy` tracks paths this run will touch so two
/// same-named siblings still land on distinct " (n)" names — kept
/// deterministic so a re-export re-plans the same paths.
fn plan_views<'a>(
    views: &'a [Value],
    directory: &Path,
    base: &Path,
    manifest: &ExportManifest,
    occupancy: &mut HashSet<PathBuf>,
    warnings: &mut Vec<String>,
) -> Vec<PagePlan<'a>> {
    let mut plans = Vec::new();
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
            // Keep descending: document pages nested under a grid/board
            // must not disappear with their parent (the legacy walk
            // flattened through non-document nodes too).
            plans.extend(plan_views(
                children, directory, base, manifest, occupancy, warnings,
            ));
            continue;
        }

        let is_space = view
            .get("is_space")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Subdirectory for the children: reuse one from a previous export
        // instead of unique_path's " (n)" fork.
        let mut subdir = None;
        if !children.is_empty() {
            let natural = directory.join(&filename);
            let dir = if occupancy.contains(&natural) || natural.is_file() {
                free_variant(&natural, occupancy)
            } else {
                natural
            };
            occupancy.insert(dir.clone());
            subdir = Some(dir);
        }

        let mut file = None;
        let mut skip = false;
        let mut record = None;
        if !is_space {
            let view_id = view.get("view_id").and_then(Value::as_str);
            let timestamp = view_last_edited_time(view);
            let old = view_id.and_then(|id| manifest.pages.get(id));
            let natural_file = match &subdir {
                Some(dir) => dir.join("README.md"),
                None => directory.join(format!("{filename}.md")),
            };
            let target = if occupancy.contains(&natural_file) {
                free_variant(&natural_file, occupancy)
            } else if natural_file.exists()
                && old.is_none_or(|old| base.join(&old.path) != natural_file)
            {
                // A file sits there that isn't this page's previous export
                // (renamed/moved page or a foreign file) — never overwrite
                // it, fork to a " (n)" name instead.
                free_variant(&natural_file, occupancy)
            } else {
                natural_file
            };
            occupancy.insert(target.clone());
            // Skip only when every fingerprint agrees: a non-empty
            // last_edited_time matching the manifest, the same target path
            // (a rename/move changes it and re-exports), and the previously
            // written file still present.
            skip = old.is_some_and(|old| {
                !old.last_edited_time.is_empty()
                    && timestamp.as_deref() == Some(old.last_edited_time.as_str())
                    && base.join(&old.path) == target
                    && target.is_file()
            });
            if view_id.is_some() {
                record = Some(ManifestPage {
                    last_edited_time: timestamp.unwrap_or_default(),
                    path: target
                        .strip_prefix(base)
                        .unwrap_or(&target)
                        .to_string_lossy()
                        .into_owned(),
                    title: name.to_string(),
                });
            }
            file = Some(target);
        }

        let child_plans = match &subdir {
            Some(dir) => plan_views(children, dir, base, manifest, occupancy, warnings),
            None => Vec::new(),
        };
        plans.push(PagePlan {
            view,
            subdir,
            file,
            skip,
            record,
            children: child_plans,
        });
    }
    plans
}

/// View ids + titles of the pages a planned export run actually has to
/// fetch — every document page except the ones skipped as unchanged.
fn collect_planned_fetches(plans: &[PagePlan<'_>]) -> Vec<(String, String)> {
    fn walk(plans: &[PagePlan<'_>], out: &mut Vec<(String, String)>) {
        for plan in plans {
            let is_space = plan
                .view
                .get("is_space")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !is_space && !plan.skip {
                if let Some(view_id) = plan
                    .view
                    .get("view_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    let name = plan
                        .view
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    out.push((view_id.to_string(), name.to_string()));
                }
            }
            walk(&plan.children, out);
        }
    }
    let mut docs = Vec::new();
    walk(plans, &mut docs);
    docs
}

/// Execute the plans built by [`plan_views`]: create folders, write the
/// fetched Markdown, count skips and rebuild the manifest pages. Every
/// per-view failure becomes a warning, same as the Python original.
/// `async fn` can't recurse, hence the boxed future.
#[allow(clippy::too_many_arguments)]
fn execute_page_plans<'a>(
    client: &'a AppFlowyClient,
    workspace_id: &'a str,
    plans: &'a [PagePlan<'a>],
    exported: &'a mut Vec<String>,
    warnings: &'a mut Vec<String>,
    skipped: &'a mut usize,
    cache: &'a PrefetchCache,
    pages: &'a mut BTreeMap<String, ManifestPage>,
) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        for plan in plans {
            let view = plan.view;
            let name = view.get("name").and_then(Value::as_str).unwrap_or_default();
            if let Some(dir) = &plan.subdir {
                // Kept from a previous export or freshly created now.
                if !dir.exists() {
                    if let Err(e) = std::fs::create_dir(dir) {
                        warnings.push(format!("failed to create folder for '{name}': {e}"));
                        continue;
                    }
                }
            }

            let is_space = view
                .get("is_space")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !is_space {
                match view.get("view_id").and_then(Value::as_str) {
                    Some(view_id) => {
                        if plan.skip {
                            *skipped += 1;
                            if let Some(record) = &plan.record {
                                pages.insert(view_id.to_string(), record.clone());
                            }
                        } else {
                            let markdown = match cache.get(view_id) {
                                Some(Err(e)) => {
                                    warnings.push(format!("failed to export '{name}': {e}"));
                                    None
                                }
                                Some(Ok(md)) => Some(md.clone()),
                                // Prefetch gaps (join failure) fall back to
                                // a direct fetch, as before.
                                None => match fetch_page_markdown(
                                    client,
                                    workspace_id,
                                    view_id,
                                    Some(name),
                                    DEFAULT_PAGE_RETRIES,
                                    DEFAULT_PAGE_BACKOFF_SECS,
                                )
                                .await
                                {
                                    Ok(md) => Some(md),
                                    Err(e) => {
                                        warnings.push(format!("failed to export '{name}': {e}"));
                                        None
                                    }
                                },
                            };
                            if let (Some(md), Some(file)) = (markdown, &plan.file) {
                                match std::fs::write(file, md) {
                                    Ok(()) => {
                                        exported.push(file.display().to_string());
                                        if let Some(record) = &plan.record {
                                            pages.insert(view_id.to_string(), record.clone());
                                        }
                                    }
                                    Err(e) => {
                                        warnings.push(format!(
                                            "failed to write '{}': {e}",
                                            file.display()
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    // Python would crash the whole export with a KeyError
                    // here; record a warning and carry on instead.
                    None => warnings.push(format!("failed to export '{name}': missing view_id")),
                }
            }

            execute_page_plans(
                client,
                workspace_id,
                &plan.children,
                exported,
                warnings,
                skipped,
                cache,
                pages,
            )
            .await;
        }
    })
}

/// Port of Python `export_views_to_directory`: mirror a view tree into a
/// directory of Markdown files — a page with children becomes a folder
/// whose own content is `README.md`, leaf pages become `.md` files.
///
/// The export is incremental: a `.appflowy-export-manifest.json` at the
/// directory root records each page's `last_edited_time` and written path,
/// and a re-export skips pages whose timestamp, computed path and on-disk
/// file all match (no page-view request, no file write). Only the pages
/// that need (re)exporting are prefetched, in parallel; the walk then just
/// reads from the cache. The incremental pass never deletes files, so
/// stale or foreign files are left in place. Every per-view failure
/// becomes a warning (never aborts the export), so this only errors on
/// nothing — the `Result` keeps the signature future-proof for callers.
/// File IO is synchronous `std::fs`: the payloads are small Markdown files
/// and it keeps the walk simple.
pub async fn export_views_to_directory(
    client: &AppFlowyClient,
    workspace_id: &str,
    views: &[Value],
    directory: &Path,
    exported: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    export_views_to_directory_stats(
        client,
        workspace_id,
        views,
        directory,
        exported,
        warnings,
        None,
        &ToolProgress::inactive(),
    )
    .await
}

/// [`export_views_to_directory`] plus a `skipped_unchanged` count: how many
/// pages were left untouched because the manifest showed them unchanged.
/// `progress` feeds one `notifications/progress` per prefetched page (pages
/// skipped as unchanged never fetched, so they never tick); pass
/// `&ToolProgress::inactive()` when the client asked for none.
#[allow(clippy::too_many_arguments)]
pub async fn export_views_to_directory_stats(
    client: &AppFlowyClient,
    workspace_id: &str,
    views: &[Value],
    directory: &Path,
    exported: &mut Vec<String>,
    warnings: &mut Vec<String>,
    skipped: Option<&mut usize>,
    progress: &ToolProgress,
) -> Result<()> {
    let manifest = load_manifest(directory, workspace_id, warnings);
    let mut occupancy = HashSet::new();
    let plans = plan_views(
        views,
        directory,
        directory,
        &manifest,
        &mut occupancy,
        warnings,
    );
    let docs = collect_planned_fetches(&plans);
    let cache = prefetch_docs(client, workspace_id, &docs, progress).await;
    let mut skipped_count = 0usize;
    let mut pages = BTreeMap::new();
    execute_page_plans(
        client,
        workspace_id,
        &plans,
        exported,
        warnings,
        &mut skipped_count,
        &cache,
        &mut pages,
    )
    .await;
    if let Err(e) = save_manifest(directory, workspace_id, pages) {
        warnings.push(format!("failed to write export manifest: {e}"));
    }
    if let Some(out) = skipped {
        *out = skipped_count;
    }
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
/// the not-empty check surfaces as `Error::Validation` (an isError tool result)
/// instead of Python's post-hoc generic rewrap. A non-empty directory
/// holding a previous export's manifest for the same workspace is
/// accepted: that is the incremental-export case, and it still never
/// deletes or blindly overwrites foreign files.
fn prepare_destination_directory(raw: &str, workspace_id: &str) -> Result<PathBuf> {
    let directory = expand_tilde(raw);
    if directory.exists() {
        let mut entries = std::fs::read_dir(&directory).map_err(|e| {
            Error::Other(format!(
                "Failed to read destination directory {}: {e}",
                directory.display()
            ))
        })?;
        if entries.next().is_some() && !directory_has_export_manifest(&directory, workspace_id) {
            return Err(Error::Validation(format!(
                "Destination directory is not empty: {}",
                directory.display()
            )));
        }
    }
    std::fs::create_dir_all(&directory).map_err(|e| {
        Error::Other(format!(
            "Failed to create directory {}: {e}",
            directory.display()
        ))
    })?;
    Ok(directory)
}

/// Wrap tool-body failures the way Python did:
/// `raise Exception(f"Failed to export {what}: {e}")`.
fn export_error(what: &str, e: Error) -> ToolError {
    ToolError::internal(format!("Failed to export {what}: {e}"))
}

/// RFC 6266 Content-Disposition filename: the extended form
/// `filename*=UTF-8''<percent-encoded>` wins; falls back to the plain
/// `filename="..."` parameter. `None` when neither is usable.
fn content_disposition_filename(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let value = headers
        .get(reqwest::header::CONTENT_DISPOSITION)?
        .to_str()
        .ok()?;
    let mut plain: Option<String> = None;
    for part in value.split(';').map(str::trim) {
        let Some((key, raw)) = part.split_once('=') else {
            continue;
        };
        let raw = raw.trim().trim_matches('"');
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                let encoded = raw
                    .strip_prefix("UTF-8''")
                    .or_else(|| raw.strip_prefix("utf-8''"))
                    .unwrap_or(raw);
                if let Ok(decoded) = percent_decode_str(encoded).decode_utf8() {
                    let decoded = decoded.trim();
                    if !decoded.is_empty() {
                        return Some(decoded.to_string());
                    }
                }
            }
            "filename" if !raw.is_empty() => plain = Some(raw.to_string()),
            _ => {}
        }
    }
    plain
}

#[tool_router(router = export_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_export_page",
        description = "Export a single page as a local Markdown file, preserving inline formatting (bold, italic, links, code). Child pages are not included; use appflowy_export_space for a whole subtree.",
        annotations(title = "Export page", open_world_hint = true)
    )]
    async fn appflowy_export_page(
        &self,
        Parameters(params): Parameters<ExportPageParams>,
    ) -> std::result::Result<Json<Value>, ToolError> {
        let ExportPageParams {
            workspace_id,
            page_id,
            request,
        } = params;
        let run = async {
            // Single-page export always re-fetches: there is deliberately
            // no manifest on this path (one page, one file — the tree
            // export's skip bookkeeping wouldn't pay off).
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

            // A path that is empty after tilde expansion would resolve
            // `.file_name()` to None and silently write a hidden `.md` into
            // the process CWD (the PDF export already rejects this).
            let expanded = expand_tilde(&request.path);
            if expanded.as_os_str().is_empty() {
                return Err(Error::Validation(
                    "path is required: pass a file path ending in .md (or no extension).".into(),
                ));
            }
            let markdown = fetch_page_markdown(
                &self.client,
                &workspace_id,
                &page_id,
                Some(&name),
                DEFAULT_PAGE_RETRIES,
                DEFAULT_PAGE_BACKOFF_SECS,
            )
            .await?;

            let mut path = expanded;
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
        description = "Export a space (or any page subtree) to a local directory of Markdown files, mirroring appflowy_import_markdown_directory: a page with children becomes a folder whose own content is README.md, leaf pages become .md files. Non-document views (grids, boards) are skipped with warnings. Incremental: the directory keeps a .appflowy-export-manifest.json; re-exporting into it skips pages unchanged since (tracked by last_edited_time) in skipped_unchanged, and never deletes older files.",
        annotations(title = "Export space", open_world_hint = true)
    )]
    async fn appflowy_export_space(
        &self,
        Parameters(params): Parameters<ExportSpaceParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<Json<Value>, ToolError> {
        let progress = ToolProgress::from_context(&context);
        // Destination validation is a parameter error and must fire before
        // any network request (Python rewrapped it as a generic error).
        let directory = prepare_destination_directory(&params.request.path, &params.workspace_id)
            .map_err(map_tool_error)?;
        let run = async {
            let params_qs = [
                ("depth", EXPORT_TREE_DEPTH.to_string()),
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
            // See the workspace export: deeper-nested pages are absent from
            // the fetched tree, so say so.
            warnings.push(format!(
                "note: the folder tree is fetched with depth {} — pages nested deeper than that are not exported",
                EXPORT_TREE_DEPTH
            ));
            let mut skipped_unchanged = 0usize;
            export_views_to_directory_stats(
                &self.client,
                &params.workspace_id,
                std::slice::from_ref(&root),
                &directory,
                &mut exported,
                &mut warnings,
                Some(&mut skipped_unchanged),
                &progress,
            )
            .await?;
            Ok::<Value, Error>(json!({
                "root_view_id": params.space_view_id,
                "path": directory.display().to_string(),
                "exported_files": exported,
                "skipped_unchanged": skipped_unchanged,
                "warnings": warnings,
            }))
        };
        run.await.map(Json).map_err(|e| export_error("space", e))
    }

    #[tool(
        name = "appflowy_export_workspace",
        description = "Export every space in a workspace to a local directory tree of Markdown files. Each space becomes a top-level folder; pages follow the appflowy_export_space layout. Incremental: the directory keeps a .appflowy-export-manifest.json; re-exporting into it skips pages unchanged since (tracked by last_edited_time) in skipped_unchanged, and never deletes older files.",
        annotations(title = "Export workspace", open_world_hint = true)
    )]
    async fn appflowy_export_workspace(
        &self,
        Parameters(params): Parameters<ExportWorkspaceParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<Json<Value>, ToolError> {
        let progress = ToolProgress::from_context(&context);
        let directory = prepare_destination_directory(&params.request.path, &params.workspace_id)
            .map_err(map_tool_error)?;
        let run = async {
            let params_qs = [("depth", EXPORT_TREE_DEPTH.to_string())];
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
            // Pages nested deeper than the folder fetch's depth simply do
            // not appear in the tree; surface that instead of silently
            // missing files.
            warnings.push(format!(
                "note: the folder tree is fetched with depth {} — pages nested deeper than that are not exported",
                EXPORT_TREE_DEPTH
            ));
            let mut skipped_unchanged = 0usize;
            export_views_to_directory_stats(
                &self.client,
                &params.workspace_id,
                children,
                &directory,
                &mut exported,
                &mut warnings,
                Some(&mut skipped_unchanged),
                &progress,
            )
            .await?;
            Ok::<Value, Error>(json!({
                "workspace_id": params.workspace_id,
                "path": directory.display().to_string(),
                "exported_files": exported,
                "skipped_unchanged": skipped_unchanged,
                "warnings": warnings,
            }))
        };
        run.await
            .map(Json)
            .map_err(|e| export_error("workspace", e))
    }

    #[tool(
        name = "appflowy_export_pdf",
        description = "Export a page as a PDF rendered by the AppFlowy server (commercial cloud). Server-side rendering preserves the original layout far more faithfully than appflowy_export_page's Markdown conversion (nested pages, databases, images). The binary PDF is saved to a local file; parent directories are created and an existing file is never overwritten (' (n)' appended).",
        annotations(title = "Export pdf", open_world_hint = true)
    )]
    async fn appflowy_export_pdf(
        &self,
        Parameters(params): Parameters<ExportPdfParams>,
    ) -> std::result::Result<Json<Value>, ToolError> {
        let ExportPdfParams {
            workspace_id,
            page_id,
            request,
        } = params;
        if workspace_id.is_empty() || page_id.is_empty() {
            return Err(ToolError::validation(
                "workspace_id and page_id (view_id of the page) must not be empty",
            ));
        }
        if request.path.is_empty() {
            return Err(ToolError::validation(
                "request.path (local destination file) must not be empty",
            ));
        }
        let query = [
            (
                "include_nested",
                request.include_nested.unwrap_or(true).to_string(),
            ),
            (
                "include_database",
                request.include_database.unwrap_or(true).to_string(),
            ),
            (
                "include_images",
                request.include_images.unwrap_or(true).to_string(),
            ),
            ("max_depth", request.max_depth.unwrap_or(2).to_string()),
        ];
        let response = self
            .client
            .request_bytes_ext(
                Method::POST,
                &format!("/api/export/view/{workspace_id}/{page_id}/pdf"),
                Some(&query),
                None,
                &[],
            )
            .await
            .map_err(map_tool_error)?;

        let filename = content_disposition_filename(&response.headers)
            .unwrap_or_else(|| format!("export-{page_id}.pdf"));

        let mut path = expand_tilde(&request.path);
        let is_pdf = path
            .extension()
            .map(|e| e.to_string_lossy().eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);
        if !is_pdf {
            if let Some(file_name) = path.file_name() {
                let mut file_name = file_name.to_os_string();
                file_name.push(".pdf");
                path.set_file_name(file_name);
            } else {
                path.push(".pdf");
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(Error::Io)
                .map_err(map_tool_error)?;
        }
        let path = unique_path(&path);
        std::fs::write(&path, &response.body)
            .map_err(Error::Io)
            .map_err(map_tool_error)?;

        Ok(Json(json!({
            "view_id": page_id,
            "filename": filename,
            "path": path.display().to_string(),
            "bytes_written": response.body.len(),
        })))
    }

    #[tool(
        name = "appflowy_export_workspace_zip",
        description = "Request a full-workspace ZIP backup from the AppFlowy server (commercial cloud). Owner-only endpoint: the archive is built asynchronously and the server emails the workspace owner a download link when it is ready — this call returns the task id, not the archive itself.",
        annotations(title = "Export workspace zip", open_world_hint = true)
    )]
    async fn appflowy_export_workspace_zip(
        &self,
        Parameters(params): Parameters<ExportWorkspaceZipParams>,
    ) -> std::result::Result<Json<Value>, ToolError> {
        if params.workspace_id.is_empty() {
            return Err(ToolError::validation("workspace_id must not be empty"));
        }
        // The AppFlowy gateway requires the deployment's base URL in X-Host.
        let base_url = self.client.base_url().await;
        let data = self
            .client
            .request_ext(
                Method::POST,
                &format!("/api/export/workspace/{}", params.workspace_id),
                None,
                Some(json!({
                    "include_file_attachments": params
                        .request
                        .include_file_attachments
                        .unwrap_or(true),
                })),
                &[("X-Host", base_url.as_str())],
            )
            .await
            .map_err(map_tool_error)?;
        let task_id = data.get("task_id").cloned().ok_or_else(|| {
            // An envelope without task_id is not a task handle; returning the
            // whole response as the "id" would mislead callers into polling
            // with an object.
            ToolError::internal(
                "workspace zip export response did not include task_id; full response: {data}",
            )
        })?;
        Ok(Json(json!({
            "task_id": task_id,
            "note": "The server builds the archive asynchronously and emails the workspace owner a download link when it is ready. Owner-only endpoint.",
        })))
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
        let err = prepare_destination_directory(base.to_str().unwrap(), "ws").unwrap_err();
        assert!(
            matches!(err, Error::Validation(ref m) if m.contains("Destination directory is not empty"))
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn destination_directory_with_manifest_allows_incremental_reexport() {
        let base = std::env::temp_dir().join(format!(
            "appflowy-export-test-manifest-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        // A previous export: one Markdown file plus the manifest.
        std::fs::write(base.join("Page.md"), "# P").unwrap();
        let manifest = ExportManifest {
            tool: "appflowy".to_string(),
            manifest_version: MANIFEST_VERSION,
            workspace_id: "ws".to_string(),
            pages: BTreeMap::new(),
        };
        std::fs::write(
            base.join(MANIFEST_FILE_NAME),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        prepare_destination_directory(base.to_str().unwrap(), "ws")
            .expect("manifest directory is reusable");
        // A manifest for another workspace does not unlock the directory.
        let other = ExportManifest {
            workspace_id: "other".to_string(),
            ..ExportManifest::empty("other")
        };
        std::fs::write(
            base.join(MANIFEST_FILE_NAME),
            serde_json::to_string_pretty(&other).unwrap(),
        )
        .unwrap();
        let err = prepare_destination_directory(base.to_str().unwrap(), "ws").unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
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

    // ------------------------------------------------------------------
    // Incremental export: manifest bookkeeping + httpmock two-run tests.
    // The yrs document fixture mirrors tests/export.rs::collab_update.
    // ------------------------------------------------------------------

    use yrs::{ArrayPrelim, Doc, Map, MapPrelim, ReadTxn, StateVector, TextPrelim, Transact};

    /// Minimal document collab with one paragraph per string (encodes to
    /// the yjs update the page-view endpoint would carry).
    fn collab_update(texts: &[&str]) -> Vec<u8> {
        let doc = Doc::new();
        let data = doc.get_or_insert_map("data");
        let mut txn = doc.transact_mut();
        let document = data.insert(&mut txn, "document", MapPrelim::default());
        document.insert(&mut txn, "page_id", "root");
        let blocks = document.insert(&mut txn, "blocks", MapPrelim::default());
        let root = blocks.insert(&mut txn, "root", MapPrelim::default());
        root.insert(&mut txn, "ty", "page");
        root.insert(&mut txn, "data", "{}");
        root.insert(&mut txn, "children", "c_root");
        let ids: Vec<String> = (0..texts.len()).map(|i| format!("b{i}")).collect();
        for id in &ids {
            let block = blocks.insert(&mut txn, id.clone(), MapPrelim::default());
            block.insert(&mut txn, "ty", "paragraph");
            block.insert(&mut txn, "data", "{}");
            block.insert(&mut txn, "children", format!("c_{id}"));
            block.insert(&mut txn, "external_id", format!("t_{id}"));
        }
        let meta = document.insert(&mut txn, "meta", MapPrelim::default());
        let children_map = meta.insert(&mut txn, "children_map", MapPrelim::default());
        let root_children: ArrayPrelim = ids.iter().cloned().collect();
        children_map.insert(&mut txn, "c_root", root_children);
        for id in &ids {
            children_map.insert(&mut txn, format!("c_{id}"), ArrayPrelim::default());
        }
        let text_map = meta.insert(&mut txn, "text_map", MapPrelim::default());
        for (id, text) in ids.iter().zip(texts) {
            text_map.insert(&mut txn, format!("t_{id}"), TextPrelim::new(*text));
        }
        drop(txn);
        let update = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        update
    }

    /// Page-view response envelope: the client unwraps one `data` layer,
    /// `fetch_page_view_markdown` reads `data.encoded_collab` below it.
    fn page_view_body(update: &[u8]) -> Value {
        json!({"data": {"data": {"encoded_collab":
            update.iter().map(|b| *b as u64).collect::<Vec<u64>>()}}})
    }

    async fn httpmock_client(server: &httpmock::MockServer) -> AppFlowyClient {
        let client = AppFlowyClient::new(Some("e".into()), Some("p".into()), server.base_url());
        client.set_access_token(Some("t".into())).await;
        client.set_refresh_token(Some("r".into())).await;
        client
    }

    fn temp_export_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "appflowy-export-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    async fn mock_page_views<'a>(
        server: &'a httpmock::MockServer,
        pages: &[(&str, &str)],
    ) -> HashMap<String, httpmock::Mock<'a>> {
        let mut mocks = HashMap::new();
        for (view_id, text) in pages {
            let update = collab_update(&[text]);
            let path = format!("/api/workspace/ws/page-view/{view_id}");
            let mock = server
                .mock_async(move |when, then| {
                    when.method(httpmock::Method::GET).path(path);
                    then.status(200).json_body(page_view_body(&update));
                })
                .await;
            mocks.insert(view_id.to_string(), mock);
        }
        mocks
    }

    #[test]
    fn manifest_load_degrades_to_full_export_on_bad_input() {
        let dir = temp_export_dir("manifest-load");
        std::fs::create_dir_all(&dir).unwrap();
        // No manifest at all: empty manifest, no warning (normal first run).
        let mut warnings = Vec::new();
        assert_eq!(
            load_manifest(&dir, "ws", &mut warnings),
            ExportManifest::empty("ws")
        );
        assert!(warnings.is_empty());
        // Bad JSON: empty manifest + warning.
        std::fs::write(dir.join(MANIFEST_FILE_NAME), "{not json").unwrap();
        let mut warnings = Vec::new();
        assert_eq!(
            load_manifest(&dir, "ws", &mut warnings),
            ExportManifest::empty("ws")
        );
        assert_eq!(warnings.len(), 1);
        // Wrong workspace: empty manifest + warning.
        std::fs::write(
            dir.join(MANIFEST_FILE_NAME),
            serde_json::to_string(&ExportManifest {
                tool: "appflowy".to_string(),
                manifest_version: MANIFEST_VERSION,
                workspace_id: "other-ws".to_string(),
                pages: BTreeMap::new(),
            })
            .unwrap(),
        )
        .unwrap();
        let mut warnings = Vec::new();
        assert_eq!(
            load_manifest(&dir, "ws", &mut warnings),
            ExportManifest::empty("ws")
        );
        assert_eq!(warnings.len(), 1);
        // Matching manifest is trusted.
        let good = ExportManifest {
            tool: "appflowy".to_string(),
            manifest_version: MANIFEST_VERSION,
            workspace_id: "ws".to_string(),
            pages: BTreeMap::from([(
                "p".to_string(),
                ManifestPage {
                    last_edited_time: "1".to_string(),
                    path: "P.md".to_string(),
                    title: "P".to_string(),
                },
            )]),
        };
        std::fs::write(
            dir.join(MANIFEST_FILE_NAME),
            serde_json::to_string(&good).unwrap(),
        )
        .unwrap();
        let mut warnings = Vec::new();
        assert_eq!(load_manifest(&dir, "ws", &mut warnings), good);
        assert!(warnings.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn view_last_edited_time_prefers_top_level_then_extra() {
        assert_eq!(
            view_last_edited_time(&json!({"last_edited_time": "2025-01-01T00:00:00Z"})),
            Some("2025-01-01T00:00:00Z".to_string())
        );
        assert_eq!(
            view_last_edited_time(&json!({"last_edited_time": 1735689600})),
            Some("1735689600".to_string())
        );
        assert_eq!(
            view_last_edited_time(
                &json!({"extra": "{\"is_pinned\": true, \"last_edited_time\": \"t2\"}"})
            ),
            Some("t2".to_string())
        );
        // Missing or empty -> never skip, always re-export.
        assert_eq!(view_last_edited_time(&json!({})), None);
        assert_eq!(
            view_last_edited_time(&json!({"last_edited_time": ""})),
            None
        );
        assert_eq!(
            view_last_edited_time(&json!({"last_edited_time": null})),
            None
        );
    }

    /// The core incremental scenario: a first full export writes every
    /// page plus the manifest; a second export of the identical tree
    /// issues not a single page-view request and writes nothing.
    #[tokio::test]
    async fn incremental_export_skips_unchanged_pages_on_second_run() {
        let server = httpmock::MockServer::start_async().await;
        let pages = [("p1", "body-p1"), ("f1", "body-f1"), ("p2", "body-p2")];
        let mocks = mock_page_views(&server, &pages).await;
        let client = httpmock_client(&server).await;

        let views = json!([
            {"view_id": "s1", "name": "Space", "is_space": true, "layout": 0,
             "last_edited_time": "9001", "children": [
                {"view_id": "p1", "name": "Page1", "layout": 0, "last_edited_time": "1001", "children": []},
                {"view_id": "f1", "name": "Folder", "layout": 0, "last_edited_time": "1002", "children": [
                    {"view_id": "p2", "name": "Page2", "layout": 0, "last_edited_time": "1003", "children": []},
                ]},
            ]},
        ]);
        let dir = temp_export_dir("incremental");
        std::fs::create_dir_all(&dir).unwrap();

        // First run: everything is exported and prefetched.
        let mut exported = Vec::new();
        let mut warnings = Vec::new();
        let mut skipped = usize::MAX; // proves the stats out-param is written
        export_views_to_directory_stats(
            &client,
            "ws",
            views.as_array().unwrap(),
            &dir,
            &mut exported,
            &mut warnings,
            Some(&mut skipped),
            &ToolProgress::inactive(),
        )
        .await
        .unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(exported.len(), 3);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        for mock in mocks.values() {
            mock.assert_calls_async(1).await;
        }

        // The manifest records every page with timestamp and relative path.
        let manifest: ExportManifest =
            serde_json::from_str(&std::fs::read_to_string(dir.join(MANIFEST_FILE_NAME)).unwrap())
                .unwrap();
        assert_eq!(manifest.tool, "appflowy");
        assert_eq!(manifest.manifest_version, MANIFEST_VERSION);
        assert_eq!(manifest.workspace_id, "ws");
        assert_eq!(manifest.pages.len(), 3);
        let p1 = &manifest.pages["p1"];
        assert_eq!(p1.last_edited_time, "1001");
        assert_eq!(p1.title, "Page1");
        assert_eq!(Path::new(&p1.path), Path::new("Space").join("Page1.md"));
        let f1 = &manifest.pages["f1"];
        assert_eq!(f1.last_edited_time, "1002");
        assert_eq!(
            Path::new(&f1.path),
            Path::new("Space").join("Folder").join("README.md")
        );
        let p2 = &manifest.pages["p2"];
        assert_eq!(p2.last_edited_time, "1003");
        assert_eq!(
            Path::new(&p2.path),
            Path::new("Space").join("Folder").join("Page2.md")
        );

        // Second run with nothing changed: zero page-view hits, zero
        // writes, all three pages counted as skipped.
        let mut exported = Vec::new();
        let mut warnings = Vec::new();
        export_views_to_directory_stats(
            &client,
            "ws",
            views.as_array().unwrap(),
            &dir,
            &mut exported,
            &mut warnings,
            Some(&mut skipped),
            &ToolProgress::inactive(),
        )
        .await
        .unwrap();
        assert_eq!(skipped, 3);
        assert!(exported.is_empty(), "second run must not write files");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        // Still exactly one request per page — i.e. none on run two.
        for mock in mocks.values() {
            mock.assert_calls_async(1).await;
        }
        // Content landed and the manifest still names the same pages.
        assert!(std::fs::read_to_string(dir.join("Space").join("Page1.md"))
            .unwrap()
            .contains("body-p1"));
        assert!(
            std::fs::read_to_string(dir.join("Space").join("Folder").join("README.md"))
                .unwrap()
                .contains("body-f1")
        );
        assert!(
            std::fs::read_to_string(dir.join("Space").join("Folder").join("Page2.md"))
                .unwrap()
                .contains("body-p2")
        );
        let manifest: ExportManifest =
            serde_json::from_str(&std::fs::read_to_string(dir.join(MANIFEST_FILE_NAME)).unwrap())
                .unwrap();
        assert_eq!(manifest.pages.len(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Change detection: a bumped `last_edited_time` re-fetches in place;
    /// a rename (same timestamp, different computed path) re-fetches under
    /// the new name while the old file is left in place; an untouched page
    /// is skipped.
    #[tokio::test]
    async fn incremental_export_refetches_edited_or_renamed_pages() {
        let server = httpmock::MockServer::start_async().await;
        let pages = [
            ("keep", "body-keep"),
            ("edit", "body-edit"),
            ("rename", "body-rename"),
        ];
        let mocks = mock_page_views(&server, &pages).await;
        let client = httpmock_client(&server).await;

        let space = |children: Value| {
            json!([{"view_id": "s1", "name": "Space", "is_space": true, "layout": 0,
                    "children": children}])
        };
        let first_tree = space(json!([
            {"view_id": "keep", "name": "Keep", "layout": 0, "last_edited_time": "1", "children": []},
            {"view_id": "edit", "name": "Edit", "layout": 0, "last_edited_time": "10", "children": []},
            {"view_id": "rename", "name": "Old", "layout": 0, "last_edited_time": "20", "children": []},
        ]));
        let dir = temp_export_dir("incremental-refetch");
        std::fs::create_dir_all(&dir).unwrap();
        let mut exported = Vec::new();
        let mut warnings = Vec::new();
        let mut skipped = 0usize;
        export_views_to_directory_stats(
            &client,
            "ws",
            first_tree.as_array().unwrap(),
            &dir,
            &mut exported,
            &mut warnings,
            Some(&mut skipped),
            &ToolProgress::inactive(),
        )
        .await
        .unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(exported.len(), 3);

        // Second run: `edit` has a new timestamp, `rename` was retitled
        // (same timestamp, so only the path differs), `keep` is untouched.
        let second_tree = space(json!([
            {"view_id": "keep", "name": "Keep", "layout": 0, "last_edited_time": "1", "children": []},
            {"view_id": "edit", "name": "Edit", "layout": 0, "last_edited_time": "11", "children": []},
            {"view_id": "rename", "name": "New", "layout": 0, "last_edited_time": "20", "children": []},
        ]));
        let mut exported = Vec::new();
        let mut warnings = Vec::new();
        export_views_to_directory_stats(
            &client,
            "ws",
            second_tree.as_array().unwrap(),
            &dir,
            &mut exported,
            &mut warnings,
            Some(&mut skipped),
            &ToolProgress::inactive(),
        )
        .await
        .unwrap();
        assert_eq!(skipped, 1, "only 'keep' is unchanged");
        assert_eq!(exported.len(), 2, "edited and renamed pages re-written");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(
            mocks["keep"].calls_async().await,
            1,
            "keep was never refetched"
        );
        assert_eq!(
            mocks["edit"].calls_async().await,
            2,
            "edited page refetched"
        );
        assert_eq!(
            mocks["rename"].calls_async().await,
            2,
            "renamed page refetched"
        );

        // The edited page was overwritten in place; the renamed page got a
        // new file and the old one is deliberately left behind (incremental
        // export never deletes).
        assert!(std::fs::read_to_string(dir.join("Space").join("Edit.md"))
            .unwrap()
            .contains("body-edit"));
        assert!(dir.join("Space").join("New.md").is_file());
        assert!(
            dir.join("Space").join("Old.md").is_file(),
            "stale file must not be deleted"
        );

        let manifest: ExportManifest =
            serde_json::from_str(&std::fs::read_to_string(dir.join(MANIFEST_FILE_NAME)).unwrap())
                .unwrap();
        assert_eq!(manifest.pages["keep"].last_edited_time, "1");
        assert_eq!(manifest.pages["edit"].last_edited_time, "11");
        assert_eq!(manifest.pages["rename"].title, "New");
        assert_eq!(
            Path::new(&manifest.pages["rename"].path),
            Path::new("Space").join("New.md")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn content_disposition_filename_prefers_rfc6266_extended_form() {
        use reqwest::header::{HeaderMap, HeaderValue, CONTENT_DISPOSITION};

        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_DISPOSITION,
            HeaderValue::from_str(
                "attachment; filename=\"fallback.pdf\"; filename*=UTF-8''%E6%8A%A5%E8%A1%A8%20v2.pdf",
            )
            .unwrap(),
        );
        assert_eq!(
            content_disposition_filename(&headers).as_deref(),
            Some("报表 v2.pdf")
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_DISPOSITION,
            HeaderValue::from_str("attachment; filename=\"My Page.pdf\"").unwrap(),
        );
        assert_eq!(
            content_disposition_filename(&headers).as_deref(),
            Some("My Page.pdf")
        );

        assert_eq!(content_disposition_filename(&HeaderMap::new()), None);
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_DISPOSITION,
            HeaderValue::from_str("attachment").unwrap(),
        );
        assert_eq!(content_disposition_filename(&headers), None);
    }
}

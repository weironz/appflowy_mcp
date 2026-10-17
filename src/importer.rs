//! Markdown file/directory importer.
//!
//! Faithful async port of `appflowy_mcp/importer.py`, with one deliberate
//! in-place optimization: the Python version uploads each image serially
//! inside the markdown image-url resolver, which is the dominant bottleneck
//! when importing large directories. Here every page is processed in two
//! phases:
//!
//! 1. The raw markdown is scanned for image references (mirroring the exact
//!    line-gating of `parse_markdown_to_blocks`, so the scan sees precisely
//!    the URLs the resolver would be asked about). Local files are resolved,
//!    deduplicated by resolved absolute path, then uploaded concurrently via
//!    a `JoinSet` + `Semaphore` (at most [`MAX_CONCURRENT_UPLOADS`] in
//!    flight) into this page's `view_id` as `parent_dir`.
//! 2. `parse_markdown_to_blocks` runs with a resolver that is a pure lookup
//!    into the `original markdown url -> uploaded url` map built by phase 1.
//!
//! Page *creation* is parallelized per directory level in two phases:
//!
//! A. Every direct Markdown file of one directory runs the full
//!    read → image-upload → parse → POST /page-view chain concurrently
//!    (a `JoinSet` + `Semaphore` capped at [`MAX_CONCURRENT_PAGE_CREATES`] —
//!    more conservative than the upload cap, because each task performs a
//!    write and may run its own upload batch underneath). Per-task results
//!    are collected back by file index so merged summaries keep scan order.
//! B. AppFlowy fixes sibling order by creation order, which concurrency
//!    scrambles. If the completion order of the creation POSTs no longer
//!    matches the scan order, the actual order is read back from the folder
//!    tree and corrected with a minimal sequence of `/move` calls, each
//!    placing one page right after its intended predecessor. When every
//!    page happens to complete in order (the common case for small
//!    directories) the correction pass issues no request at all.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use regex::Regex;
use reqwest::Method;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::client::AppFlowyClient;
use crate::error::{Error, Result};
use crate::markdown::{parse_markdown_to_blocks, Block};

const MARKDOWN_SUFFIXES: [&str; 2] = [".md", ".markdown"];
const INDEX_FILENAMES: [&str; 2] = ["readme.md", "index.md"];
const IGNORED_DIRECTORIES: [&str; 7] = [
    ".git",
    ".hg",
    ".svn",
    ".idea",
    ".vscode",
    "node_modules",
    "__pycache__",
];

/// Upper bound on in-flight asset uploads for a single page.
const MAX_CONCURRENT_UPLOADS: usize = 8;

/// Upper bound on concurrently created sibling pages of one directory. Kept
/// well below [`MAX_CONCURRENT_UPLOADS`]: every page task performs a write
/// (POST /page-view) and may run its own upload batch on top of it.
const MAX_CONCURRENT_PAGE_CREATES: usize = 4;

// ---------------------------------------------------------------------------
// Report types (serde field order mirrors the Python dict insertion order)
// ---------------------------------------------------------------------------

/// One entry of `errors`: `{"path", "error"}`.
#[derive(Debug, Clone, Serialize)]
pub struct ImportError {
    pub path: String,
    pub error: String,
}

/// One entry of `uploaded_assets`, mirroring the Python dict.
#[derive(Debug, Clone, Serialize)]
pub struct UploadedAsset {
    pub source_path: String,
    pub markdown_url: String,
    pub caption: String,
    pub page_view_id: String,
    /// `upload.get("file_id")` — may be null, like Python's `dict.get`.
    pub file_id: Value,
    /// `upload.get("url")` — may be null, like Python's `dict.get`.
    pub url: Value,
    pub content_type: String,
    pub content_length: usize,
}

/// One entry of `created_pages` (also the `root_page` shape), mirroring the
/// Python `_create_page` entry dict.
#[derive(Debug, Clone, Serialize)]
pub struct CreatedPage {
    pub title: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub source_path: String,
    pub view_id: String,
    pub collab_id: String,
    pub parent_view_id: String,
    pub block_count: usize,
    pub asset_count: usize,
    pub page: Value,
}

/// Mirrors the Python `_summary` dict exactly.
#[derive(Debug, Clone, Serialize)]
pub struct ImportSummary {
    pub root_page: Option<CreatedPage>,
    pub created_page_count: usize,
    pub uploaded_asset_count: usize,
    pub error_count: usize,
    pub warning_count: usize,
    pub created_pages: Vec<CreatedPage>,
    pub uploaded_assets: Vec<UploadedAsset>,
    pub warnings: Vec<String>,
    pub errors: Vec<ImportError>,
}

// ---------------------------------------------------------------------------
// Importer
// ---------------------------------------------------------------------------

pub struct MarkdownImporter {
    pub client: AppFlowyClient,
    pub workspace_id: String,
    pub created_pages: Vec<CreatedPage>,
    pub uploaded_assets: Vec<UploadedAsset>,
    pub warnings: Vec<String>,
    pub errors: Vec<ImportError>,
}

impl MarkdownImporter {
    pub fn new(client: AppFlowyClient, workspace_id: impl Into<String>) -> Self {
        Self {
            client,
            workspace_id: workspace_id.into(),
            created_pages: Vec::new(),
            uploaded_assets: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub async fn import_file(
        &mut self,
        file_path: &str,
        parent_view_id: &str,
        title: Option<&str>,
        upload_assets: bool,
    ) -> Result<ImportSummary> {
        let path = resolve_user_path(file_path);
        if !path.is_file() {
            return Err(Error::Validation(format!(
                "Markdown file does not exist: {}",
                path.display()
            )));
        }
        if !MARKDOWN_SUFFIXES.contains(&lowercase_suffix(&path).as_str()) {
            return Err(Error::Validation(format!(
                "File is not a Markdown file: {}",
                path.display()
            )));
        }

        // Python `title or path.stem`: an empty title also falls back.
        let title = match title.filter(|t| !t.is_empty()) {
            Some(t) => t.to_string(),
            None => path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        let page = self
            .create_markdown_page(
                &path,
                parent_view_id,
                &title,
                upload_assets,
                "markdown_file",
            )
            .await;
        Ok(self.summary(page))
    }

    pub async fn import_directory(
        &mut self,
        directory_path: &str,
        parent_view_id: &str,
        upload_assets: bool,
    ) -> Result<ImportSummary> {
        let path = resolve_user_path(directory_path);
        if !path.is_dir() {
            return Err(Error::Validation(format!(
                "Markdown directory does not exist: {}",
                path.display()
            )));
        }

        if !subtree_has_markdown(&path) {
            self.warnings.push(format!(
                "No Markdown files found under {}; nothing was imported.",
                path.display()
            ));
            return Ok(self.summary(None));
        }

        let root_page = self
            .import_directory_tree(&path, parent_view_id, upload_assets)
            .await;
        Ok(self.summary(root_page))
    }

    /// Mirrors `_import_directory`. The boxed recursion is required because
    /// `async fn` cannot recurse directly.
    fn import_directory_tree<'a>(
        &'a mut self,
        directory_path: &'a Path,
        parent_view_id: &'a str,
        upload_assets: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<CreatedPage>> + Send + 'a>> {
        Box::pin(async move {
            // Precondition: the caller has verified this directory has Markdown in
            // its subtree, so it is worth a page. One scan yields everything we need.
            let (index_file, markdown_files, child_dirs) = scan_dir(directory_path);
            let view_id = Uuid::new_v4().to_string();
            // The document collab is stored with object_id == collab_id, but AppFlowy
            // loads a view's document by view_id. They must match or the page opens
            // with "Collab not found for object_id: <view_id>".
            let collab_id = view_id.clone();
            let mut blocks: Vec<Block> = Vec::new();
            let mut asset_count = 0usize;

            if let Some(index_file) = &index_file {
                let content = self.read_markdown(index_file);
                let before = self.uploaded_assets.len();
                let markdown_dir = index_file.parent().unwrap_or(directory_path);
                blocks = self
                    .parse_markdown_with_uploads(&content, markdown_dir, &view_id, upload_assets)
                    .await;
                asset_count = self.uploaded_assets.len() - before;
            }

            let title = directory_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let page = self
                .create_page(
                    parent_view_id,
                    &title,
                    blocks,
                    &view_id,
                    &collab_id,
                    directory_path,
                    "folder",
                    asset_count,
                )
                .await?;
            let page_view_id = page.view_id.clone();

            let sibling_files: Vec<PathBuf> = markdown_files
                .iter()
                .filter(|p| index_file.as_ref() != Some(*p))
                .cloned()
                .collect();
            // Failure of any single file is recorded into `errors` and does
            // not stop the rest, mirroring the Python try/except.
            self.create_sibling_markdown_pages(&sibling_files, &page_view_id, upload_assets)
                .await;

            for child_dir in &child_dirs {
                if !subtree_has_markdown(child_dir) {
                    // Asset-only / empty subfolder (e.g. "assets" with just images) —
                    // no document content, so don't create a page for it.
                    continue;
                }
                self.import_directory_tree(child_dir, &page_view_id, upload_assets)
                    .await;
            }

            Some(page)
        })
    }

    /// Mirrors `_create_markdown_page`.
    async fn create_markdown_page(
        &mut self,
        markdown_path: &Path,
        parent_view_id: &str,
        title: &str,
        upload_assets: bool,
        page_kind: &str,
    ) -> Option<CreatedPage> {
        let view_id = Uuid::new_v4().to_string();
        // collab_id must equal view_id; see import_directory_tree for details.
        let collab_id = view_id.clone();
        let content = self.read_markdown(markdown_path);
        let before = self.uploaded_assets.len();
        let markdown_dir = markdown_path.parent().unwrap_or(markdown_path);
        let blocks = self
            .parse_markdown_with_uploads(&content, markdown_dir, &view_id, upload_assets)
            .await;
        self.create_page(
            parent_view_id,
            title,
            blocks,
            &view_id,
            &collab_id,
            markdown_path,
            page_kind,
            self.uploaded_assets.len() - before,
        )
        .await
    }

    /// Phase A + B of the sibling pipeline (see the module docs): create one
    /// page per direct Markdown file concurrently, then restore the scan
    /// order among the created siblings when concurrency scrambled it.
    async fn create_sibling_markdown_pages(
        &mut self,
        markdown_files: &[PathBuf],
        parent_view_id: &str,
        upload_assets: bool,
    ) {
        if markdown_files.is_empty() {
            return;
        }
        // A single file cannot get out of order; create it inline.
        if let [path] = markdown_files {
            let title = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.create_markdown_page(path, parent_view_id, &title, upload_assets, "markdown_file")
                .await;
            return;
        }

        // --- Phase A: concurrent read → upload → parse → POST /page-view ---
        // Every task owns a fresh importer so per-page results (assets,
        // warnings, errors, the created-page entry) accumulate on task-local
        // state and are merged back by file index below — no shared mutable
        // state is touched from concurrent tasks.
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_PAGE_CREATES));
        let mut tasks: JoinSet<(usize, Option<CreatedPage>, MarkdownImporter)> = JoinSet::new();
        for (index, path) in markdown_files.iter().enumerate() {
            let client = self.client.clone();
            let workspace_id = self.workspace_id.clone();
            let parent_view_id = parent_view_id.to_string();
            let title = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let path = path.clone();
            let semaphore = Arc::clone(&semaphore);
            tasks.spawn(async move {
                let mut sub = MarkdownImporter::new(client, workspace_id);
                let page = match semaphore.acquire_owned().await {
                    Ok(_permit) => {
                        sub.create_markdown_page(
                            &path,
                            &parent_view_id,
                            &title,
                            upload_assets,
                            "markdown_file",
                        )
                        .await
                    }
                    Err(e) => {
                        sub.errors.push(ImportError {
                            path: path.display().to_string(),
                            error: format!("page-create semaphore closed: {e}"),
                        });
                        None
                    }
                };
                (index, page, sub)
            });
        }

        // The join order also records the order in which the creation POSTs
        // finished; AppFlowy's sibling order follows that order.
        let mut outcomes: Vec<Option<(Option<CreatedPage>, MarkdownImporter)>> = Vec::new();
        outcomes.resize_with(markdown_files.len(), || None);
        let mut completion_rank: Vec<usize> = vec![usize::MAX; markdown_files.len()];
        let mut completed = 0usize;
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((index, page, sub)) => {
                    completion_rank[index] = completed;
                    outcomes[index] = Some((page, sub));
                }
                Err(join_error) => {
                    // A panicked task carries no index; record a level error.
                    // Task bodies are infallible except for bugs.
                    self.errors.push(ImportError {
                        path: format!("page-create task under {parent_view_id}"),
                        error: join_error.to_string(),
                    });
                }
            }
            completed += 1;
        }

        // Merge in scan order so created_pages / uploaded_assets / warnings /
        // errors keep a deterministic, index-ordered shape.
        let mut indexed_view_ids: Vec<(usize, String)> = Vec::new();
        for (index, outcome) in outcomes.into_iter().enumerate() {
            let Some((page, mut sub)) = outcome else {
                continue;
            };
            self.uploaded_assets.append(&mut sub.uploaded_assets);
            self.warnings.append(&mut sub.warnings);
            self.errors.append(&mut sub.errors);
            self.created_pages.append(&mut sub.created_pages);
            if let Some(page) = page {
                indexed_view_ids.push((index, page.view_id));
            }
        }

        // --- Phase B: sibling-order correction -----------------------------
        let desired: Vec<String> = indexed_view_ids.iter().map(|(_, id)| id.clone()).collect();
        let mut by_completion = indexed_view_ids;
        by_completion.sort_by_key(|(index, _)| completion_rank[*index]);
        let completion_order: Vec<String> = by_completion.into_iter().map(|(_, id)| id).collect();
        // Fast path: every creation POST finished in scan order (the common
        // case), so the AppFlowy sibling order already matches and the
        // correction pass needs no extra request at all.
        if completion_order != desired {
            self.correct_sibling_order(parent_view_id, &desired).await;
        }
    }

    /// Reads the actual sibling order of the `desired` pages back from the
    /// folder tree and issues the minimal `/move` sequence that restores the
    /// `desired` order. Failures are recorded into `errors` and do not stop
    /// the remaining moves or the import.
    async fn correct_sibling_order(&mut self, parent_view_id: &str, desired: &[String]) {
        if desired.len() < 2 {
            return;
        }
        let params = [
            ("depth", "1".to_string()),
            ("root_view_id", parent_view_id.to_string()),
        ];
        let data = match self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/folder", self.workspace_id),
                Some(&params),
                None,
            )
            .await
        {
            Ok(data) => data,
            Err(e) => {
                self.errors.push(ImportError {
                    path: format!("sibling order check under {parent_view_id}"),
                    error: e.to_string(),
                });
                return;
            }
        };
        // Restrict to the pages this import just created; pre-existing
        // siblings participate only as fixed anchors via prev_view_id.
        let created: HashSet<&str> = desired.iter().map(String::as_str).collect();
        let actual: Vec<String> = data
            .get("children")
            .and_then(Value::as_array)
            .map(|children| {
                children
                    .iter()
                    .filter_map(|child| child.get("view_id").and_then(Value::as_str))
                    .filter(|id| created.contains(id))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        for action in plan_order_correction(&actual, desired) {
            let path = format!(
                "/api/workspace/{}/page-view/{}/move",
                self.workspace_id, action.view_id
            );
            let payload = json!({
                "new_parent_view_id": parent_view_id,
                "prev_view_id": action.prev_view_id,
            });
            // Sequential on purpose: each move is anchored at the page placed
            // by the previous one (AppFlowy has no bulk-reorder endpoint).
            if let Err(e) = self
                .client
                .request(Method::POST, &path, None, Some(payload))
                .await
            {
                self.errors.push(ImportError {
                    path: format!(
                        "sibling reorder of {} under {parent_view_id}",
                        action.view_id
                    ),
                    error: e.to_string(),
                });
            }
        }
    }

    /// Mirrors `_create_page`: POST /api/workspace/{wid}/page-view; on failure
    /// record `{"path", "error"}` and return None so the import continues.
    // The argument list mirrors the Python keyword-only parameters one-to-one.
    #[allow(clippy::too_many_arguments)]
    async fn create_page(
        &mut self,
        parent_view_id: &str,
        title: &str,
        blocks: Vec<Block>,
        view_id: &str,
        collab_id: &str,
        source_path: &Path,
        page_kind: &str,
        asset_count: usize,
    ) -> Option<CreatedPage> {
        let block_count = blocks.len();
        let payload = json!({
            "parent_view_id": parent_view_id,
            "layout": 0,
            "name": title,
            "page_data": {
                "type": "page",
                "children": blocks,
            },
            "view_id": view_id,
            "collab_id": collab_id,
        });
        let body = match self
            .client
            .request(
                Method::POST,
                &format!("/api/workspace/{}/page-view", self.workspace_id),
                None,
                Some(payload),
            )
            .await
        {
            Ok(body) => body,
            Err(e) => {
                self.errors.push(ImportError {
                    path: source_path.display().to_string(),
                    error: e.to_string(),
                });
                return None;
            }
        };
        // Defensive `body.get("data", body)` fallback, exactly as the Python
        // importer applied after `_request` (the Rust client already unwraps
        // the envelope centrally).
        let page = body.get("data").cloned().unwrap_or(body);
        let entry = CreatedPage {
            title: title.to_string(),
            ty: page_kind.to_string(),
            source_path: source_path.display().to_string(),
            view_id: view_id.to_string(),
            collab_id: collab_id.to_string(),
            parent_view_id: parent_view_id.to_string(),
            block_count,
            asset_count,
            page,
        };
        self.created_pages.push(entry.clone());
        Some(entry)
    }

    /// Two-phase replacement for the Python `image_url_resolver` pipeline:
    /// scan + resolve + parallel upload first, then parse with a pure lookup
    /// resolver. See the module docs for the rationale.
    async fn parse_markdown_with_uploads(
        &mut self,
        content: &str,
        markdown_dir: &Path,
        page_view_id: &str,
        upload_assets: bool,
    ) -> Vec<Block> {
        let url_map = if upload_assets {
            self.upload_page_assets(content, markdown_dir, page_view_id)
                .await
        } else {
            HashMap::new()
        };
        let mut resolver = |url: &str, _caption: &str| -> String {
            // Remote URLs, missing files and failed uploads are absent from the
            // map, so they keep their original url — mirroring the Python
            // resolver's pass-through behavior.
            url_map.get(url).cloned().unwrap_or_else(|| url.to_string())
        };
        parse_markdown_to_blocks(content, Some(&mut resolver))
    }

    /// Scans `content` for image references, resolves local files, uploads
    /// each unique file concurrently, and returns the
    /// `original url string -> uploaded url` map while appending to
    /// `uploaded_assets` / `warnings` / `errors`.
    async fn upload_page_assets(
        &mut self,
        content: &str,
        markdown_dir: &Path,
        page_view_id: &str,
    ) -> HashMap<String, String> {
        let references = scan_image_references(content);

        // One upload slot per unique resolved file; `url_to_slot` may point
        // several distinct markdown url strings at the same file (the Python
        // version re-uploads per occurrence — deduping by resolved path is
        // part of the optimization).
        struct PendingAsset {
            path: PathBuf,
            markdown_url: String,
            caption: String,
        }
        let mut pending: Vec<PendingAsset> = Vec::new();
        let mut path_to_slot: HashMap<PathBuf, usize> = HashMap::new();
        let mut url_to_slot: HashMap<String, usize> = HashMap::new();

        for (url, caption) in references {
            if is_remote_url(&url) {
                continue;
            }
            let path = match resolve_local_asset(markdown_dir, &url) {
                Some(path) => path,
                None => {
                    // Emitted once per occurrence, matching the Python
                    // resolver being called for every reference.
                    self.warnings.push(format!(
                        "Image not found: {} in {}",
                        url,
                        markdown_dir.display()
                    ));
                    continue;
                }
            };
            if url_to_slot.contains_key(&url) {
                continue;
            }
            let slot = match path_to_slot.get(&path) {
                Some(&slot) => slot,
                None => {
                    pending.push(PendingAsset {
                        path: path.clone(),
                        markdown_url: url.clone(),
                        caption,
                    });
                    path_to_slot.insert(path, pending.len() - 1);
                    pending.len() - 1
                }
            };
            url_to_slot.insert(url, slot);
        }

        struct UploadOk {
            upload: Value,
            content_type: String,
            content_length: usize,
        }

        // Upload the unique files concurrently, bounded by a semaphore. Each
        // task carries its slot index so results can be re-assembled in
        // deterministic first-occurrence order regardless of completion order.
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_UPLOADS));
        let mut tasks: JoinSet<(usize, Result<UploadOk>)> = JoinSet::new();
        for (slot, asset) in pending.iter().enumerate() {
            let client = self.client.clone();
            let workspace_id = self.workspace_id.clone();
            let parent_dir = page_view_id.to_string();
            let path = asset.path.clone();
            let semaphore = Arc::clone(&semaphore);
            tasks.spawn(async move {
                let result = async {
                    let _permit = semaphore
                        .acquire_owned()
                        .await
                        .map_err(|e| Error::Other(format!("upload semaphore closed: {e}")))?;
                    let content = tokio::fs::read(&path).await?;
                    let content_type = mime_guess::from_path(&path)
                        .first()
                        .map(|m| m.essence_str().to_string())
                        .unwrap_or_else(|| "application/octet-stream".to_string());
                    let upload = client
                        .upload_file_blob_v1(&workspace_id, &parent_dir, &content, &content_type)
                        .await?;
                    Ok(UploadOk {
                        upload,
                        content_type,
                        content_length: content.len(),
                    })
                }
                .await;
                (slot, result)
            });
        }

        let mut results: Vec<Option<UploadOk>> = Vec::new();
        results.resize_with(pending.len(), || None);
        let mut failed: Vec<(usize, String)> = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((slot, Ok(ok))) => results[slot] = Some(ok),
                Ok((slot, Err(e))) => failed.push((slot, e.to_string())),
                Err(join_error) => {
                    // A panicked task carries no slot; record a page-level
                    // error. Task bodies are infallible except for bugs.
                    failed.push((usize::MAX, join_error.to_string()));
                }
            }
        }
        // Single-file upload failure → errors entry + the image keeps its
        // original url (its slot never enters `results`), aligning with the
        // Python resolver's except branch.
        failed.sort_by_key(|(slot, _)| *slot);
        for (slot, error) in failed {
            let path = if slot < pending.len() {
                pending[slot].path.display().to_string()
            } else {
                format!("upload task (page {page_view_id})")
            };
            self.errors.push(ImportError { path, error });
        }

        // Append asset entries in first-occurrence order (matching the order
        // of the Python resolver's *first* call per unique file) and collect
        // each slot's uploaded url for the resolver map.
        let mut uploaded_url: Vec<Option<String>> = Vec::new();
        uploaded_url.resize_with(pending.len(), || None);
        for (slot, result) in results.into_iter().enumerate() {
            let Some(ok) = result else { continue };
            let asset = &pending[slot];
            uploaded_url[slot] = ok
                .upload
                .get("url")
                .and_then(Value::as_str)
                .map(str::to_string);
            self.uploaded_assets.push(UploadedAsset {
                source_path: asset.path.display().to_string(),
                markdown_url: asset.markdown_url.clone(),
                caption: asset.caption.clone(),
                page_view_id: page_view_id.to_string(),
                file_id: ok.upload.get("file_id").cloned().unwrap_or(Value::Null),
                url: ok.upload.get("url").cloned().unwrap_or(Value::Null),
                content_type: ok.content_type,
                content_length: ok.content_length,
            });
        }

        let mut url_map = HashMap::with_capacity(url_to_slot.len());
        for (url, &slot) in &url_to_slot {
            if let Some(Some(uploaded)) = uploaded_url.get(slot) {
                url_map.insert(url.clone(), uploaded.clone());
            }
        }
        url_map
    }

    /// Mirrors `_read_markdown`: decode as UTF-8 with BOM stripping
    /// (`utf-8-sig`); on invalid UTF-8 warn and decode lossy.
    fn read_markdown(&mut self, path: &Path) -> String {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) => {
                // Python would propagate the OSError out of read_text; the
                // Rust call chain has no Result path here, so degrade to an
                // empty page body and keep importing.
                self.warnings.push(format!(
                    "Failed to read Markdown file, imported as empty: {} ({})",
                    path.display(),
                    e
                ));
                return String::new();
            }
        };
        let bytes = strip_bom(&bytes);
        match String::from_utf8(bytes.to_vec()) {
            Ok(text) => text,
            Err(_) => {
                self.warnings.push(format!(
                    "File is not valid UTF-8, decoded with replacement: {}",
                    path.display()
                ));
                String::from_utf8_lossy(bytes).into_owned()
            }
        }
    }

    /// Mirrors `_summary`.
    fn summary(&self, root_page: Option<CreatedPage>) -> ImportSummary {
        ImportSummary {
            root_page,
            created_page_count: self.created_pages.len(),
            uploaded_asset_count: self.uploaded_assets.len(),
            error_count: self.errors.len(),
            warning_count: self.warnings.len(),
            created_pages: self.created_pages.clone(),
            uploaded_assets: self.uploaded_assets.clone(),
            warnings: self.warnings.clone(),
            errors: self.errors.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

fn lowercase_suffix(path: &Path) -> String {
    path.extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

/// Equivalent of Python's `Path.expanduser()` for a leading `~/` (or bare
/// `~`): on Windows the home directory comes from `%USERPROFILE%` and
/// elsewhere from `$HOME` (USERPROFILE wins when both exist, matching the
/// Windows-first target); with no known home the input is returned unchanged.
fn expand_tilde(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') {
            let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
            if let Some(home) = home {
                let mut path = PathBuf::from(home);
                let rest = rest.trim_start_matches(['/', '\\']);
                if !rest.is_empty() {
                    path.push(rest);
                }
                return path;
            }
        }
    }
    PathBuf::from(input)
}

/// `Path(input).expanduser().resolve()` without requiring existence:
/// canonicalize when possible, else fall back to an absolute (cwd-joined)
/// path so validation errors still carry a full path. The Windows verbatim
/// (`\\?\`) prefix from `canonicalize` is stripped so reported paths read
/// like Python's `str(Path.resolve())`.
fn resolve_user_path(input: &str) -> PathBuf {
    let expanded = expand_tilde(input);
    match fs::canonicalize(&expanded) {
        Ok(p) => strip_verbatim_prefix(p),
        Err(_) if expanded.is_absolute() => expanded,
        Err(_) => std::env::current_dir()
            .map(|cwd| cwd.join(&expanded))
            .unwrap_or(expanded),
    }
}

fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        // Keep \\?\UNC\ intact; drive-letter paths become regular paths.
        Some(rest) if !rest.starts_with(r"UNC\") => PathBuf::from(rest),
        _ => path,
    }
}

/// Mirrors `_is_remote_url`: the lowercased scheme is one of
/// http/https/data/mailto.
fn is_remote_url(value: &str) -> bool {
    match url::Url::parse(value) {
        Ok(parsed) => matches!(parsed.scheme(), "http" | "https" | "data" | "mailto"),
        // No scheme at all parses as Err — treated as a local path.
        Err(_) => false,
    }
}

/// Mirrors `_resolve_local_asset`: urlsplit + unquote, `\` → `/`, relative to
/// the markdown directory, resolve, then `is_file`.
fn resolve_local_asset(markdown_dir: &Path, image_url: &str) -> Option<PathBuf> {
    // `urlsplit(image_url).path` for the local (scheme-less or ignored-scheme)
    // cases this importer encounters: strip any query/fragment. For
    // scheme-bearing non-remote URLs the whole string is used as the path —
    // it then fails `is_file` exactly like Python's resolved candidate does.
    let without_fragment = image_url.split(['?', '#']).next().unwrap_or_default();
    let decoded = percent_encoding::percent_decode_str(without_fragment)
        .decode_utf8_lossy()
        .replace('\\', "/");
    if decoded.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(decoded);
    let candidate = if candidate.is_absolute() {
        candidate
    } else {
        markdown_dir.join(candidate)
    };
    // Python's non-strict resolve + is_file() == canonicalize succeeding.
    let resolved = fs::canonicalize(candidate).ok()?;
    let resolved = strip_verbatim_prefix(resolved);
    if resolved.is_file() {
        Some(resolved)
    } else {
        None
    }
}

/// Mirrors `_scan_dir`: one listing sorted by lowercase name, returning
/// (index_file, markdown_files, child_dirs). Unreadable directory →
/// (None, [], []).
fn scan_dir(directory_path: &Path) -> (Option<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let mut index_file: Option<PathBuf> = None;
    let mut markdown_files: Vec<PathBuf> = Vec::new();
    let mut child_dirs: Vec<PathBuf> = Vec::new();
    let mut entries: Vec<PathBuf> = match fs::read_dir(directory_path) {
        Ok(rd) => rd
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .collect(),
        Err(_) => return (None, Vec::new(), Vec::new()),
    };
    entries.sort_by_key(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    });
    for item in entries {
        if item.is_file() {
            if MARKDOWN_SUFFIXES.contains(&lowercase_suffix(&item).as_str()) {
                let name = item
                    .file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if index_file.is_none() && INDEX_FILENAMES.contains(&name.as_str()) {
                    index_file = Some(item.clone());
                }
                markdown_files.push(item);
            }
        } else if item.is_dir() {
            let name = item
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !IGNORED_DIRECTORIES.contains(&name.as_str()) && !name.starts_with('.') {
                child_dirs.push(item);
            }
        }
    }
    (index_file, markdown_files, child_dirs)
}

/// Mirrors `_subtree_has_markdown`.
fn subtree_has_markdown(directory_path: &Path) -> bool {
    let (_, markdown_files, child_dirs) = scan_dir(directory_path);
    if !markdown_files.is_empty() {
        return true;
    }
    child_dirs.iter().any(|child| subtree_has_markdown(child))
}

// ---------------------------------------------------------------------------
// Sibling-order correction plan
// ---------------------------------------------------------------------------

/// One planned reordering step: place `view_id` immediately after
/// `prev_view_id` (`None` = make it the first child), mirroring the
/// `/page-view/{id}/move` endpoint's semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SiblingMove {
    view_id: String,
    prev_view_id: Option<String>,
}

/// Compute the reordering steps that turn `actual` into `desired`, both
/// restricted to the same set of view_ids (ids of `desired` missing from
/// `actual` — e.g. a page whose creation failed — are skipped).
///
/// Strategy: walk `desired` left to right over a simulated order; a page
/// already sitting at its final absolute position is left untouched, every
/// other page is moved once behind its intended predecessor. In-order
/// prefixes therefore cost nothing, only misplaced pages are moved, each
/// exactly once, and since the last page can never be the only one out of
/// place the worst case is `n - 1` moves for `n` pages (a fully reversed
/// order).
fn plan_order_correction(actual: &[String], desired: &[String]) -> Vec<SiblingMove> {
    let exists: HashSet<&str> = actual.iter().map(String::as_str).collect();
    let wanted: Vec<&str> = desired
        .iter()
        .map(String::as_str)
        .filter(|id| exists.contains(id))
        .collect();
    if wanted.len() < 2 {
        return Vec::new();
    }
    let wanted_set: HashSet<&str> = wanted.iter().copied().collect();
    let mut current: Vec<&str> = actual
        .iter()
        .map(String::as_str)
        .filter(|id| wanted_set.contains(id))
        .collect();
    let mut moves = Vec::new();
    for (index, &id) in wanted.iter().enumerate() {
        let Some(position) = current.iter().position(|&x| x == id) else {
            continue;
        };
        if position == index {
            continue;
        }
        current.remove(position);
        current.insert(index, id);
        moves.push(SiblingMove {
            view_id: id.to_string(),
            prev_view_id: (index > 0).then(|| wanted[index - 1].to_string()),
        });
    }
    moves
}

// ---------------------------------------------------------------------------
// Image-reference pre-scan
// ---------------------------------------------------------------------------

fn full_line_image_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^!\[(?P<alt>[^\]]*)\]\((?P<url>[^)]+)\)$").expect("image pattern compiles")
    })
}

fn inline_image_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"!\[(?P<alt>[^\]]*)\]\((?P<url>[^)]+)\)")
            .expect("inline image pattern compiles")
    })
}

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^#{1,6}\s+.+$").expect("heading pattern compiles"))
}

fn ordered_list_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d+\.\s+.+$").expect("ordered list pattern compiles"))
}

fn math_one_liner_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\$\$.+?\$\$$").expect("math block pattern compiles"))
}

fn table_separator_cell_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^:?-+:?$").expect("table separator cell pattern compiles"))
}

/// Scan `content` for the exact set of `(url, caption)` references that
/// `parse_markdown_to_blocks` would pass to its image resolver: full-line
/// images plus inline images on fallback (plain paragraph) lines only.
///
/// This deliberately mirrors the parser's line-gating control flow — front
/// matter, fenced code blocks, dividers, tables, math, headings, todos,
/// lists and quotes never reach the resolver there, and so must not be
/// scanned here either — so the upload set matches byte-for-byte what the
/// Python serial resolver would have uploaded.
fn scan_image_references(content: &str) -> Vec<(String, String)> {
    let mut lines: Vec<&str> = content.lines().collect();
    strip_front_matter_lines(&mut lines);

    let mut refs = Vec::new();
    let total = lines.len();
    let mut i = 0usize;
    let mut in_code_block = false;

    while i < total {
        let line = lines[i];
        let stripped = line.trim();

        if stripped.starts_with("```") {
            in_code_block = !in_code_block;
            i += 1;
            continue;
        }
        if in_code_block || stripped.is_empty() {
            i += 1;
            continue;
        }
        if stripped == "---" || stripped == "***" {
            i += 1;
            continue;
        }
        if stripped.starts_with('|') && i + 1 < total && is_table_separator_line(lines[i + 1]) {
            let mut j = i + 2;
            while j < total && lines[j].trim().starts_with('|') {
                j += 1;
            }
            i = j;
            continue;
        }
        if stripped == "$$" {
            let mut j = i + 1;
            while j < total && lines[j].trim() != "$$" {
                j += 1;
            }
            i = if j < total { j + 1 } else { j };
            continue;
        }
        if math_one_liner_re().is_match(stripped) {
            i += 1;
            continue;
        }
        if let Some(caps) = full_line_image_re().captures(stripped) {
            refs.push((caps["url"].to_string(), caps["alt"].to_string()));
            i += 1;
            continue;
        }
        if heading_re().is_match(stripped)
            || stripped.starts_with("- [ ] ")
            || stripped.starts_with("- [x] ")
            || stripped.starts_with("- ")
            || stripped.starts_with("* ")
            || ordered_list_re().is_match(stripped)
            || stripped.starts_with("> ")
        {
            i += 1;
            continue;
        }
        for caps in inline_image_re().captures_iter(line) {
            refs.push((caps["url"].to_string(), caps["alt"].to_string()));
        }
        i += 1;
    }
    refs
}

/// Local copy of markdown.rs's private `strip_front_matter`, kept in sync so
/// the pre-scan and the parser skip the same lines.
fn strip_front_matter_lines(lines: &mut Vec<&str>) {
    if lines.first().map(|l| l.trim()) != Some("---") {
        return;
    }
    for idx in 1..lines.len() {
        if lines[idx].trim() == "---" {
            lines.drain(..=idx);
            return;
        }
    }
}

/// Simplified `is_table_separator`: unlike markdown.rs it splits cells on
/// plain `|` without unescaping `\|`. An escaped pipe inside a separator row
/// is virtually nonexistent; the worst case is the scan treating a table
/// header as a fallback line (an extra upload), never a missed replacement.
fn is_table_separator_line(line: &str) -> bool {
    let stripped = line.trim();
    if !stripped.contains('|') {
        return false;
    }
    let mut row = stripped;
    if let Some(rest) = row.strip_prefix('|') {
        row = rest;
    }
    if let Some(rest) = row.strip_suffix('|') {
        row = rest;
    }
    let cells: Vec<&str> = row.split('|').collect();
    if cells.is_empty() {
        return false;
    }
    cells
        .iter()
        .all(|cell| table_separator_cell_re().is_match(cell.trim()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn unique_temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("appflowy-rs-importer-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // -- expand_tilde ------------------------------------------------------

    #[test]
    fn expand_tilde_leaves_plain_paths_alone() {
        assert_eq!(expand_tilde("docs/a.md"), PathBuf::from("docs/a.md"));
        assert_eq!(
            expand_tilde("~user/docs/a.md"),
            PathBuf::from("~user/docs/a.md")
        );
    }

    #[test]
    fn expand_tilde_uses_home_when_available() {
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
        if home.is_none() {
            // Spec: with no home known, return the input unchanged.
            assert_eq!(expand_tilde("~/a.md"), PathBuf::from("~/a.md"));
            return;
        }
        let expanded = expand_tilde("~/a.md");
        let text = expanded.to_string_lossy().replace('\\', "/");
        assert!(text.ends_with("/a.md"), "unexpected: {text}");
        assert!(!text.starts_with('~'));
    }

    // -- is_remote_url -----------------------------------------------------

    #[test]
    fn remote_url_detection_matches_python_scheme_set() {
        for url in [
            "https://example.com/a.png",
            "http://example.com/a.png",
            "HTTP://example.com/a.png",
            "data:image/png;base64,AAAA",
            "mailto:a@b.c",
        ] {
            assert!(is_remote_url(url), "{url} should be remote");
        }
        for url in [
            "assets/a.png",
            "./assets/a.png",
            "../up/a.png",
            "/abs/a.png",
            r"C:\pics\a.png",
            "ftp://example.com/a.png",
        ] {
            assert!(!is_remote_url(url), "{url} should be local");
        }
    }

    // -- resolve_local_asset ------------------------------------------------

    #[test]
    fn resolve_local_asset_finds_relative_file() {
        let dir = unique_temp_dir("resolve");
        fs::create_dir_all(dir.join("assets")).unwrap();
        let img = dir.join("assets").join("a b.png");
        fs::write(&img, b"png").unwrap();

        let found = resolve_local_asset(&dir, "assets/a%20b.png").expect("must resolve");
        assert!(found.is_file());

        // Query/fragment are stripped like urlsplit does.
        let found = resolve_local_asset(&dir, "assets/a%20b.png?v=2#frag").expect("must resolve");
        assert!(found.is_file());

        assert!(resolve_local_asset(&dir, "assets/missing.png").is_none());
        assert!(resolve_local_asset(&dir, "").is_none());

        fs::remove_dir_all(&dir).ok();
    }

    // -- scan_dir / subtree_has_markdown ------------------------------------

    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn build() -> Self {
            let root = unique_temp_dir("scan");
            fs::write(root.join("readme.md"), "# index").unwrap();
            fs::write(root.join("b.md"), "b").unwrap();
            fs::write(root.join("A.MD"), "a").unwrap();
            fs::write(root.join("pic.png"), "png").unwrap();
            let sub = root.join("sub");
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join("x.markdown"), "x").unwrap();
            let assets = root.join("assets");
            fs::create_dir_all(&assets).unwrap();
            fs::write(assets.join("only.png"), "png").unwrap();
            let hidden = root.join(".hidden");
            fs::create_dir_all(&hidden).unwrap();
            fs::write(hidden.join("y.md"), "y").unwrap();
            let ignored = root.join("node_modules");
            fs::create_dir_all(&ignored).unwrap();
            fs::write(ignored.join("z.md"), "z").unwrap();
            // A container whose only Markdown is inside an ignored child.
            let container = root.join("container");
            fs::create_dir_all(container.join("node_modules")).unwrap();
            fs::write(container.join("node_modules").join("z.md"), "z").unwrap();
            Self { root }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).ok();
        }
    }

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| {
                p.file_name()
                    .unwrap_or(OsStr::new(""))
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn scan_dir_sorts_and_classifies_entries() {
        let tree = Tree::build();
        let (index, markdown_files, child_dirs) = scan_dir(&tree.root);
        assert_eq!(index.as_deref().unwrap().file_name().unwrap(), "readme.md");
        // Sorted by lowercase name: a.MD, b.md, readme.md.
        assert_eq!(names(&markdown_files), vec!["A.MD", "b.md", "readme.md"]);
        // Dot-prefixed and ignored directories are skipped.
        assert_eq!(names(&child_dirs), vec!["assets", "container", "sub"]);
    }

    #[test]
    fn subtree_has_markdown_skips_asset_only_and_ignored_dirs() {
        let tree = Tree::build();
        assert!(subtree_has_markdown(&tree.root));
        assert!(!subtree_has_markdown(&tree.root.join("assets")));
        assert!(subtree_has_markdown(&tree.root.join("sub")));
        // The ignored/dot-dir filter applies to *children* of a scanned
        // directory (matching Python): Markdown nested only inside an ignored
        // child does not count, while querying the ignored dir itself does.
        assert!(!subtree_has_markdown(&tree.root.join("container")));
        assert!(subtree_has_markdown(&tree.root.join("node_modules")));
    }

    // -- scan_image_references ----------------------------------------------

    fn urls(content: &str) -> Vec<String> {
        scan_image_references(content)
            .into_iter()
            .map(|(url, _)| url)
            .collect()
    }

    #[test]
    fn scan_collects_full_line_and_inline_images() {
        let content =
            "![full](a.png)\n\ntext with ![inline](b.png) and more\n![also](c.png) after text\n";
        assert_eq!(urls(content), vec!["a.png", "b.png", "c.png"]);
    }

    #[test]
    fn scan_skips_regions_the_parser_never_resolves() {
        let content = concat!(
            "---\nmeta: ![fm](fm.png)\n---\n",
            "```\n![code](code.png)\n```\n",
            "# Heading ![head](head.png)\n",
            "- bullet ![list](list.png)\n",
            "- [ ] todo ![todo](todo.png)\n",
            "1. ordered ![ord](ord.png)\n",
            "> quote ![q](q.png)\n",
            "$$ ![math-inline](mi.png) $$\n",
            "$$\n![math](m.png)\n$$\n",
            "| ![tbl](t.png) |\n| --- |\n| ![tbl2](t2.png) |\n",
            "---\n",
            "![kept](keep.png)\n"
        );
        assert_eq!(urls(content), vec!["keep.png"]);
    }

    #[test]
    fn scan_matches_parser_gating_for_edge_lines() {
        // `#no-space` is not a heading for the parser, so the inline image
        // DOES reach the resolver.
        let content = "#no-space ![x](x.png)";
        assert_eq!(urls(content), vec!["x.png"]);
    }

    // -- summary / serialization shape ---------------------------------------

    #[test]
    fn summary_serializes_with_python_field_names() {
        let summary = ImportSummary {
            root_page: None,
            created_page_count: 0,
            uploaded_asset_count: 0,
            error_count: 1,
            warning_count: 0,
            created_pages: vec![],
            uploaded_assets: vec![],
            warnings: vec![],
            errors: vec![ImportError {
                path: "p".into(),
                error: "e".into(),
            }],
        };
        let value = serde_json::to_value(&summary).unwrap();
        let obj = value.as_object().unwrap();
        for key in [
            "root_page",
            "created_page_count",
            "uploaded_asset_count",
            "error_count",
            "warning_count",
            "created_pages",
            "uploaded_assets",
            "warnings",
            "errors",
        ] {
            assert!(obj.contains_key(key), "missing key {key}");
        }
        assert_eq!(obj.len(), 9);
        assert_eq!(value["root_page"], Value::Null);
        assert_eq!(value["errors"][0]["path"], "p");
        assert_eq!(value["errors"][0]["error"], "e");
    }

    #[test]
    fn created_page_uses_type_key() {
        let page = CreatedPage {
            title: "t".into(),
            ty: "folder".into(),
            source_path: "s".into(),
            view_id: "v".into(),
            collab_id: "v".into(),
            parent_view_id: "p".into(),
            block_count: 0,
            asset_count: 0,
            page: Value::Null,
        };
        let value = serde_json::to_value(&page).unwrap();
        assert_eq!(value["type"], "folder");
        assert!(value.get("ty").is_none());
    }

    // -- read_markdown --------------------------------------------------------

    #[test]
    fn read_markdown_strips_bom_and_decodes_lossy() {
        let dir = unique_temp_dir("read");
        let mut importer =
            MarkdownImporter::new(AppFlowyClient::new(None, None, "http://127.0.0.1:1"), "ws");

        let bom_path = dir.join("bom.md");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("# hi".as_bytes());
        fs::write(&bom_path, &bytes).unwrap();
        assert_eq!(importer.read_markdown(&bom_path), "# hi");

        let bad_path = dir.join("bad.md");
        fs::write(&bad_path, [0x66, 0x6F, 0xC3, 0x28]).unwrap(); // invalid UTF-8
        let text = importer.read_markdown(&bad_path);
        assert!(text.contains('\u{FFFD}'));
        assert_eq!(importer.warnings.len(), 1);
        assert!(importer.warnings[0].starts_with("File is not valid UTF-8"));

        fs::remove_dir_all(&dir).ok();
    }

    // -- plan_order_correction (sibling-order repair) ------------------------

    fn str_ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// Apply a planned move sequence the way the AppFlowy move endpoint
    /// would: remove the page, then re-insert it immediately behind
    /// `prev_view_id` (or at the very front when `prev_view_id` is None).
    fn apply_moves(actual: &[String], moves: &[SiblingMove]) -> Vec<String> {
        let mut current = actual.to_vec();
        for action in moves {
            current.retain(|id| id != &action.view_id);
            let position = match &action.prev_view_id {
                None => 0,
                Some(prev) => current
                    .iter()
                    .position(|id| id == prev)
                    .map(|p| p + 1)
                    .unwrap_or(current.len()),
            };
            current.insert(position, action.view_id.clone());
        }
        current
    }

    #[test]
    fn order_correction_is_empty_when_already_ordered() {
        let desired = str_ids(&["a", "b", "c"]);
        assert!(plan_order_correction(&desired, &desired).is_empty());
    }

    #[test]
    fn order_correction_moves_single_misplaced_page_once() {
        let desired = str_ids(&["a", "b", "c"]);
        let actual = str_ids(&["b", "a", "c"]);
        let moves = plan_order_correction(&actual, &desired);
        assert_eq!(
            moves,
            vec![SiblingMove {
                view_id: "a".into(),
                prev_view_id: None, // front: it precedes everything
            }]
        );
        assert_eq!(apply_moves(&actual, &moves), desired);
    }

    #[test]
    fn order_correction_skips_in_order_prefixes() {
        // Only the tail is scrambled; the settled prefix costs no move.
        let desired = str_ids(&["a", "b", "c", "d", "e"]);
        let actual = str_ids(&["a", "b", "c", "e", "d"]);
        let moves = plan_order_correction(&actual, &desired);
        assert_eq!(
            moves,
            vec![SiblingMove {
                view_id: "d".into(),
                prev_view_id: Some("c".into()),
            }]
        );
        assert_eq!(apply_moves(&actual, &moves), desired);
    }

    #[test]
    fn order_correction_reversed_order_needs_n_minus_one_moves() {
        let desired = str_ids(&["a", "b", "c", "d", "e", "f", "g", "h"]);
        let actual: Vec<String> = desired.iter().rev().cloned().collect();
        let moves = plan_order_correction(&actual, &desired);
        // The worst case: every page but the last is out of place.
        assert_eq!(moves.len(), desired.len() - 1);
        assert_eq!(moves[0].prev_view_id, None);
        assert_eq!(apply_moves(&actual, &moves), desired);
    }

    #[test]
    fn order_correction_converges_for_scrambled_orders() {
        let desired = str_ids(&["a", "b", "c", "d", "e"]);
        for actual_strs in [
            &["a", "c", "b", "e", "d"][..],
            &["e", "d", "c", "b", "a"][..],
            &["b", "c", "d", "e", "a"][..],
            &["c", "a", "e", "b", "d"][..],
            &["d", "e", "a", "b", "c"][..],
        ] {
            let actual = str_ids(actual_strs);
            let moves = plan_order_correction(&actual, &desired);
            assert!(
                moves.len() < desired.len(),
                "never more than n - 1 moves: {actual:?} -> {moves:?}"
            );
            assert_eq!(apply_moves(&actual, &moves), desired, "for {actual:?}");
        }
    }

    #[test]
    fn order_correction_skips_pages_missing_from_actual() {
        // "b" was never created (its creation failed upstream), so the plan
        // only reorders the pages that actually exist.
        let desired = str_ids(&["a", "b", "c"]);
        let actual = str_ids(&["c", "a"]);
        let moves = plan_order_correction(&actual, &desired);
        assert_eq!(
            moves,
            vec![SiblingMove {
                view_id: "a".into(),
                prev_view_id: None,
            }]
        );
        assert_eq!(apply_moves(&actual, &moves), str_ids(&["a", "c"]));

        // Fewer than two existing pages can never be out of order.
        let actual = str_ids(&["b"]);
        assert!(plan_order_correction(&actual, &desired).is_empty());
        assert!(plan_order_correction(&[], &desired).is_empty());
    }
}

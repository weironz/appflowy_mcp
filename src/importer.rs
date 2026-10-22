//! Markdown file/directory importer.
//!
//! Faithful async port of `appflowy_mcp/importer.py`, with one deliberate
//! in-place optimization: the Python version uploads each image serially
//! inside the markdown image-url resolver, which is the dominant bottleneck
//! when importing large directories. Here every page is processed in two
//! phases:
//!
//! 1. The raw markdown is run through `parse_markdown_to_blocks` once with a
//!    recording image resolver, so the scan sees precisely the URLs the real
//!    resolver will be asked about — by construction, not by a hand-maintained
//!    mirror of the parser's line gating. Local files are resolved,
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
use std::sync::Arc;

use reqwest::Method;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::client::AppFlowyClient;
use crate::error::{Error, Result};
use crate::markdown::{parse_markdown_to_blocks, Block};
use crate::tools::util::ToolProgress;

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
    /// Active progress reporter plus the import-wide page counter. Attached
    /// by the `appflowy_import_markdown_directory` tool handler when the
    /// client sent a `_meta.progressToken`; `None` (the default) disables
    /// notifications entirely.
    progress: Option<ImportProgress>,
}

struct ImportProgress {
    reporter: ToolProgress,
    done: usize,
    total: usize,
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
            progress: None,
        }
    }

    /// Attach a progress reporter (built from the tool call's
    /// `RequestContext`). An inert reporter — no progressToken — stays
    /// `None`, so the import never even counts pages.
    pub fn set_progress(&mut self, reporter: ToolProgress) {
        if reporter.is_active() {
            self.progress = Some(ImportProgress {
                reporter,
                done: 0,
                total: 0,
            });
        }
    }

    /// One Markdown file of the import finished being processed (its page
    /// created or failed): advance the `importing done/total pages` bar.
    async fn tick_progress(&mut self) {
        let Some(progress) = &mut self.progress else {
            return;
        };
        progress.done += 1;
        progress
            .reporter
            .report(progress.done, progress.total, "importing")
            .await;
    }

    pub async fn import_file(
        &mut self,
        file_path: &str,
        parent_view_id: &str,
        title: Option<&str>,
        upload_assets: bool,
    ) -> Result<ImportSummary> {
        let path = resolve_user_path(file_path).await?;
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
        let path = resolve_user_path(directory_path).await?;
        if !path.is_dir() {
            return Err(Error::Validation(format!(
                "Markdown directory does not exist: {}",
                path.display()
            )));
        }

        // One DFS decides up front which directories contain Markdown, so
        // the tree walk consults this set instead of re-walking whole
        // subtrees at every ancestor level (the per-call walk was
        // O(n × depth) directory visits). The synchronous filesystem IO
        // runs off the tokio workers via spawn_blocking.
        let root = path.clone();
        let markdown_dirs = tokio::task::spawn_blocking(move || markdown_subtree_dirs(&root))
            .await
            // A JoinError is only possible if the DFS panicked (it cannot);
            // an empty set then means "no Markdown anywhere" — a safe no-op.
            .unwrap_or_default();
        if !markdown_dirs.contains(&path) {
            self.warnings.push(format!(
                "No Markdown files found under {}; nothing was imported.",
                path.display()
            ));
            return Ok(self.summary(None));
        }

        // Denominator for `importing n/total pages`: every Markdown file in
        // the tree, counted under the same ignore rules as the import walk.
        if let Some(progress) = &mut self.progress {
            let root = path.clone();
            progress.total = tokio::task::spawn_blocking(move || count_markdown_files(&root))
                .await
                .unwrap_or(0);
        }

        let root_page = self
            .import_directory_tree(&path, parent_view_id, upload_assets, &markdown_dirs)
            .await;
        Ok(self.summary(root_page))
    }

    /// Mirrors `_import_directory`. The boxed recursion is required because
    /// `async fn` cannot recurse directly. `markdown_dirs` is the
    /// precomputed set of Markdown-bearing directories (see
    /// [`Self::import_directory`]) consulted instead of re-walking each
    /// subtree per level.
    fn import_directory_tree<'a>(
        &'a mut self,
        directory_path: &'a Path,
        parent_view_id: &'a str,
        upload_assets: bool,
        markdown_dirs: &'a HashSet<PathBuf>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<CreatedPage>> + Send + 'a>> {
        Box::pin(async move {
            // Precondition: the caller has verified this directory has Markdown in
            // its subtree, so it is worth a page. One scan yields everything we need;
            // it performs blocking IO, so it runs off the tokio workers.
            let scan_root = directory_path.to_path_buf();
            let (index_file, markdown_files, child_dirs) =
                tokio::task::spawn_blocking(move || scan_dir(&scan_root))
                    .await
                    // A JoinError is only possible if scan_dir panicked (it
                    // cannot); degrade like an unreadable directory.
                    .unwrap_or_else(|_| (None, Vec::new(), Vec::new()));
            let view_id = Uuid::new_v4().to_string();
            // The document collab is stored with object_id == collab_id, but AppFlowy
            // loads a view's document by view_id. They must match or the page opens
            // with "Collab not found for object_id: <view_id>".
            let collab_id = view_id.clone();
            let mut blocks: Vec<Block> = Vec::new();
            let mut asset_count = 0usize;

            if let Some(index_file) = &index_file {
                let content = self.read_markdown(index_file).await;
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
            // The index file is folded into this folder page, so its page
            // creation is that file's progress tick.
            if index_file.is_some() {
                self.tick_progress().await;
            }

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
                if !markdown_dirs.contains(child_dir) {
                    // Asset-only / empty subfolder (e.g. "assets" with just images) —
                    // no document content, so don't create a page for it.
                    continue;
                }
                self.import_directory_tree(child_dir, &page_view_id, upload_assets, markdown_dirs)
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
        let content = self.read_markdown(markdown_path).await;
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
            self.tick_progress().await;
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
            // One Markdown file's page-create pipeline finished — successful
            // or recorded into errors, it counts as done.
            self.tick_progress().await;
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
            match self
                .upload_page_assets(content, markdown_dir, page_view_id)
                .await
            {
                Ok(url_map) => url_map,
                Err(e) => {
                    // Off-worker path resolution failed wholesale; keep the
                    // page importable with the original urls rather than
                    // emitting a misleading "not found" warning per image.
                    self.errors.push(ImportError {
                        path: markdown_dir.display().to_string(),
                        error: e.to_string(),
                    });
                    HashMap::new()
                }
            }
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
    ///
    /// Err means the blocking path-resolution task itself failed (a
    /// `JoinError`); per-asset read/upload failures remain in `errors` and
    /// leave the affected image on its original markdown url.
    async fn upload_page_assets(
        &mut self,
        content: &str,
        markdown_dir: &Path,
        page_view_id: &str,
    ) -> Result<HashMap<String, String>> {
        let references = scan_image_references(content);

        // Resolve every unique local reference in one blocking task:
        // `canonicalize` + `is_file` can stall on a slow or network disk, and
        // a page may hold hundreds of image urls. Deduplicating first also
        // avoids canonicalizing the same file once per occurrence.
        let mut seen_local = HashSet::new();
        let mut local_urls: Vec<String> = Vec::new();
        for (url, _) in &references {
            if !is_remote_url(url) && seen_local.insert(url.clone()) {
                local_urls.push(url.clone());
            }
        }
        let resolved = resolve_local_assets(markdown_dir, &local_urls).await?;

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
            let path = match resolved.get(&url) {
                Some(path) => path.clone(),
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
        Ok(url_map)
    }

    /// Mirrors `_read_markdown`: decode as UTF-8 with BOM stripping
    /// (`utf-8-sig`); on invalid UTF-8 warn and decode lossy. The blocking
    /// `fs::read` runs via `spawn_blocking` so a large file on a slow disk
    /// cannot stall the tokio workers.
    async fn read_markdown(&mut self, path: &Path) -> String {
        let owned = path.to_path_buf();
        let read = tokio::task::spawn_blocking(move || fs::read(&owned)).await;
        // A JoinError is only possible if the closure panicked (`fs::read`
        // cannot); fold it into the ordinary read-error path below.
        let read = read.unwrap_or_else(|join_error| Err(std::io::Error::other(join_error)));
        let bytes = match read {
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

/// Test-only probe recording the OS thread that performed the blocking path
/// work. The regression tests use it to prove `canonicalize`/`is_file` run on
/// a `spawn_blocking` thread rather than a tokio worker.
#[cfg(test)]
mod blocking_probe {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static USER_PATH: AtomicU64 = AtomicU64::new(0);
    pub static LOCAL_ASSET: AtomicU64 = AtomicU64::new(0);

    /// Hashed `ThreadId` — stable for the life of the process, comparable
    /// across calls without exposing `ThreadId` itself.
    pub fn thread_hash() -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::thread::current().id().hash(&mut hasher);
        hasher.finish()
    }

    pub fn record(slot: &AtomicU64) {
        slot.store(thread_hash(), Ordering::SeqCst);
    }
}

/// `Path(input).expanduser().resolve()` without requiring existence:
/// canonicalize when possible, else fall back to an absolute (cwd-joined)
/// path so validation errors still carry a full path. The Windows verbatim
/// (`\\?\`) prefix from `canonicalize` is stripped so reported paths read
/// like Python's `str(Path.resolve())`.
///
/// Blocking (canonicalize may hit a slow network filesystem): call it from
/// [`resolve_user_path`] or another blocking context, never directly from an
/// async task.
fn resolve_user_path_blocking(input: &str) -> PathBuf {
    let expanded = expand_tilde(input);
    match fs::canonicalize(&expanded) {
        Ok(p) => strip_verbatim_prefix(p),
        Err(_) if expanded.is_absolute() => expanded,
        Err(_) => std::env::current_dir()
            .map(|cwd| cwd.join(&expanded))
            .unwrap_or(expanded),
    }
}

/// Async wrapper around [`resolve_user_path_blocking`] so canonicalizing a
/// user-supplied (possibly network-mounted) path cannot stall a tokio worker.
async fn resolve_user_path(input: &str) -> Result<PathBuf> {
    let owned = input.to_string();
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        blocking_probe::record(&blocking_probe::USER_PATH);
        resolve_user_path_blocking(&owned)
    })
    .await
    .map_err(|join_error| Error::Other(format!("could not resolve path {input:?}: {join_error}")))
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

/// Resolve every local image url for one page in a single blocking task, so
/// `canonicalize` + `is_file` never run on a tokio worker. Urls whose file
/// cannot be canonicalized are simply absent from the map (the "image not
/// found" case); an Err here is the blocking task itself failing, which the
/// caller records on the page rather than misreporting every image missing.
async fn resolve_local_assets(
    markdown_dir: &Path,
    urls: &[String],
) -> Result<HashMap<String, PathBuf>> {
    if urls.is_empty() {
        return Ok(HashMap::new());
    }
    let dir = markdown_dir.to_path_buf();
    let urls = urls.to_vec();
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        blocking_probe::record(&blocking_probe::LOCAL_ASSET);
        let mut resolved = HashMap::with_capacity(urls.len());
        for url in urls {
            if let Some(path) = resolve_local_asset_blocking(&dir, &url) {
                resolved.insert(url, path);
            }
        }
        resolved
    })
    .await
    .map_err(|join_error| {
        Error::Other(format!(
            "could not resolve image assets under {}: {join_error}",
            markdown_dir.display()
        ))
    })
}

/// Mirrors `_resolve_local_asset`: urlsplit + unquote, `\` → `/`, relative to
/// the markdown directory, resolve, then `is_file`.
///
/// Blocking: only call it from [`resolve_local_assets`] or another blocking
/// context.
fn resolve_local_asset_blocking(markdown_dir: &Path, image_url: &str) -> Option<PathBuf> {
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

/// Single-DFS replacement for Python's per-call `_subtree_has_markdown`:
/// collects every directory (reachable from `root` without crossing an
/// ignored or dot-prefixed child) whose subtree contains at least one
/// Markdown file. The import walk used to invoke the per-call variant once
/// per ancestor level, re-walking the whole subtree each time —
/// O(n × depth) directory visits — so the set is computed once up front and
/// each level consults it in O(1).
///
/// Semantics match `_subtree_has_markdown` exactly for every directory this
/// flow queries: the ignored/dot filter applies to *children* of a scanned
/// directory (the start directory itself is always scanned), and children
/// reached through `scan_dir` are pre-filtered, so an ignored directory is
/// never looked up in a set built without it.
fn markdown_subtree_dirs(root: &Path) -> HashSet<PathBuf> {
    let mut dirs = HashSet::new();
    collect_markdown_dirs(root, &mut dirs);
    dirs
}

/// Total Markdown files under `root`, honouring the exact ignore rules the
/// import walk applies ([`scan_dir`]'s suffix and directory filters) —
/// the denominator of the import progress bar. Every counted file ticks
/// exactly once during the import: as an index file folded into its folder
/// page, or as a sibling page.
fn count_markdown_files(root: &Path) -> usize {
    let mut total = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let (_, markdown_files, child_dirs) = scan_dir(&dir);
        total += markdown_files.len();
        stack.extend(child_dirs);
    }
    total
}

/// Post-order DFS marking every Markdown-bearing ancestor. Returns whether
/// `directory_path`'s subtree holds Markdown so parents inherit the mark.
fn collect_markdown_dirs(directory_path: &Path, dirs: &mut HashSet<PathBuf>) -> bool {
    let (_, markdown_files, child_dirs) = scan_dir(directory_path);
    let mut has_markdown = !markdown_files.is_empty();
    for child in &child_dirs {
        // Visit every child even when this directory already has direct
        // Markdown: each child needs its own set entry for the level below.
        if collect_markdown_dirs(child, dirs) {
            has_markdown = true;
        }
    }
    if has_markdown {
        dirs.insert(directory_path.to_path_buf());
    }
    has_markdown
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

/// Collect the exact set of `(url, caption)` references that
/// `parse_markdown_to_blocks` will pass to its image resolver.
///
/// The scan simply runs the parser once with a recording resolver: it notes
/// every `(url, caption)` pair and returns the url unchanged. The collected
/// set is therefore identical to what the real resolver will be asked about,
/// by construction — a previous hand-written mirror of the parser's line
/// gating drifted and silently skipped images inside headings, todos, lists
/// and quotes, so those were never uploaded (BUGS.md #1). Cost: one extra
/// pure-CPU parse pass per page.
fn scan_image_references(content: &str) -> Vec<(String, String)> {
    let mut refs = Vec::new();
    let mut resolver = |url: &str, caption: &str| -> String {
        refs.push((url.to_string(), caption.to_string()));
        url.to_string()
    };
    // The blocks built here are discarded; only the recorded references are
    // kept (the real parse in `parse_markdown_with_uploads` builds them again
    // with the lookup resolver).
    let _ = parse_markdown_to_blocks(content, Some(&mut resolver));
    refs
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::sync::atomic::Ordering;

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

    // -- resolve_user_path -------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_user_path_runs_canonicalize_off_the_async_worker() {
        let dir = unique_temp_dir("resolve-user-path");
        let file = dir.join("doc.md");
        fs::write(&file, b"# doc").unwrap();

        let worker_thread = blocking_probe::thread_hash();
        let resolved = resolve_user_path(file.to_str().unwrap()).await.unwrap();
        let expected = strip_verbatim_prefix(fs::canonicalize(&file).unwrap());
        assert_eq!(resolved, expected);
        assert_ne!(
            blocking_probe::USER_PATH.load(Ordering::SeqCst),
            worker_thread,
            "canonicalize must run on a spawn_blocking thread"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_user_path_keeps_nonexistent_relative_paths_absolute() {
        let missing = format!("definitely-missing-{}.md", Uuid::new_v4());
        let resolved = resolve_user_path(&missing).await.unwrap();
        assert!(resolved.is_absolute(), "got {}", resolved.display());
        assert_eq!(
            resolved.file_name().and_then(|n| n.to_str()),
            Some(missing.as_str())
        );
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

        let found = resolve_local_asset_blocking(&dir, "assets/a%20b.png").expect("must resolve");
        assert!(found.is_file());

        // Query/fragment are stripped like urlsplit does.
        let found =
            resolve_local_asset_blocking(&dir, "assets/a%20b.png?v=2#frag").expect("must resolve");
        assert!(found.is_file());

        assert!(resolve_local_asset_blocking(&dir, "assets/missing.png").is_none());
        assert!(resolve_local_asset_blocking(&dir, "").is_none());

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_local_assets_runs_canonicalize_off_the_async_worker() {
        let dir = unique_temp_dir("resolve-off-worker");
        fs::write(dir.join("a.png"), b"png").unwrap();

        let worker_thread = blocking_probe::thread_hash();
        let urls = ["a.png".to_string(), "missing.png".to_string()];
        let resolved = resolve_local_assets(&dir, &urls).await.unwrap();
        assert!(resolved.get("a.png").expect("resolved").is_file());
        assert!(!resolved.contains_key("missing.png"));
        assert_ne!(
            blocking_probe::LOCAL_ASSET.load(Ordering::SeqCst),
            worker_thread,
            "canonicalize/is_file must run on a spawn_blocking thread"
        );

        fs::remove_dir_all(&dir).ok();
    }

    // -- scan_dir / markdown_subtree_dirs ------------------------------------

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
    fn markdown_subtree_dirs_skips_asset_only_and_ignored_dirs() {
        let tree = Tree::build();
        let dirs = markdown_subtree_dirs(&tree.root);
        assert!(dirs.contains(&tree.root));
        assert!(!dirs.contains(&tree.root.join("assets")));
        assert!(dirs.contains(&tree.root.join("sub")));
        // The ignored/dot-dir filter applies to *children* of a scanned
        // directory (matching Python): Markdown nested only inside an ignored
        // child does not count, while the directory passed as the scan root
        // itself is always scanned — so an ignored root still lands in the
        // set built from it (the flow only ever queries pre-filtered
        // children, so a set built from the import root covers every lookup
        // it performs).
        assert!(!dirs.contains(&tree.root.join("container")));
        assert!(markdown_subtree_dirs(&tree.root.join("node_modules"))
            .contains(&tree.root.join("node_modules")));
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
    fn scan_covers_images_in_every_container_line_type() {
        // Headings, todos, lists and quotes pass their inline images through
        // the resolver (via `parse_rich_blocks`), so the scan must collect
        // them too — these used to be gated out and never uploaded.
        let content = concat!(
            "# Heading ![head](head.png)\n",
            "- bullet ![list](list.png)\n",
            "- [ ] todo ![todo](todo.png)\n",
            "1. ordered ![ord](ord.png)\n",
            "> quote ![q](q.png)\n",
        );
        let mut got = urls(content);
        got.sort();
        assert_eq!(
            got,
            vec!["head.png", "list.png", "ord.png", "q.png", "todo.png"]
        );
    }

    #[test]
    fn scan_skips_regions_the_parser_never_resolves() {
        let content = concat!(
            "---\nmeta: ![fm](fm.png)\n---\n",
            "```\n![code](code.png)\n```\n",
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

    #[tokio::test]
    async fn read_markdown_strips_bom_and_decodes_lossy() {
        let dir = unique_temp_dir("read");
        let mut importer =
            MarkdownImporter::new(AppFlowyClient::new(None, None, "http://127.0.0.1:1"), "ws");

        let bom_path = dir.join("bom.md");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("# hi".as_bytes());
        fs::write(&bom_path, &bytes).unwrap();
        assert_eq!(importer.read_markdown(&bom_path).await, "# hi");

        let bad_path = dir.join("bad.md");
        fs::write(&bad_path, [0x66, 0x6F, 0xC3, 0x28]).unwrap(); // invalid UTF-8
        let text = importer.read_markdown(&bad_path).await;
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

    // ------------------------------------------------------------------
    // Regression tests for the image pre-scan bugs (see BUGS.md #1-2): the
    // scan must cover exactly the URLs the parser hands its resolver.
    // ------------------------------------------------------------------

    /// URLs the parser asks the image resolver about, in resolution order.
    fn resolved_urls(content: &str) -> Vec<String> {
        let mut resolved = Vec::new();
        let mut resolve = |url: &str, _caption: &str| {
            resolved.push(url.to_string());
            url.to_string()
        };
        parse_markdown_to_blocks(content, Some(&mut resolve));
        resolved
    }

    fn scanned_urls(content: &str) -> Vec<String> {
        scan_image_references(content)
            .into_iter()
            .map(|(url, _)| url)
            .collect()
    }

    /// The upload pre-scan must collect exactly the URLs the parser will
    /// hand its resolver. It used to gate out heading / todo / bulleted /
    /// numbered / quote lines while `parse_markdown_to_blocks` resolves
    /// images on exactly those lines
    /// (tests/markdown.rs::test_inline_images_inside_list_and_heading proves
    /// the resolver IS called there, rewriting the URL), so with
    /// `upload_assets: true`, those images were never uploaded and the
    /// imported page kept the raw local relative path as a broken image
    /// URL — silently (the file just never appeared in `uploaded_assets`).
    #[test]
    fn scan_covers_every_url_the_parser_resolves() {
        let content = concat!(
            "# Heading ![head](h.png)\n",
            "- bullet ![list](b.png)\n",
            "- [ ] todo ![todo](t.png)\n",
            "1. ordered ![ord](o.png)\n",
            "> quote ![q](q.png)\n",
            "plain paragraph ![para](p.png)\n",
        );

        let mut resolved = resolved_urls(content);
        resolved.sort();
        assert_eq!(
            resolved,
            ["b.png", "h.png", "o.png", "p.png", "q.png", "t.png"],
            "parser-side expectations changed; update this repro"
        );

        let mut scanned = scanned_urls(content);
        scanned.sort();
        assert_eq!(
            scanned, resolved,
            "pre-scan and parser disagree: these images are never uploaded"
        );
    }

    /// The importer's old local `strip_front_matter_lines` copy drained to
    /// the next `---` unconditionally, while markdown.rs's
    /// `strip_front_matter` only strips a band whose lines look like YAML. A
    /// document leading with a divider + plain text keeps that band in the
    /// parser (so its images are resolved) and the scan must agree — now
    /// guaranteed by construction, since the scan runs the very same parser.
    #[test]
    fn scan_matches_parser_front_matter_gating() {
        let content = "---\nplain text ![x](x.png)\n---\n![kept](y.png)\n";

        let mut resolved = resolved_urls(content);
        resolved.sort();
        assert_eq!(resolved, ["x.png", "y.png"]);

        let mut scanned = scanned_urls(content);
        scanned.sort();
        assert_eq!(
            scanned, resolved,
            "pre-scan and parser disagree on non-YAML front matter"
        );
    }
}

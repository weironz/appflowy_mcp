//! P8: document content editing tools.
//!
//! All tools operate on a page's Document collab
//! ([`collab_doc::COLLAB_TYPE_DOCUMENT`]) over the web-update route proven by
//! [`crate::collab_edit`]: fetch collab → mutate the yrs doc
//! ([`crate::collab_doc`]) → push the diff since the fetched state vector.
//! The rollback story: every mutating tool accepts `snapshot_path` to save
//! the pre-edit full document state, and `appflowy_restore_document_snapshot`
//! pushes such a snapshot back as a whole-state update.

use std::path::Path;

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use yrs::{Doc, ReadTxn, Transact};

use crate::collab_doc;
use crate::collab_edit;
use crate::error::Error;
use crate::server::AppServer;
use crate::tools::util::{map_tool_error, ToolError};

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct UpdateDocumentBlockRequest {
    pub workspace_id: String,
    /// The page whose document collab holds the block.
    pub page_id: String,
    /// Block id inside the document's blocks map.
    pub block_id: String,
    /// New whole-block text, with markdown inline semantics (**bold**,
    /// *italic*, ~~strike~~, `code`, [label](url)); empty clears the block.
    pub text: String,
    /// Optional local path: before editing, save the full pre-edit document
    /// state (base64 doc_state + timestamp) there; restore with
    /// appflowy_restore_document_snapshot. Parent directories are created.
    pub snapshot_path: Option<String>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct DeleteDocumentBlockRequest {
    pub workspace_id: String,
    /// The page whose document collab holds the block.
    pub page_id: String,
    /// Block id to detach from its parent's children list.
    pub block_id: String,
    /// Optional local path: before deleting, save the full pre-edit document
    /// state (base64 doc_state + timestamp) there; restore with
    /// appflowy_restore_document_snapshot. Parent directories are created.
    pub snapshot_path: Option<String>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct SnapshotDocumentRequest {
    pub workspace_id: String,
    /// The page whose document collab is snapshotted.
    pub page_id: String,
    /// Local path for the snapshot file; parent directories are created.
    pub snapshot_path: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct RestoreDocumentSnapshotRequest {
    pub workspace_id: String,
    /// Must equal the page_id stored inside the snapshot file.
    pub page_id: String,
    /// Snapshot file written by snapshot_path / appflowy_snapshot_document.
    pub snapshot_path: String,
}

#[tool_router(router = docedit_tool_router, vis = "pub(crate)")]
impl AppServer {
    /// Save the full pre-edit document state when `snapshot_path` is given.
    /// The file is written *before* any mutation so it is both audit
    /// evidence and the restore payload.
    async fn save_snapshot_if_requested(
        &self,
        doc: &Doc,
        workspace_id: &str,
        page_id: &str,
        snapshot_path: Option<&str>,
    ) -> Result<Value, ToolError> {
        let Some(path) = snapshot_path else {
            return Ok(Value::Null);
        };
        if path.trim().is_empty() {
            return Err(ToolError::validation(
                "snapshot_path must not be empty when given.",
            ));
        }
        let (envelope, _) = collab_doc::write_snapshot(doc, workspace_id, page_id, Path::new(path))
            .map_err(map_tool_error)?;
        Ok(json!({
            "snapshot_path": path,
            "snapshot_created_at": envelope["created_at"],
            "snapshot_doc_state_bytes": envelope["doc_state_bytes"],
        }))
    }

    /// Fetch a page's document collab and validate the flat ids.
    async fn fetch_document(&self, workspace_id: &str, page_id: &str) -> Result<Doc, ToolError> {
        if workspace_id.is_empty() {
            return Err(ToolError::validation("workspace_id must not be empty."));
        }
        if page_id.is_empty() {
            return Err(ToolError::validation("page_id must not be empty."));
        }
        collab_edit::fetch_collab_doc(
            &self.client,
            workspace_id,
            page_id,
            collab_doc::COLLAB_TYPE_DOCUMENT,
        )
        .await
        .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_update_document_block",
        description = "Replace the ENTIRE text of one document block (gray semantics: whole-paragraph replacement, NOT an incremental/caret edit) through the document collab, the way the webclient edits save. `text` is parsed with markdown inline semantics (**bold**, *italic*, ~~strike~~, `code`, [label](url)); formatting the new text does not re-declare is dropped. Only text-bearing block types are supported: paragraph, heading, quote, todo_list, bulleted_list, numbered_list — anything else is rejected without writing. An empty `text` clears the block. Pass `snapshot_path` to save the pre-edit state for appflowy_restore_document_snapshot before the change."
    )]
    async fn appflowy_update_document_block(
        &self,
        Parameters(request): Parameters<UpdateDocumentBlockRequest>,
    ) -> Result<Json<Value>, ToolError> {
        if request.block_id.is_empty() {
            return Err(ToolError::validation("block_id must not be empty."));
        }
        let doc = self
            .fetch_document(&request.workspace_id, &request.page_id)
            .await?;
        let snapshot = self
            .save_snapshot_if_requested(
                &doc,
                &request.workspace_id,
                &request.page_id,
                request.snapshot_path.as_deref(),
            )
            .await?;
        let since = doc.transact().state_vector();
        let external_id = collab_doc::update_block_text(&doc, &request.block_id, &request.text)
            .map_err(map_tool_error)?;
        let update_bytes = collab_edit::push_web_update(
            &self.client,
            &request.workspace_id,
            &request.page_id,
            collab_doc::COLLAB_TYPE_DOCUMENT,
            &doc,
            &since,
        )
        .await
        .map_err(map_tool_error)?;
        Ok(Json(json!({
            "page_id": request.page_id,
            "block_id": request.block_id,
            "external_id": external_id,
            "update_bytes": update_bytes,
            "snapshot": snapshot,
            "undo": "keep the snapshot file; appflowy_restore_document_snapshot returns the document to it.",
        })))
    }

    #[tool(
        name = "appflowy_delete_document_block",
        description = "Detach a block from its parent's children list through the document collab (gray semantics: the block record and its subtree stay in the collab as unreachable orphans — this is the conservative form of the GUI's delete, with no recursive cleanup — so only an older snapshot can bring it back). Nested list items work: whichever children array holds the id is edited. Pass `snapshot_path` to save the pre-edit state for appflowy_restore_document_snapshot before the change."
    )]
    async fn appflowy_delete_document_block(
        &self,
        Parameters(request): Parameters<DeleteDocumentBlockRequest>,
    ) -> Result<Json<Value>, ToolError> {
        if request.block_id.is_empty() {
            return Err(ToolError::validation("block_id must not be empty."));
        }
        let doc = self
            .fetch_document(&request.workspace_id, &request.page_id)
            .await?;
        let snapshot = self
            .save_snapshot_if_requested(
                &doc,
                &request.workspace_id,
                &request.page_id,
                request.snapshot_path.as_deref(),
            )
            .await?;
        let since = doc.transact().state_vector();
        let outcome = collab_doc::delete_block_from_parent(&doc, &request.block_id)
            .map_err(map_tool_error)?;
        let update_bytes = collab_edit::push_web_update(
            &self.client,
            &request.workspace_id,
            &request.page_id,
            collab_doc::COLLAB_TYPE_DOCUMENT,
            &doc,
            &since,
        )
        .await
        .map_err(map_tool_error)?;
        Ok(Json(json!({
            "page_id": request.page_id,
            "block_id": request.block_id,
            "detached_from_children": outcome.children_id,
            "index": outcome.index,
            "update_bytes": update_bytes,
            "snapshot": snapshot,
            "undo": "keep the snapshot file; appflowy_restore_document_snapshot returns the document to it.",
        })))
    }

    #[tool(
        name = "appflowy_snapshot_document",
        description = "Save a page's FULL document state (base64 doc_state plus timestamp) to a local file — the rollback anchor for document edits. The collab edit tools also accept `snapshot_path` to do this inline before mutating; restore with appflowy_restore_document_snapshot. The whole state costs the full collab size (unlike per-edit diffs), so use it per decision point, not per keystroke."
    )]
    async fn appflowy_snapshot_document(
        &self,
        Parameters(request): Parameters<SnapshotDocumentRequest>,
    ) -> Result<Json<Value>, ToolError> {
        if request.snapshot_path.trim().is_empty() {
            return Err(ToolError::validation("snapshot_path must not be empty."));
        }
        let doc = self
            .fetch_document(&request.workspace_id, &request.page_id)
            .await?;
        let (envelope, _) = collab_doc::write_snapshot(
            &doc,
            &request.workspace_id,
            &request.page_id,
            Path::new(&request.snapshot_path),
        )
        .map_err(map_tool_error)?;
        Ok(Json(json!({
            "page_id": request.page_id,
            "snapshot_path": request.snapshot_path,
            "snapshot_created_at": envelope["created_at"],
            "snapshot_doc_state_bytes": envelope["doc_state_bytes"],
        })))
    }

    #[tool(
        name = "appflowy_restore_document_snapshot",
        description = "Restore the visible document body to a saved snapshot's content (CRDT-honest semantics — NOT a merge and NOT a raw state push-back, which cannot revive tombstoned content): every current root block is detached into invisible history and the snapshot is rebuilt from its decoded markdown as entirely fresh blocks (fresh ids). Later edits made after the snapshot is gone from view; invisible CRDT history is preserved. Fresh rebuild instantiates new yjs items, so afterwards the visible document equals the snapshot. `page_id` must match the snapshot's."
    )]
    async fn appflowy_restore_document_snapshot(
        &self,
        Parameters(request): Parameters<RestoreDocumentSnapshotRequest>,
    ) -> Result<Json<Value>, ToolError> {
        if request.snapshot_path.trim().is_empty() {
            return Err(ToolError::validation("snapshot_path must not be empty."));
        }
        if request.workspace_id.is_empty() {
            return Err(ToolError::validation("workspace_id must not be empty."));
        }
        if request.page_id.is_empty() {
            return Err(ToolError::validation("page_id must not be empty."));
        }
        let raw = tokio::fs::read(&request.snapshot_path)
            .await
            .map_err(Error::Io)
            .map_err(map_tool_error)?;
        let snapshot = collab_doc::parse_snapshot(&raw).map_err(map_tool_error)?;
        if snapshot.page_id != request.page_id {
            return Err(ToolError::validation(format!(
                "Snapshot belongs to page {}, not {} (no changes were written).",
                snapshot.page_id, request.page_id
            )));
        }

        // Rebuild flow rather than a whole-state push-back: with monotone yjs
        // delete sets a stale doc_state cannot un-delete anything (verified
        // live), so the snapshot must be decoded to markdown, re-parsed into
        // fresh blocks, and written over a detached-current document.
        let snapshot_document =
            crate::collab::decode_document(&snapshot.doc_state).map_err(map_tool_error)?;
        let markdown = crate::render::document_to_markdown(&snapshot_document, None);
        let blocks = crate::markdown::parse_markdown_to_blocks(&markdown, None);

        let doc = self
            .fetch_document(&request.workspace_id, &request.page_id)
            .await?;
        let since = doc.transact().state_vector();
        let outcome =
            collab_doc::rebuild_document_from_blocks(&doc, &blocks).map_err(map_tool_error)?;
        let update_bytes = collab_edit::push_web_update(
            &self.client,
            &request.workspace_id,
            &request.page_id,
            collab_doc::COLLAB_TYPE_DOCUMENT,
            &doc,
            &since,
        )
        .await
        .map_err(map_tool_error)?;

        Ok(Json(json!({
            "page_id": request.page_id,
            "restored_from": request.snapshot_path,
            "snapshot_created_at": snapshot.created_at,
            "rebuilt_blocks": outcome.rebuilt_blocks,
            "removed_existing": outcome.removed_existing,
            "warnings": outcome.warnings,
            "update_bytes": update_bytes,
            "restored": true,
        })))
    }
}

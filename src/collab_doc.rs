//! P8: document content editing through the collab (yjs) layer — pure logic.
//!
//! A Document collab ([`COLLAB_TYPE_DOCUMENT`]) is shaped as
//! `data.document { page_id, blocks, meta }`
//! (see [`crate::collab::decode_document`] for the read side):
//!
//! * `blocks[block_id]` — a Map `{ty, data, children, external_id?, ...}`.
//!   `external_id` is the key into the text map for text-bearing blocks.
//! * `meta.text_map[external_id]` — a shared YText holding the block's rich
//!   text as yjs chunks (formatting attrs ride on each chunk).
//! * `meta.children_map[children_key]` — a YArray of child block ids; every
//!   block's `children` field names its own children key. Deleting a block in
//!   the GUI is removing its id from the parent array.
//!
//! The collab-edit route for every caller is: fetch the collab
//! ([`crate::collab_edit::fetch_collab_doc`]) → mutate via this module →
//! [`crate::collab_edit::push_web_update`] with the state vector captured
//! before the mutation. The pushed diff is a few hundred bytes for a single
//! text edit; the snapshot route re-encodes the *whole* document state, which
//! costs the full collab size (KBs to low MBs as JSON ints ≈ 3–7× the binary
//! size) — acceptable as a one-shot recovery anchor, pointless per edit.
//!
//! Restore is content-level **rebuild**, never a state push-back: yjs delete
//! sets are monotone, so pushing a whole older state cannot revive content
//! deleted after the snapshot (verified against a real server — deleted
//! blocks stay dead, later text edits survive as merge residue). The working
//! CRDT semantics ([`rebuild_document_from_blocks`], driven by
//! `appflowy_restore_document_snapshot`) are: detach every root-level child
//! of the current document — the same conservative detach as
//! [`delete_block_from_parent`], batched, old blocks become invisible
//! tombstones — and rebuild the snapshot's content (decoded via
//! [`crate::collab::decode_document`], round-tripped through markdown for
//! inline formatting) as entirely fresh blocks with fresh ids. This is the
//! strongest restore CRDT semantics allow; it is not a merge and does not
//! undo individual edits.

use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use yrs::types::text::TextPrelim;
use yrs::types::{Attrs, Delta};
use yrs::{
    Any, Array, ArrayPrelim, ArrayRef, Doc, In, Map, MapPrelim, MapRef, Out, ReadTxn, StateVector,
    Text, TextRef, Transact, TransactionMut,
};

use crate::b64::base64_decode;
use crate::delta::DeltaOp;
use crate::error::{Error, Result};
use crate::markdown::{parse_rich_text, Block};

/// CollabType::Document — a page's block tree.
pub const COLLAB_TYPE_DOCUMENT: i64 = 0;

/// Block types whose text lives in `meta.text_map` and can be rewritten by
/// [`update_block_text`]. Container/media types (divider, image, table
/// cells...) either carry no text or store it outside the text map.
pub const TEXT_BEARING_BLOCK_TYPES: [&str; 6] = [
    "paragraph",
    "heading",
    "quote",
    "todo_list",
    "bulleted_list",
    "numbered_list",
];

/// Snapshot envelope version written to `snapshot_path` files.
pub const SNAPSHOT_VERSION: i64 = 1;

fn ymap(out: Option<Out>, what: &str) -> Result<MapRef> {
    match out {
        Some(Out::YMap(map)) => Ok(map),
        other => Err(Error::Other(format!(
            "collab path {what} is not a map (found {other:?})"
        ))),
    }
}

/// Resolve `data.document`; the returned ref is used in a later write
/// transaction (refs resolve read-only, so no store-lock inversion).
fn document_map(doc: &Doc) -> Result<MapRef> {
    let root = doc.get_or_insert_map("data");
    let txn = doc.transact();
    ymap(
        root.get(&txn, "document"),
        "data/document (not a document collab?)",
    )
}

/// Replace the whole text of one block with new markdown-parsed rich text.
///
/// Semantics are *replace the paragraph*, not caret editing: the existing
/// YText is cleared and re-inserted from `parse_rich_text(text)`, so inline
/// formatting that the new text does not re-declare is dropped. Returns the
/// block's `external_id` (its text_map key).
///
/// Some document collabs carry no `last_modified` on blocks; the timestamp
/// is only refreshed when the key already exists, keeping its original
/// scalar type (string vs number), same rule as row cells.
pub fn update_block_text(doc: &Doc, block_id: &str, text: &str) -> Result<String> {
    let document = document_map(doc)?;
    // Resolve refs and validate in a read transaction first, so nothing is
    // written when the block is missing or of the wrong type.
    let (block, text_ref, external_id, stamp) = {
        let txn = doc.transact();
        let blocks = ymap(document.get(&txn, "blocks"), "document/blocks")?;
        let block = match blocks.get(&txn, block_id) {
            Some(Out::YMap(block)) => block,
            _ => {
                return Err(Error::Validation(format!(
                    "Block {block_id} is not in this document (no changes were written)."
                )))
            }
        };
        let ty = match block.get(&txn, "ty") {
            Some(Out::Any(Any::String(ty))) => ty.to_string(),
            _ => String::new(),
        };
        if !TEXT_BEARING_BLOCK_TYPES.contains(&ty.as_str()) {
            return Err(Error::Validation(format!(
                "Block {block_id} has type {ty:?}, which is not text-bearing; \
                 supported types: {} (no changes were written).",
                TEXT_BEARING_BLOCK_TYPES.join(", ")
            )));
        }
        let external_id = match block.get(&txn, "external_id") {
            Some(Out::Any(Any::String(id))) if !id.is_empty() => id.to_string(),
            _ => {
                return Err(Error::Validation(format!(
                    "Block {block_id} has no external_id and therefore no text entry \
                     (no changes were written)."
                )))
            }
        };
        let meta = ymap(document.get(&txn, "meta"), "document/meta")?;
        let text_map = ymap(meta.get(&txn, "text_map"), "meta/text_map")?;
        let text_ref = match text_map.get(&txn, &external_id) {
            Some(Out::YText(text)) => text,
            _ => {
                return Err(Error::Validation(format!(
                    "Block {block_id} has no text entry in the text map \
                     (external_id {external_id}; no changes were written)."
                )))
            }
        };
        let stamp = match block.get(&txn, "last_modified") {
            Some(Out::Any(Any::String(_))) => Some(Any::String(
                crate::collab_edit::now_unix().to_string().into(),
            )),
            Some(_) => Some(Any::from(crate::collab_edit::now_unix())),
            None => None,
        };
        (block, text_ref, external_id, stamp)
    };

    let mut txn = doc.transact_mut();
    let len = text_ref.len(&txn);
    if len > 0 {
        text_ref.remove_range(&mut txn, 0, len);
    }
    apply_delta_ops(&text_ref, &mut txn, &parse_rich_text(text))?;
    if let Some(stamp) = stamp {
        block.insert(&mut txn, "last_modified", In::from(stamp));
    }
    txn.commit();
    Ok(external_id)
}

/// Convert a JSON value to yrs [`Any`] by reference, avoiding the deep
/// `value.clone()` that `serde_json::from_value` (taking the value by value)
/// forced on every rich-text attribute.
///
/// Behavior is identical to what yrs' own serde `Deserialize` impl produced:
/// serde_json feeds its unsigned number repr to `visit_u64` (which yrs only
/// accepts up to i64::MAX, with this exact error wording), negatives to
/// `visit_i64`, and floats to `visit_f64`.
fn json_to_any(value: &Value) -> std::result::Result<Any, String> {
    Ok(match value {
        Value::Null => Any::Null,
        Value::Bool(b) => Any::Bool(*b),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                if u > i64::MAX as u64 {
                    return Err(format!("Value {u} out of range for i64"));
                }
                Any::from(u as i64)
            } else if let Some(i) = n.as_i64() {
                Any::from(i)
            } else {
                // serde_json keeps every remaining number as an f64.
                Any::from(n.as_f64().unwrap_or_default())
            }
        }
        Value::String(s) => Any::from(s.as_str()),
        Value::Array(items) => Any::Array(
            items
                .iter()
                .map(json_to_any)
                .collect::<std::result::Result<Vec<_>, _>>()?
                .into(),
        ),
        Value::Object(map) => {
            let mut out = std::collections::HashMap::with_capacity(map.len());
            for (key, value) in map {
                out.insert(key.clone(), json_to_any(value)?);
            }
            Any::Map(Arc::new(out))
        }
    })
}

/// Insert rich-text ops into a YText. The whole delta goes in through ONE
/// apply_delta call: its attr bookkeeping (unset_missing /
/// minimize_attr_changes) opens and closes formatting ranges between chunks.
/// Mixing `insert` / `insert_with_attributes` leaves an open format marker
/// from the previous chunk covering the plain chunk after it — observed
/// directly in `update_block_text_diffs_and_applies_back`. Empty chunks are
/// dropped (a wholly empty delta means an empty YText).
fn apply_delta_ops(text_ref: &TextRef, txn: &mut TransactionMut, ops: &[DeltaOp]) -> Result<()> {
    let mut delta = Vec::new();
    for DeltaOp { insert, attributes } in ops {
        if insert.is_empty() {
            continue;
        }
        let attrs: Option<Box<Attrs>> = match attributes {
            None => None,
            Some(map) if map.is_empty() => None,
            Some(map) => {
                let mut out = Attrs::new();
                for (key, value) in map {
                    let any: Any = json_to_any(value).map_err(|e| {
                        Error::Other(format!(
                            "rich-text attribute {key} not represented in the collab: {e}"
                        ))
                    })?;
                    out.insert(Arc::from(key.as_str()), any);
                }
                Some(Box::new(out))
            }
        };
        delta.push(Delta::Inserted(In::from(insert.as_str()), attrs));
    }
    text_ref.apply_delta(txn, delta);
    Ok(())
}

/// Where a deleted block used to sit, for the tool response.
#[derive(Debug)]
pub struct DeleteOutcome {
    /// The `meta.children_map` key (the parent's children array) the block
    /// id was removed from.
    pub children_id: String,
    /// Zero-based position the block occupied in that array.
    pub index: u32,
}

/// Detach `block_id` from its parent's children array — the conservative
/// form of the GUI's delete: the block record in `blocks` and its own
/// children subtree stay in the collab as unreachable orphans (no recursive
/// cleanup, so a later push of an older state can re-link them).
///
/// A block in a nested list lives in its parent item's children array, so
/// nested blocks need no special handling: whichever array holds the id is
/// the one edited. If the id somehow sits in several arrays, only the first
/// match is detached (a well-formed document links each block exactly once).
pub fn delete_block_from_parent(doc: &Doc, block_id: &str) -> Result<DeleteOutcome> {
    let document = document_map(doc)?;
    let (children_id, array, index) = {
        let txn = doc.transact();
        let blocks = ymap(document.get(&txn, "blocks"), "document/blocks")?;
        if !matches!(blocks.get(&txn, block_id), Some(Out::YMap(_))) {
            return Err(Error::Validation(format!(
                "Block {block_id} is not in this document (no changes were written)."
            )));
        }
        let meta = ymap(document.get(&txn, "meta"), "document/meta")?;
        let children_map = ymap(meta.get(&txn, "children_map"), "meta/children_map")?;
        let mut found: Option<(String, ArrayRef, u32)> = None;
        for (key, out) in children_map.iter(&txn) {
            let Out::YArray(array) = out else {
                continue;
            };
            // A single linear scan: Array::get(index) re-walks the array
            // from its head per call, so an index loop over a long children
            // list is O(n^2).
            let index = array.iter(&txn).position(|out| match out {
                Out::Any(Any::String(id)) => id.as_ref() == block_id,
                other => other.to_string(&txn) == block_id,
            });
            if let Some(index) = index {
                found = Some((key.to_string(), array, index as u32));
                break;
            }
        }
        found.ok_or_else(|| {
            Error::Validation(format!(
                "Block {block_id} exists but is not linked in any children list \
                 (already detached? no changes were written)."
            ))
        })?
    };

    let mut txn = doc.transact_mut();
    array.remove(&mut txn, index);
    txn.commit();
    Ok(DeleteOutcome { children_id, index })
}

// ---------------------------------------------------------------------------
// rebuild-from-snapshot restore
// ---------------------------------------------------------------------------

/// Text-bearing block types rebuilt with a fresh YText in the text map —
/// [`TEXT_BEARING_BLOCK_TYPES`] plus `code` (its delta is plain text, rich
/// runs survive the markdown round-trip as literal characters).
fn rebuild_text_bearing(ty: &str) -> bool {
    TEXT_BEARING_BLOCK_TYPES.contains(&ty) || ty == "code"
}

/// Non-text block types rebuilt by passing their `data` object through as-is.
pub const REBUILD_DATA_ONLY_BLOCK_TYPES: [&str; 3] = ["divider", "image", "math_equation"];

/// What [`rebuild_document_from_blocks`] did, for the tool response.
#[derive(Debug)]
pub struct RebuildOutcome {
    /// Fresh blocks created from the snapshot (nested children included).
    pub rebuilt_blocks: u32,
    /// Root-level children detached from the current document (they stay in
    /// the collab as invisible tombstones).
    pub removed_existing: u32,
    /// Unsupported snapshot content (e.g. tables) that was skipped.
    pub warnings: Vec<String>,
}

fn new_collab_id() -> String {
    // Fresh ids are essential: reusing an old block/text id would merge with
    // its tombstone instead of appearing as clean new content.
    uuid::Uuid::new_v4().to_string()
}

/// The rich-text ops of a parsed [`Block`] (`data.delta`), tolerating
/// missing/malformed entries by dropping them.
fn block_delta_ops(block: &Block) -> Vec<DeltaOp> {
    block
        .data
        .get("delta")
        .and_then(Value::as_array)
        .map(|ops| {
            ops.iter()
                .filter_map(|op| serde_json::from_value::<DeltaOp>(op.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Recursive post-order build of one [`Block`]: children are created first
/// so each `children_map` array is inserted once with its full contents.
/// Returns the new block id, or `None` for unsupported types (recorded as a
/// warning including the skipped subtree size).
fn rebuild_one(
    txn: &mut TransactionMut,
    blocks: &MapRef,
    children_map: &MapRef,
    text_map: &MapRef,
    block: &Block,
    outcome: &mut RebuildOutcome,
) -> Result<Option<String>> {
    let ty = block.ty.as_str();
    let text_bearing = rebuild_text_bearing(ty);
    if !(text_bearing || REBUILD_DATA_ONLY_BLOCK_TYPES.contains(&ty)) {
        let mut skipped = 1usize;
        let mut stack: Vec<&Block> = block.children.iter().collect();
        while let Some(child) = stack.pop() {
            skipped += 1;
            stack.extend(child.children.iter());
        }
        outcome.warnings.push(format!(
            "skipped unsupported block type {ty:?} ({skipped} block(s) total)"
        ));
        return Ok(None);
    }

    let mut child_ids = Vec::with_capacity(block.children.len());
    for child in &block.children {
        if let Some(id) = rebuild_one(txn, blocks, children_map, text_map, child, outcome)? {
            child_ids.push(id);
        }
    }

    let block_id = new_collab_id();
    let children_id = new_collab_id();
    let data = serde_json::to_string(&block.data).unwrap_or_else(|_| "{}".to_string());
    let entry = blocks.insert(txn, block_id.as_str(), empty_map());
    entry.insert(txn, "ty", In::from(ty));
    entry.insert(txn, "data", In::from(data));
    entry.insert(txn, "children", In::from(children_id.as_str()));
    children_map.insert(
        txn,
        children_id.as_str(),
        ArrayPrelim::from(child_ids.into_iter().map(In::from).collect::<Vec<_>>()),
    );
    if text_bearing {
        let external_id = new_collab_id();
        entry.insert(txn, "external_id", In::from(external_id.as_str()));
        let text_ref = text_map.insert(txn, external_id.as_str(), TextPrelim::new(""));
        apply_delta_ops(&text_ref, txn, &block_delta_ops(block))?;
    }
    outcome.rebuilt_blocks += 1;
    Ok(Some(block_id))
}

fn empty_map() -> MapPrelim {
    MapPrelim::from_iter(Vec::<(&str, In)>::new())
}

/// Restore the visible document body to `new_blocks` (the snapshot content,
/// markdown round-tripped by the caller): every root-level child of the
/// current document is detached — batched [`delete_block_from_parent`], the
/// CRDT-mandated way to remove under monotone delete sets — and the
/// snapshot is rebuilt as entirely fresh blocks with fresh ids.
pub fn rebuild_document_from_blocks(doc: &Doc, new_blocks: &[Block]) -> Result<RebuildOutcome> {
    let document = document_map(doc)?;
    // Resolve every ref in a read transaction; the rebuild runs in ONE write
    // transaction so the diff is a single update.
    let (blocks, children_map, text_map, root_children) = {
        let txn = doc.transact();
        let blocks = ymap(document.get(&txn, "blocks"), "document/blocks")?;
        let page_id = match document.get(&txn, "page_id") {
            Some(Out::Any(Any::String(id))) => id.to_string(),
            _ => return Err(Error::Other("collab document is missing page_id".into())),
        };
        let root_children_key = match blocks.get(&txn, &page_id) {
            Some(Out::YMap(page)) => match page.get(&txn, "children") {
                Some(Out::Any(Any::String(key))) => key.to_string(),
                _ => return Err(Error::Other("root page block has no children key".into())),
            },
            _ => {
                return Err(Error::Other(format!(
                    "document collab has no root page block {page_id}"
                )))
            }
        };
        let meta = ymap(document.get(&txn, "meta"), "document/meta")?;
        let children_map = ymap(meta.get(&txn, "children_map"), "meta/children_map")?;
        let text_map = ymap(meta.get(&txn, "text_map"), "meta/text_map")?;
        let root_children = match children_map.get(&txn, &root_children_key) {
            Some(Out::YArray(array)) => array,
            _ => {
                return Err(Error::Other(format!(
                    "root children list {root_children_key} missing from children_map"
                )))
            }
        };
        (blocks, children_map, text_map, root_children)
    };

    let mut txn = doc.transact_mut();
    let removed_existing = root_children.len(&txn);
    if removed_existing > 0 {
        // One range removal instead of len × remove(0): each single removal
        // re-walks all previously accumulated tombstones, making the
        // one-at-a-time loop O(n^2) in the document's total block count.
        root_children.remove_range(&mut txn, 0, removed_existing);
    }

    let mut outcome = RebuildOutcome {
        rebuilt_blocks: 0,
        removed_existing,
        warnings: Vec::new(),
    };
    let mut new_root_ids = Vec::with_capacity(new_blocks.len());
    for block in new_blocks {
        if let Some(id) = rebuild_one(
            &mut txn,
            &blocks,
            &children_map,
            &text_map,
            block,
            &mut outcome,
        )? {
            new_root_ids.push(id);
        }
    }
    for id in new_root_ids {
        root_children.push_back(&mut txn, In::from(id));
    }
    txn.commit();
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// snapshots (the rollback story)
// ---------------------------------------------------------------------------

/// Standard base64 encoder with padding (`crate::b64` only carries the
/// decoder; the alphabet matches it exactly so snapshot files round-trip).
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The snapshot envelope for a document's current full state: the complete
/// doc_state base64-encoded plus a timestamp, to be written to disk before
/// a mutation as both audit evidence and the restore payload.
pub fn snapshot_envelope(doc: &Doc, workspace_id: &str, page_id: &str) -> Value {
    // StateVector::default() = "since nothing" — the full doc state, as
    // opposed to the per-edit diff push_web_update sends.
    let bytes = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    json!({
        "version": SNAPSHOT_VERSION,
        "kind": "appflowy-document-snapshot",
        "workspace_id": workspace_id,
        "page_id": page_id,
        "created_at": crate::collab_edit::now_unix(),
        "doc_state_bytes": bytes.len(),
        "doc_state_b64": base64_encode(&bytes),
    })
}

/// Write the snapshot envelope to `path`, creating parent directories.
pub fn write_snapshot(
    doc: &Doc,
    workspace_id: &str,
    page_id: &str,
    path: &Path,
) -> Result<(Value, usize)> {
    let envelope = snapshot_envelope(doc, workspace_id, page_id);
    let bytes = serde_json::to_vec_pretty(&envelope)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &bytes)?;
    Ok((envelope, bytes.len()))
}

/// A parsed snapshot file: the page it belongs to and the full doc_state
/// bytes to push back.
pub struct Snapshot {
    pub page_id: String,
    pub workspace_id: String,
    pub created_at: i64,
    pub doc_state: Vec<u8>,
}

/// Parse a snapshot file written from [`write_snapshot`]; a malformed file is
/// a caller problem (bad path/params), so it surfaces as
/// [`Error::Validation`].
pub fn parse_snapshot(bytes: &[u8]) -> Result<Snapshot> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| Error::Validation(format!("snapshot file is not valid JSON: {e}")))?;
    if value.get("version").and_then(Value::as_i64) != Some(SNAPSHOT_VERSION)
        || value.get("kind").and_then(Value::as_str) != Some("appflowy-document-snapshot")
    {
        return Err(Error::Validation(
            "snapshot file is not an appflowy-document-snapshot (version 1); \
             take one with appflowy_snapshot_document or the snapshot_path \
             parameter of the edit tools."
                .into(),
        ));
    }
    let page_id = value
        .get("page_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if page_id.is_empty() {
        return Err(Error::Validation("snapshot file has no page_id.".into()));
    }
    let b64 = value
        .get("doc_state_b64")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Validation("snapshot file has no doc_state_b64.".into()))?;
    let doc_state = base64_decode(b64)
        .filter(|state| !state.is_empty())
        .ok_or_else(|| {
            Error::Validation("snapshot doc_state_b64 is not valid base64 data.".into())
        })?;
    Ok(Snapshot {
        page_id,
        workspace_id: value
            .get("workspace_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        created_at: value.get("created_at").and_then(Value::as_i64).unwrap_or(0),
        doc_state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::types::text::YChange;
    use yrs::updates::decoder::Decode;
    use yrs::{ArrayPrelim, GetString, MapPrelim, TextPrelim, Update};

    fn empty_map() -> MapPrelim {
        MapPrelim::from_iter(Vec::<(&str, In)>::new())
    }

    fn clone_doc(doc: &Doc) -> Doc {
        let bytes = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let clone = Doc::new();
        clone
            .transact_mut()
            .apply_update(Update::decode_v1(&bytes).unwrap())
            .unwrap();
        clone
    }

    /// Minimal linked document: page -> [b1 paragraph "hello world", b2
    /// heading "title", b3 divider], blocks/text_map/children_map all wired.
    fn fixture_doc() -> Doc {
        let doc = Doc::new();
        {
            let root = doc.get_or_insert_map("data");
            let mut txn = doc.transact_mut();
            let document = root.insert(&mut txn, "document", empty_map());
            document.insert(&mut txn, "page_id", In::from("page"));
            let blocks = document.insert(&mut txn, "blocks", empty_map());

            let page = blocks.insert(&mut txn, "page", empty_map());
            page.insert(&mut txn, "ty", In::from(""));
            page.insert(&mut txn, "children", In::from("c_page"));

            let b1 = blocks.insert(&mut txn, "b1", empty_map());
            b1.insert(&mut txn, "ty", In::from("paragraph"));
            b1.insert(&mut txn, "children", In::from("c_b1"));
            b1.insert(&mut txn, "external_id", In::from("t_b1"));

            let b2 = blocks.insert(&mut txn, "b2", empty_map());
            b2.insert(&mut txn, "ty", In::from("heading"));
            b2.insert(&mut txn, "children", In::from("c_b2"));
            b2.insert(&mut txn, "external_id", In::from("t_b2"));

            let b3 = blocks.insert(&mut txn, "b3", empty_map());
            b3.insert(&mut txn, "ty", In::from("divider"));
            b3.insert(&mut txn, "children", In::from("c_b3"));

            let meta = document.insert(&mut txn, "meta", empty_map());
            let children_map = meta.insert(&mut txn, "children_map", empty_map());
            children_map.insert(
                &mut txn,
                "c_page",
                ArrayPrelim::from([In::from("b1"), In::from("b2"), In::from("b3")]),
            );
            let text_map = meta.insert(&mut txn, "text_map", empty_map());
            text_map.insert(&mut txn, "t_b1", TextPrelim::new("hello world"));
            text_map.insert(&mut txn, "t_b2", TextPrelim::new("title"));
            txn.commit();
        }
        doc
    }

    fn text_of(doc: &Doc, external_id: &str) -> String {
        let root = doc.get_or_insert_map("data");
        let txn = doc.transact();
        let Some(Out::YMap(document)) = root.get(&txn, "document") else {
            panic!("no document");
        };
        let Some(Out::YMap(meta)) = document.get(&txn, "meta") else {
            panic!("no meta");
        };
        let Some(Out::YMap(text_map)) = meta.get(&txn, "text_map") else {
            panic!("no text_map");
        };
        match text_map.get(&txn, external_id) {
            Some(Out::YText(text)) => text.get_string(&txn),
            other => panic!("no text {external_id}: {other:?}"),
        }
    }

    fn page_children(doc: &Doc) -> Vec<String> {
        let root = doc.get_or_insert_map("data");
        let txn = doc.transact();
        let Some(Out::YMap(document)) = root.get(&txn, "document") else {
            panic!("no document");
        };
        let Some(Out::YMap(meta)) = document.get(&txn, "meta") else {
            panic!("no meta");
        };
        let Some(Out::YMap(children_map)) = meta.get(&txn, "children_map") else {
            panic!("no children_map");
        };
        match children_map.get(&txn, "c_page") {
            Some(Out::YArray(array)) => array.iter(&txn).map(|v| v.to_string(&txn)).collect(),
            other => panic!("no c_page array: {other:?}"),
        }
    }

    #[test]
    fn update_block_text_diffs_and_applies_back() {
        let base = fixture_doc();
        let doc = clone_doc(&base);
        assert_eq!(text_of(&base, "t_b1"), "hello world");

        let since = doc.transact().state_vector();
        let external_id = update_block_text(&doc, "b1", "**bold** and *ital*").unwrap();
        assert_eq!(external_id, "t_b1");
        assert_eq!(text_of(&doc, "t_b1"), "bold and ital");

        // The pre-edit doc receives exactly the change through the diff.
        let diff = doc.transact().encode_state_as_update_v1(&since);
        assert!(!diff.is_empty());
        base.transact_mut()
            .apply_update(Update::decode_v1(&diff).unwrap())
            .unwrap();
        assert_eq!(text_of(&base, "t_b1"), "bold and ital");
        assert_eq!(text_of(&base, "t_b2"), "title");

        // Parse produced formatting attrs on the first chunk.
        let root = base.get_or_insert_map("data");
        let txn = base.transact();
        let Some(Out::YMap(document)) = root.get(&txn, "document") else {
            panic!()
        };
        let Some(Out::YMap(meta)) = document.get(&txn, "meta") else {
            panic!()
        };
        let Some(Out::YMap(text_map)) = meta.get(&txn, "text_map") else {
            panic!()
        };
        let Some(Out::YText(text)) = text_map.get(&txn, "t_b1") else {
            panic!()
        };
        let chunks = text.diff(&txn, YChange::identity);
        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks[0]
                .attributes
                .as_ref()
                .and_then(|attrs| attrs.get("bold")),
            Some(&Any::Bool(true))
        );
        assert!(chunks[1].attributes.is_none());
        assert_eq!(
            chunks[2]
                .attributes
                .as_ref()
                .and_then(|attrs| attrs.get("italic")),
            Some(&Any::Bool(true))
        );
    }

    #[test]
    fn update_block_text_handles_unicode_and_empty() {
        let doc = fixture_doc();
        update_block_text(&doc, "b1", "你好 **粗体** ★").unwrap();
        assert_eq!(text_of(&doc, "t_b1"), "你好 粗体 ★");
        update_block_text(&doc, "b1", "").unwrap();
        assert_eq!(text_of(&doc, "t_b1"), "");
    }

    #[test]
    fn update_block_text_rejects_missing_and_untyped_blocks() {
        let doc = fixture_doc();
        let since = doc.transact().state_vector();
        let baseline = doc.transact().encode_state_as_update_v1(&since);
        let err = update_block_text(&doc, "nope", "x").unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "{err:?}");
        assert!(err.to_string().contains("no changes were written"));

        let err = update_block_text(&doc, "b3", "x").unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "{err:?}");
        for ty in TEXT_BEARING_BLOCK_TYPES {
            assert!(err.to_string().contains(ty), "{err}");
        }
        // Nothing was written: the failing updates produced the same
        // (trivial) diff as before them.
        let diff = doc.transact().encode_state_as_update_v1(&since);
        assert_eq!(diff, baseline);
        assert_eq!(text_of(&doc, "t_b1"), "hello world");
    }

    #[test]
    fn delete_block_detaches_from_parent_only() {
        let base = fixture_doc();
        let doc = clone_doc(&base);

        let since = doc.transact().state_vector();
        let outcome = delete_block_from_parent(&doc, "b2").unwrap();
        assert_eq!(outcome.children_id, "c_page");
        assert_eq!(outcome.index, 1);
        assert_eq!(page_children(&doc), ["b1", "b3"]);

        let diff = doc.transact().encode_state_as_update_v1(&since);
        base.transact_mut()
            .apply_update(Update::decode_v1(&diff).unwrap())
            .unwrap();
        assert_eq!(page_children(&base), ["b1", "b3"]);

        // The block record survives as an orphan (conservative GUI delete).
        {
            // Scoped read: opening another txn while this one lives deadlocks
            // the single-threaded RwLock (no recursive read upgrade).
            let root = base.get_or_insert_map("data");
            let txn = base.transact();
            let Some(Out::YMap(document)) = root.get(&txn, "document") else {
                panic!()
            };
            let Some(Out::YMap(blocks)) = document.get(&txn, "blocks") else {
                panic!()
            };
            assert!(matches!(blocks.get(&txn, "b2"), Some(Out::YMap(_))));
        }
        assert_eq!(text_of(&base, "t_b2"), "title");
    }

    #[test]
    fn delete_block_rejects_missing_and_finds_nested() {
        let doc = fixture_doc();
        let err = delete_block_from_parent(&doc, "nope").unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "{err:?}");

        // Nest b3 under b2's children array; deleting finds that array.
        {
            let root = doc.get_or_insert_map("data");
            let mut txn = doc.transact_mut();
            let Some(Out::YMap(document)) = root.get(&txn, "document") else {
                panic!()
            };
            let Some(Out::YMap(meta)) = document.get(&txn, "meta") else {
                panic!()
            };
            let Some(Out::YMap(children_map)) = meta.get(&txn, "children_map") else {
                panic!()
            };
            let Some(Out::YArray(c_page)) = children_map.get(&txn, "c_page") else {
                panic!()
            };
            c_page.remove(&mut txn, 2);
            children_map.insert(&mut txn, "c_b2", ArrayPrelim::from([In::from("b3")]));
            txn.commit();
        }
        let outcome = delete_block_from_parent(&doc, "b3").unwrap();
        assert_eq!(outcome.children_id, "c_b2");
        assert_eq!(outcome.index, 0);
        assert_eq!(page_children(&doc), ["b1", "b2"]);
    }

    #[test]
    fn json_to_any_matches_serde_deserialization() {
        // The zero-clone converter must be indistinguishable from what
        // serde_json::from_value::<Any> produced, including the u64 that
        // does not fit an i64 (rejected with the same message).
        for value in [
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(42),
            json!(-42),
            json!(3.5),
            json!(i64::MAX),
            json!(i64::MIN),
            json!(u64::MAX),
            json!("text"),
            json!([1, "two", [3.5], null, {"k": true}]),
            json!({"bold": true, "n": -7, "nested": {"a": [1, 2]}}),
        ] {
            let via_serde = serde_json::from_value::<Any>(value.clone());
            match json_to_any(&value) {
                Ok(converted) => assert_eq!(Some(converted), via_serde.ok()),
                Err(message) => {
                    assert_eq!(via_serde.err().map(|e| e.to_string()), Some(message))
                }
            }
        }
    }

    #[test]
    fn snapshot_round_trips() {
        let doc = fixture_doc();
        let envelope = snapshot_envelope(&doc, "ws", "page");
        assert_eq!(envelope["page_id"], json!("page"));
        assert!(envelope["doc_state_bytes"].as_u64().unwrap() > 0);

        let bytes = serde_json::to_vec_pretty(&envelope).unwrap();
        let snapshot = parse_snapshot(&bytes).unwrap();
        assert_eq!(snapshot.page_id, "page");
        assert_eq!(snapshot.workspace_id, "ws");

        // The decoded state rebuilds an identical document.
        let restored = Doc::new();
        restored
            .transact_mut()
            .apply_update(Update::decode_v1(&snapshot.doc_state).unwrap())
            .unwrap();
        assert_eq!(text_of(&restored, "t_b1"), "hello world");
        assert_eq!(page_children(&restored), ["b1", "b2", "b3"]);

        assert!(parse_snapshot(b"{}").is_err());
        assert!(parse_snapshot(b"not json").is_err());
    }

    /// Document whose root children are paragraphs A "alpha", B "beta",
    /// C "gamma" — the A/B/C fixture for restore semantics.
    fn abc_doc() -> Doc {
        let doc = Doc::new();
        {
            let root = doc.get_or_insert_map("data");
            let mut txn = doc.transact_mut();
            let document = root.insert(&mut txn, "document", empty_map());
            document.insert(&mut txn, "page_id", In::from("page"));
            let blocks = document.insert(&mut txn, "blocks", empty_map());
            let page = blocks.insert(&mut txn, "page", empty_map());
            page.insert(&mut txn, "children", In::from("c_page"));
            let meta = document.insert(&mut txn, "meta", empty_map());
            let children_map = meta.insert(&mut txn, "children_map", empty_map());
            let text_map = meta.insert(&mut txn, "text_map", empty_map());
            for (id, text) in [("A", "alpha"), ("B", "beta"), ("C", "gamma")] {
                let block = blocks.insert(&mut txn, id, empty_map());
                block.insert(&mut txn, "ty", In::from("paragraph"));
                block.insert(&mut txn, "children", In::from(format!("c_{id}").as_str()));
                block.insert(
                    &mut txn,
                    "external_id",
                    In::from(format!("t_{id}").as_str()),
                );
                text_map.insert(&mut txn, format!("t_{id}").as_str(), TextPrelim::new(text));
            }
            children_map.insert(
                &mut txn,
                "c_page",
                ArrayPrelim::from([In::from("A"), In::from("B"), In::from("C")]),
            );
            txn.commit();
        }
        doc
    }

    /// Every child id linked from anywhere in children_map (what is visible).
    fn linked_ids(doc: &Doc) -> Vec<String> {
        let root = doc.get_or_insert_map("data");
        let txn = doc.transact();
        let Some(Out::YMap(document)) = root.get(&txn, "document") else {
            panic!()
        };
        let Some(Out::YMap(meta)) = document.get(&txn, "meta") else {
            panic!()
        };
        let Some(Out::YMap(children_map)) = meta.get(&txn, "children_map") else {
            panic!()
        };
        let mut linked = Vec::new();
        for (_, out) in children_map.iter(&txn) {
            if let Out::YArray(array) = out {
                linked.extend(array.iter(&txn).map(|v| v.to_string(&txn)));
            }
        }
        linked
    }

    /// The block record still sits in blocks (reachable or not).
    fn block_exists(doc: &Doc, block_id: &str) -> bool {
        let root = doc.get_or_insert_map("data");
        let txn = doc.transact();
        let Some(Out::YMap(document)) = root.get(&txn, "document") else {
            panic!()
        };
        let Some(Out::YMap(blocks)) = document.get(&txn, "blocks") else {
            panic!()
        };
        matches!(blocks.get(&txn, block_id), Some(Out::YMap(_)))
    }

    /// Visible root body texts, in order.
    fn root_child_texts(doc: &Doc) -> Vec<String> {
        let root = doc.get_or_insert_map("data");
        let txn = doc.transact();
        let Some(Out::YMap(document)) = root.get(&txn, "document") else {
            panic!()
        };
        let Some(Out::YMap(blocks)) = document.get(&txn, "blocks") else {
            panic!()
        };
        let Some(Out::YMap(meta)) = document.get(&txn, "meta") else {
            panic!()
        };
        let Some(Out::YMap(children_map)) = meta.get(&txn, "children_map") else {
            panic!()
        };
        let Some(Out::YMap(text_map)) = meta.get(&txn, "text_map") else {
            panic!()
        };
        let Some(Out::YArray(ids)) = children_map.get(&txn, "c_page") else {
            panic!()
        };
        ids.iter(&txn)
            .map(|v| v.to_string(&txn))
            .map(|id| {
                let Some(Out::YMap(block)) = blocks.get(&txn, &id) else {
                    panic!("new block {id} missing");
                };
                match block.get(&txn, "external_id") {
                    Some(Out::Any(Any::String(ext))) => match text_map.get(&txn, ext.as_ref()) {
                        Some(Out::YText(text)) => text.get_string(&txn),
                        other => panic!("no text {ext}: {other:?}"),
                    },
                    other => format!("{other:?}"),
                }
            })
            .collect()
    }

    #[test]
    fn rebuild_detaches_everything_and_recreates_snapshot_content() {
        let doc = abc_doc();
        // Snapshot content: the ORIGINAL A/B/C texts, as parsed blocks.
        let snapshot_blocks = vec![
            Block::new("paragraph", json!({"delta": [{"insert": "alpha"}]})),
            Block::new("paragraph", json!({"delta": [{"insert": "beta"}]})),
            Block::new("paragraph", json!({"delta": [{"insert": "gamma"}]})),
        ];

        // Post-snapshot edits: change B, detach C, append a new block D.
        update_block_text(&doc, "B", "CHANGED").unwrap();
        delete_block_from_parent(&doc, "C").unwrap();
        {
            let root = doc.get_or_insert_map("data");
            let mut txn = doc.transact_mut();
            let Some(Out::YMap(document)) = root.get(&txn, "document") else {
                panic!()
            };
            let Some(Out::YMap(blocks)) = document.get(&txn, "blocks") else {
                panic!()
            };
            let d = blocks.insert(&mut txn, "D", empty_map());
            d.insert(&mut txn, "ty", In::from("paragraph"));
            d.insert(&mut txn, "children", In::from("c_D"));
            let Some(Out::YMap(meta)) = document.get(&txn, "meta") else {
                panic!()
            };
            let Some(Out::YMap(children_map)) = meta.get(&txn, "children_map") else {
                panic!()
            };
            let Some(Out::YArray(c_page)) = children_map.get(&txn, "c_page") else {
                panic!()
            };
            c_page.push_back(&mut txn, In::from("D"));
            txn.commit();
        }
        assert_eq!(text_of(&doc, "t_B"), "CHANGED");
        assert_eq!(page_children(&doc), ["A", "B", "D"]);

        let since = doc.transact().state_vector();
        let outcome = rebuild_document_from_blocks(&doc, &snapshot_blocks).unwrap();
        assert_eq!(outcome.removed_existing, 3, "A, B and D were attached");
        assert_eq!(outcome.rebuilt_blocks, 3);
        assert!(outcome.warnings.is_empty());

        // Visible body == the snapshot, fresh ids everywhere.
        let visible = page_children(&doc);
        assert_eq!(visible.len(), 3);
        for id in &visible {
            assert!(!["A", "B", "C", "D"].contains(&id.as_str()));
        }
        assert_eq!(root_child_texts(&doc), ["alpha", "beta", "gamma"]);

        // Tombstones: the old records stay in blocks and in the text map,
        // linked from nowhere.
        for id in ["A", "B", "C", "D"] {
            assert!(block_exists(&doc, id), "block {id} must survive");
            assert!(
                !linked_ids(&doc).contains(&id.to_string()),
                "block {id} must be invisible"
            );
        }
        assert_eq!(text_of(&doc, "t_B"), "CHANGED", "tombstone text preserved");

        // And the whole restore ships as one coherent diff onto a stale copy.
        let stale = abc_doc();
        let diff = doc.transact().encode_state_as_update_v1(&since);
        stale
            .transact_mut()
            .apply_update(Update::decode_v1(&diff).unwrap())
            .unwrap();
        assert_eq!(root_child_texts(&stale), ["alpha", "beta", "gamma"]);
        for id in ["A", "B", "C"] {
            assert!(block_exists(&stale, id));
        }
    }

    #[test]
    fn rebuild_skips_unsupported_types_with_warning() {
        let doc = abc_doc();
        let blocks = vec![
            Block::new("paragraph", json!({"delta": [{"insert": "kept"}]})),
            crate::markdown::Block::with_children(
                "simple_table",
                json!({}),
                vec![crate::markdown::Block::new("simple_table_row", json!({}))],
            ),
        ];
        let outcome = rebuild_document_from_blocks(&doc, &blocks).unwrap();
        assert_eq!(outcome.rebuilt_blocks, 1);
        assert_eq!(outcome.removed_existing, 3);
        assert_eq!(outcome.warnings.len(), 1);
        assert!(
            outcome.warnings[0].contains("simple_table"),
            "{:?}",
            outcome.warnings
        );
        assert_eq!(root_child_texts(&doc), ["kept"]);
    }
}

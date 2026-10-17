//! Decode an AppFlowy document collab back into plain data structures.
//!
//! The page-view endpoint returns `encoded_collab` — a raw yjs v1 update.
//! Decoding it with yrs preserves inline formatting (bold, italic, links)
//! that the server's `collab/{id}/json` endpoint flattens to plain text.

use std::collections::HashMap;

use serde_json::{json, Value};
use yrs::types::text::YChange;
use yrs::types::Attrs;
use yrs::updates::decoder::Decode;
use yrs::{Any, Array, Doc, Map, Number, Out, ReadTxn, Text, Transact, Update};

use crate::delta::DeltaOp;
use crate::error::{Error, Result};

/// A decoded AppFlowy document — mirrors the dict shape produced by the
/// Python `decode_document`.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub page_id: String,
    pub blocks: HashMap<String, BlockEntry>,
    pub children_map: HashMap<String, Vec<String>>,
    pub text_map: HashMap<String, Vec<DeltaOp>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockEntry {
    /// Python `block["ty"]`, defaults to `""` when absent.
    pub ty: String,
    /// `block["data"]` is a JSON string in the collab; unparseable data
    /// falls back to `{}` (same as Python).
    pub data: Value,
    /// Python `block["children"]`, defaults to `""`.
    pub children: String,
    pub external_id: Option<String>,
}

/// Decode an AppFlowy document collab (yjs v1 update bytes) into plain data.
///
/// The Python version also accepted `list[int]`; here the caller passes
/// `&[u8]` directly.
pub fn decode_document(encoded_collab: &[u8]) -> Result<Document> {
    let doc = Doc::new();
    let update = Update::decode_v1(encoded_collab)
        .map_err(|e| Error::Other(format!("invalid yjs update: {e}")))?;
    doc.transact_mut()
        .apply_update(update)
        .map_err(|e| Error::Other(format!("failed to apply yjs update: {e}")))?;
    // NB: resolve root refs BEFORE opening a transaction — get_or_insert_map
    // takes the store write lock when the root is missing and would deadlock
    // against an open transaction.
    let root = doc.get_or_insert_map("data");
    let txn = doc.transact();
    let document = match root.get(&txn, "document") {
        Some(Out::YMap(map)) => map,
        other => {
            return Err(Error::Other(format!(
                "collab is missing the data/document map (found {other:?})"
            )))
        }
    };

    let page_id = map_string(&document, &txn, "page_id")
        .ok_or_else(|| Error::Other("collab document is missing page_id".into()))?;

    let mut blocks = HashMap::new();
    if let Some(Out::YMap(blocks_map)) = document.get(&txn, "blocks") {
        for (block_id, out) in blocks_map.iter(&txn) {
            let Out::YMap(block) = out else {
                continue;
            };
            // `block["data"]` is a JSON string; parse failures fall back to {}.
            let data = match block.get(&txn, "data") {
                Some(Out::Any(Any::String(raw))) if !raw.is_empty() => {
                    serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
                }
                _ => json!({}),
            };
            blocks.insert(
                block_id.to_string(),
                BlockEntry {
                    ty: map_string(&block, &txn, "ty").unwrap_or_default(),
                    data,
                    children: map_string(&block, &txn, "children").unwrap_or_default(),
                    external_id: map_string(&block, &txn, "external_id"),
                },
            );
        }
    }

    let meta = match document.get(&txn, "meta") {
        Some(Out::YMap(map)) => map,
        other => {
            return Err(Error::Other(format!(
                "collab document is missing the meta map (found {other:?})"
            )))
        }
    };

    let mut children_map = HashMap::new();
    if let Some(Out::YMap(cm)) = meta.get(&txn, "children_map") {
        for (key, out) in cm.iter(&txn) {
            let ids = match out {
                Out::YArray(array) => array.iter(&txn).map(|v| v.to_string(&txn)).collect(),
                // Plain JSON arrays (e.g. from a non-yjs source) are accepted too.
                Out::Any(Any::Array(vals)) => vals.iter().map(ToString::to_string).collect(),
                _ => Vec::new(),
            };
            children_map.insert(key.to_string(), ids);
        }
    }

    let mut text_map = HashMap::new();
    if let Some(Out::YMap(tm)) = meta.get(&txn, "text_map") {
        for (text_id, out) in tm.iter(&txn) {
            let ops = match out {
                Out::YText(text) => text
                    .diff(&txn, YChange::identity)
                    .into_iter()
                    .map(|d| {
                        // Insert is a string chunk here; fall back to str(...)
                        // like Python for exotic embedded values.
                        let insert = match d.insert {
                            Out::Any(Any::String(s)) => s.to_string(),
                            other => other.to_string(&txn),
                        };
                        // Python emits "attributes" only for non-empty attr maps.
                        let attributes = d
                            .attributes
                            .filter(|attrs| !attrs.is_empty())
                            .map(|attrs| attrs_to_json(&attrs));
                        DeltaOp { insert, attributes }
                    })
                    .collect(),
                // Plain string (e.g. from the lossy collab/json fallback).
                Out::Any(Any::String(s)) if !s.is_empty() => vec![DeltaOp::plain(s.as_ref())],
                // Null/Undefined/empty string: Python treats these as falsy -> [].
                Out::Any(Any::Null | Any::Undefined | Any::String(_)) => Vec::new(),
                Out::Any(any) => vec![DeltaOp::plain(any.to_string())],
                other => vec![DeltaOp::plain(other.to_string(&txn))],
            };
            text_map.insert(text_id.to_string(), ops);
        }
    }

    Ok(Document {
        page_id,
        blocks,
        children_map,
        text_map,
    })
}

fn map_string(map: &yrs::MapRef, txn: &impl ReadTxn, key: &str) -> Option<String> {
    match map.get(txn, key) {
        Some(Out::Any(Any::String(s))) => Some(s.to_string()),
        _ => None,
    }
}

fn attrs_to_json(attrs: &Attrs) -> serde_json::Map<String, Value> {
    attrs
        .iter()
        .map(|(k, v)| (k.to_string(), any_to_json(v)))
        .collect()
}

/// Convert a yrs `Any` to `serde_json::Value`. `Undefined` has no JSON
/// equivalent and is mapped to null (matching yrs' own ToJson behavior).
fn any_to_json(any: &Any) -> Value {
    match any {
        Any::Null | Any::Undefined => Value::Null,
        Any::Bool(b) => (*b).into(),
        Any::Number(Number::Int(i)) => (*i).into(),
        Any::Number(Number::Float(f)) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Any::String(s) => s.as_ref().into(),
        Any::Buffer(bytes) => Value::Array(bytes.iter().map(|b| Value::from(*b)).collect()),
        Any::Array(items) => items.iter().map(any_to_json).collect(),
        Any::Map(map) => map
            .iter()
            .map(|(k, v)| (k.clone(), any_to_json(v)))
            .collect(),
    }
}

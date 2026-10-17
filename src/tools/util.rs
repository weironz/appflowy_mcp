//! Helpers shared by the tool modules.

use std::path::{Path, PathBuf};

use reqwest::Method;
use rmcp::ErrorData;
use serde_json::{Value, json};

use crate::client::AppFlowyClient;
use crate::error::{Error, Result};
use crate::markdown::Block;

/// Bridge crate errors onto MCP errors. Validation failures are caller bugs
/// (invalid params); everything else keeps the full Display — which already
/// carries the AppFlowy `code` and HTTP status — as an internal error.
pub fn map_tool_error(e: Error) -> ErrorData {
    match e {
        Error::Validation(message) => ErrorData::invalid_params(message, None),
        other => ErrorData::internal_error(other.to_string(), None),
    }
}

/// Guard against passing the workspace_id as a page parent.
///
/// AppFlowy's hierarchy is workspace -> space -> page. The workspace root
/// view_id equals the workspace_id, and any view created directly under it
/// becomes a Space, not a document. This is a silent footgun, so reject it
/// with an actionable message instead of creating an unexpected space.
pub fn ensure_parent_is_not_workspace(workspace_id: &str, parent_view_id: &str) -> Result<()> {
    if parent_view_id == workspace_id {
        return Err(Error::Validation(
            "parent_view_id must be the view_id of a Space or an existing Page, \
             not the workspace_id. AppFlowy's hierarchy is workspace -> space -> \
             page; a view created directly under the workspace root becomes a \
             Space rather than a document. Call appflowy_list_spaces to pick a \
             space (or appflowy_create_space to make one) and pass its view_id."
                .into(),
        ));
    }
    Ok(())
}

/// Pre-order traversal of a view tree, matching Python's `walk_views`
/// generator.
pub fn walk_views(view: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    let mut stack = vec![view];
    while let Some(current) = stack.pop() {
        out.push(current);
        if let Some(children) = current.get("children").and_then(Value::as_array) {
            for child in children.iter().rev() {
                stack.push(child);
            }
        }
    }
    out
}

/// POST /api/workspace/{workspace_id}/page-view. The client's response
/// handling already unwraps the `{code, message, data}` envelope, so the
/// returned value is the unwrapped `data`.
#[allow(clippy::too_many_arguments)]
pub async fn create_page_with_blocks(
    client: &AppFlowyClient,
    workspace_id: &str,
    parent_view_id: &str,
    title: &str,
    blocks: Vec<Block>,
    layout: i64,
    view_id: Option<String>,
    collab_id: Option<String>,
) -> Result<Value> {
    // AppFlowy stores the document collab under object_id == collab_id, but loads
    // a view's document by view_id. If only view_id is given, collab_id would
    // default to a different server-generated uuid, making the page unreadable
    // ("Collab not found"). Keep them in sync.
    let collab_id = collab_id.or_else(|| view_id.clone());
    ensure_parent_is_not_workspace(workspace_id, parent_view_id)?;
    client
        .request(
            Method::POST,
            &format!("/api/workspace/{workspace_id}/page-view"),
            None,
            // The client strips top-level null values from the payload.
            Some(json!({
                "parent_view_id": parent_view_id,
                "layout": layout,
                "name": title,
                "page_data": {
                    "type": "page",
                    "children": blocks,
                },
                "view_id": view_id,
                "collab_id": collab_id,
            })),
        )
        .await
}

/// Faithful port of Python `unique_path`: return `path` if free, otherwise
/// append ` (n)` before the suffix, starting at 2.
pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
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
        if !candidate.exists() {
            return candidate;
        }
        counter += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_views_preorder() {
        let tree = json!({
            "view_id": "a",
            "children": [
                {"view_id": "b", "children": [{"view_id": "c"}]},
                {"view_id": "d"},
            ],
        });
        let ids: Vec<&str> = walk_views(&tree)
            .iter()
            .filter_map(|v| v.get("view_id").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, ["a", "b", "c", "d"]);
    }

    #[test]
    fn rejects_workspace_as_parent() {
        assert!(ensure_parent_is_not_workspace("ws", "ws").is_err());
        assert!(ensure_parent_is_not_workspace("ws", "space").is_ok());
    }

    #[test]
    fn errors_map_to_mcp_codes() {
        let err = map_tool_error(Error::Validation("bad".into()));
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        let err = map_tool_error(Error::api("boom", Some(500), None));
        assert_eq!(err.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(err.message.contains("boom"));
    }
}

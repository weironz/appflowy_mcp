//! Helpers shared by the tool modules.

use std::path::{Path, PathBuf};

use reqwest::Method;
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock};
use rmcp::ErrorData;
use serde_json::{json, Value};

use crate::client::AppFlowyClient;
use crate::error::{Error, Result};
use crate::markdown::Block;

/// Error type for tool functions, bridging the two MCP failure modes
/// (SEP-1303): **input validation** failures are tool *execution* errors
/// (an `isError: true` result the model can read and self-correct), while
/// everything else stays a JSON-RPC protocol error (`ErrorData`). The rmcp
/// blanket `IntoCallToolResult for Result<T, E>` routes each variant.
pub enum ToolError {
    /// Caller-fixable bad input -> `Ok(CallToolResult::error(...))`.
    Validation(String),
    /// API / transport / infrastructure failure -> `Err(ErrorData)`.
    Protocol(ErrorData),
}

impl ToolError {
    /// Bad caller input; surfaces as an `isError: true` tool result.
    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }

    /// Internal failure; surfaces as JSON-RPC `-32603`. Kept for the rare
    /// spots that construct an internal error directly instead of going
    /// through a crate [`Error`].
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Protocol(ErrorData::internal_error(message.into(), None))
    }

    /// Wrap an already-built [`ErrorData`] as a protocol error.
    pub fn protocol(data: ErrorData) -> Self {
        Self::Protocol(data)
    }
}

impl IntoCallToolResult for ToolError {
    fn into_call_tool_result(self) -> std::result::Result<CallToolResponse, ErrorData> {
        match self {
            // CallToolResult::error already sets is_error: true; the blanket
            // Result impl would set it again anyway.
            Self::Validation(message) => {
                Ok(CallToolResult::error(vec![ContentBlock::text(message)]).into())
            }
            Self::Protocol(data) => Err(data),
        }
    }
}

/// Bridge crate errors onto [`ToolError`]. Validation failures are tool
/// execution errors the caller can fix (SEP-1303); everything else keeps the
/// full Display — which already carries the AppFlowy `code` and HTTP status —
/// as a protocol-level internal error.
pub fn map_tool_error(e: Error) -> ToolError {
    match e {
        Error::Validation(message) => ToolError::Validation(message),
        other => ToolError::Protocol(ErrorData::internal_error(other.to_string(), None)),
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

/// Minimal port of Python `Path.expanduser()`: expands a leading `~` (bare,
/// `~/...`, or `~\...`) against `$USERPROFILE` (Windows) or `$HOME`. The
/// `~user/...` form is left untouched, as are paths without a leading tilde.
pub fn expand_tilde(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix('~') else {
        return PathBuf::from(path);
    };
    if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\')) {
        return PathBuf::from(path);
    }
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let Some(home) = std::env::var_os(home_var) else {
        return PathBuf::from(path);
    };
    if rest.is_empty() {
        PathBuf::from(home)
    } else {
        PathBuf::from(home).join(rest.trim_start_matches(['/', '\\']))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

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
    fn validation_becomes_tool_execution_error() {
        // SEP-1303: input validation surfaces as an isError tool result, not
        // a JSON-RPC protocol error, so the model can read it and retry.
        let err = map_tool_error(Error::Validation("bad".into()));
        let CallToolResponse::Complete(result) = err.into_call_tool_result().expect("tool result")
        else {
            panic!("expected a complete call-tool result");
        };
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.content.len(), 1);
        assert!(matches!(
            &result.content[0],
            rmcp::model::ContentBlock::Text(t) if t.text == "bad"
        ));
    }

    #[test]
    fn non_validation_stays_protocol_error() {
        // API/transport/etc. keep the full message (AppFlowy code + HTTP
        // status) as a JSON-RPC -32603 protocol error.
        let err = map_tool_error(Error::api("boom", Some(500), None));
        let data = err.into_call_tool_result().expect_err("protocol error");
        assert_eq!(data.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(data.message.contains("boom"));
        assert!(data.message.contains("500"));
    }

    #[test]
    fn direct_validation_constructor_matches() {
        let data = ToolError::internal("kaput");
        assert!(data.into_call_tool_result().is_err());
        let err = ToolError::validation("nope");
        let CallToolResponse::Complete(result) = err.into_call_tool_result().expect("tool result")
        else {
            panic!("expected a complete call-tool result");
        };
        assert_eq!(result.is_error, Some(true));
    }

    // These tests mutate process-wide env vars; serialize them with a lock
    // and always restore the original value.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    struct EnvGuard(&'static str, Option<std::ffi::OsString>);

    impl EnvGuard {
        fn set(var: &'static str, value: Option<&str>) -> Self {
            let guard = Self(var, std::env::var_os(var));
            match value {
                Some(value) => std::env::set_var(var, value),
                None => std::env::remove_var(var),
            }
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.1 {
                Some(value) => std::env::set_var(self.0, value),
                None => std::env::remove_var(self.0),
            }
        }
    }

    fn home_var() -> &'static str {
        if cfg!(windows) {
            "USERPROFILE"
        } else {
            "HOME"
        }
    }

    #[test]
    fn expand_tilde_passes_through_non_tilde_paths() {
        assert_eq!(expand_tilde("docs/a.md"), PathBuf::from("docs/a.md"));
        assert_eq!(expand_tilde("~user/a.md"), PathBuf::from("~user/a.md"));
    }

    #[test]
    fn expand_tilde_expands_against_home() {
        let _lock = env_lock().lock().unwrap();
        let _guard = EnvGuard::set(home_var(), Some("/fake/home"));
        assert_eq!(expand_tilde("~"), PathBuf::from("/fake/home"));
        assert_eq!(expand_tilde("~/a.md"), PathBuf::from("/fake/home/a.md"));
        assert_eq!(expand_tilde("~\\a.md"), PathBuf::from("/fake/home/a.md"));
    }

    #[test]
    fn expand_tilde_without_home_var_passes_through() {
        let _lock = env_lock().lock().unwrap();
        let _guard = EnvGuard::set(home_var(), None);
        assert_eq!(expand_tilde("~/a.md"), PathBuf::from("~/a.md"));
    }
}

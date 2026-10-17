//! Search tools (`appflowy_search`, `appflowy_search_summary`).

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, Json, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::AppServer;
use crate::tools::util::map_tool_error;

/// Python-style `or` truthiness over JSON: null, false, 0, and empty
/// strings/arrays/objects fall through to the next candidate.
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
    }
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct SearchRequest {
    pub workspace_id: String,
    pub query: String,
    /// Max number of matching documents to return (default 10).
    pub limit: Option<i64>,
    /// Preview characters per match (default 180).
    pub preview_size: Option<i64>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct SearchSummaryRequest {
    pub workspace_id: String,
    pub query: String,
    /// Max number of matched documents fed into the summary (default 5).
    pub limit: Option<i64>,
    /// Return the raw context instead of an AI-generated answer (default false).
    pub only_context: Option<bool>,
}

#[tool_router(router = search_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_search",
        description = "Search documents in a workspace by keyword or natural-language query (full-text plus semantic search when the server has indexing enabled). Returns matching documents with object_id (the page view_id), preview, and score."
    )]
    async fn appflowy_search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let params: Vec<(&str, String)> = vec![
            ("query", request.query.clone()),
            ("limit", request.limit.unwrap_or(10).to_string()),
            ("preview_size", request.preview_size.unwrap_or(180).to_string()),
        ];
        let data = self
            .client
            .request(
                Method::GET,
                &format!("/api/search/{}", request.workspace_id),
                Some(&params),
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_search_summary",
        description = "Search the workspace and return an AI-generated, cited summary that answers the query over the matched documents. Runs a document search first, then summarizes the results (each summary lists its source object_ids)."
    )]
    async fn appflowy_search_summary(
        &self,
        Parameters(request): Parameters<SearchSummaryRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let search_path = format!("/api/search/{}", request.workspace_id);
        let params: Vec<(&str, String)> = vec![
            ("query", request.query.clone()),
            ("limit", request.limit.unwrap_or(5).to_string()),
        ];
        let search = self
            .client
            .request(Method::GET, &search_path, Some(&params), None)
            .await
            .map_err(map_tool_error)?;

        // Python: `items.get("items") or items.get("data") or []`.
        let items = match &search {
            Value::Object(map) => [map.get("items"), map.get("data")]
                .into_iter()
                .flatten()
                .find(|value| !is_falsy(value))
                .cloned()
                .unwrap_or_else(|| json!([])),
            other => other.clone(),
        };

        // Each kept match needs a truthy object_id; content falls back to
        // preview, then "" (Python `it.get("content") or it.get("preview") or ""`).
        let results: Vec<Value> = match &items {
            Value::Array(list) => list
                .iter()
                .filter_map(|item| {
                    let item = item.as_object()?;
                    let object_id = item.get("object_id")?;
                    if is_falsy(object_id) {
                        return None;
                    }
                    let content = [item.get("content"), item.get("preview")]
                        .into_iter()
                        .flatten()
                        .find(|value| !is_falsy(value))
                        .cloned()
                        .unwrap_or_else(|| json!(""));
                    Some(json!({"object_id": object_id, "content": content}))
                })
                .collect(),
            _ => Vec::new(),
        };

        if results.is_empty() {
            return Ok(Json(json!({
                "summaries": [],
                "note": "No matching documents to summarize.",
            })));
        }

        let body = self
            .client
            .request(
                Method::GET,
                &format!("{search_path}/summary"),
                None,
                Some(json!({
                    "query": request.query,
                    "search_results": results,
                    "only_context": request.only_context.unwrap_or(false),
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(body))
    }
}

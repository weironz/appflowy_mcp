//! Database, row, and field tools.

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use yrs::{ReadTxn, Transact};

use crate::collab_edit;
use crate::error::Error;
use crate::markdown::parse_markdown_to_blocks;
use crate::server::AppServer;
use crate::tools::util::{map_tool_error, ToolError};

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ListDatabasesRequest {
    pub workspace_id: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct GetDatabaseFieldsRequest {
    pub workspace_id: String,
    pub database_id: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ListRowsRequest {
    pub workspace_id: String,
    pub database_id: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct GetRowDetailsRequest {
    pub workspace_id: String,
    pub database_id: String,
    /// Comma-separated row UUIDs.
    pub row_ids: String,
    pub with_doc: bool,
}

/// Mirrors Python `models.py::RowCreateRequest` plus the flat path ids.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct CreateRowRequest {
    pub workspace_id: String,
    pub database_id: String,
    /// Cell values keyed by field. Sent as `{}` when omitted or null.
    pub cells: Value,
    pub document: Option<String>,
}

/// Mirrors Python `models.py::RowUpdateRequest` plus the flat path ids.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct UpsertRowRequest {
    pub workspace_id: String,
    pub database_id: String,
    /// Content hash of the row to update; empty string creates a new row.
    pub pre_hash: Option<String>,
    /// Cell values keyed by field. Sent as `{}` when omitted or null.
    pub cells: Value,
    pub document: Option<String>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct GetUpdatedRowsRequest {
    pub workspace_id: String,
    pub database_id: String,
    /// ISO 8601 datetime; only rows updated after it are returned.
    pub after: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ListDatabaseHistoryRequest {
    pub workspace_id: String,
    pub database_id: String,
    /// Page size; the API defaults to 30 when omitted.
    pub limit: Option<i64>,
    /// Unix seconds; only history entries at/after it are returned.
    pub since: Option<i64>,
    /// Follow the response's next-cursor fields and merge every page
    /// (default false: return the first page as-is).
    pub fetch_all: Option<bool>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct GetDatabaseRestoreStateRequest {
    pub workspace_id: String,
    pub database_id: String,
}

/// AppFlowy's deterministic document UUID for a database row.
///
/// A row is a DatabaseRow collab; its page body is a *separate* Document
/// collab whose UUID is UUIDv5(namespace = row UUID, name = "document_id").
/// Passing the row UUID to a normal page-content endpoint therefore targets
/// the wrong collab and produces `Record deleted: Document/{row_id} is not
/// active` — that mismatch was community issue #1.
pub fn database_row_document_id(row_id: &str) -> Result<String, Error> {
    let row_uuid = uuid::Uuid::parse_str(row_id)
        .map_err(|_| Error::Validation(format!("row_id must be a valid UUID, got {row_id:?}")))?;
    Ok(uuid::Uuid::new_v5(&row_uuid, b"document_id").to_string())
}

/// The `append-block` endpoint inserts each block's children at the front and
/// reorders batched top-level blocks, so children are pre-reversed and blocks
/// go out one top-level block per call (see `append_blocks`).
fn server_order(mut block: Value) -> Value {
    if let Some(children) = block.get_mut("children").and_then(Value::as_array_mut) {
        let reversed: Vec<Value> = children.drain(..).rev().map(server_order).collect();
        *children = reversed;
    }
    block
}

/// Append blocks to the end of a document, in Markdown order: one top-level
/// block per call with children pre-reversed. Returns the last response —
/// the Python `append_blocks` helper (issue: batched calls landed
/// out of order and tables came out upside-down).
async fn append_blocks(
    client: &crate::client::AppFlowyClient,
    workspace_id: &str,
    document_id: &str,
    blocks: Vec<Value>,
) -> Result<Value, Error> {
    let mut last = Value::Null;
    for block in blocks {
        last = client
            .request(
                Method::POST,
                &format!("/api/workspace/{workspace_id}/page-view/{document_id}/append-block"),
                None,
                Some(json!({ "blocks": [server_order(block)] })),
            )
            .await?;
    }
    Ok(last)
}

/// Mirrors Python `models.py::AppendMarkdownRequest` for the row variant.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct AppendMarkdownToRowRequest {
    pub workspace_id: String,
    pub database_id: String,
    pub row_id: String,
    /// Markdown to append to the row's page body.
    pub content: String,
}

/// `cells` maps field name (or field id) to the new value; see the
/// update_row tool description for the per-type value forms.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct UpdateRowRequest {
    pub workspace_id: String,
    pub database_id: String,
    pub row_id: String,
    /// Cell values keyed by field name (or field id). Sent as `{}` when omitted.
    pub cells: Value,
}

/// Shared shape of the delete/restore row tools.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct RowRefRequest {
    pub workspace_id: String,
    pub database_id: String,
    pub row_id: String,
}

/// Mirrors Python `models.py::CreateDatabaseFieldRequest` plus the flat path ids.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct CreateDatabaseFieldRequest {
    pub workspace_id: String,
    pub database_id: String,
    pub name: String,
    /// AppFlowy field type id: 0=RichText, 1=Number, 2=DateTime, 3=SingleSelect, 4=MultiSelect, 5=Checkbox, 6=URL, 7=Checklist.
    pub field_type: i64,
    pub type_option_data: Option<Value>,
}

/// Python took `body.get("data", [])` as the row/field list; the client's
/// envelope unwrap yields the `data` payload directly, so only the no-data
/// (`Null`) case needs the empty-array fallback.
fn data_or_empty_list(data: Value) -> Value {
    match data {
        Value::Null => json!([]),
        other => other,
    }
}

/// Python's `cells or {}`: an omitted or null `cells` object is sent as `{}`.
fn cells_or_none(cells: Value) -> Option<Value> {
    match cells {
        Value::Null => None,
        other => Some(other),
    }
}

/// Upper bound on history pages fetched by `fetch_all`, guarding against a
/// server which keeps handing out cursors.
const MAX_HISTORY_PAGES: usize = 100;

/// Fetch every remaining page of a database history listing (the
/// `fetch_all` path of `appflowy_list_database_history`) and merge the
/// items arrays into the last page's envelope.
///
/// Incomplete results are never reported as success: a follow-up page with
/// an unrecognized shape, or a still-live cursor at the [`MAX_HISTORY_PAGES`]
/// cap, is a hard error rather than a silently truncated merge.
async fn fetch_all_history_pages(
    client: &crate::client::AppFlowyClient,
    base_path: &str,
    query: &[(&str, String)],
) -> crate::error::Result<Value> {
    let first_page = client
        .request(Method::GET, base_path, Some(query), None)
        .await?;
    // Merging needs a recognized items array; unknown shapes fall back
    // to the first page verbatim.
    let mut merged = match history_page_items(&first_page) {
        Some(items) => items,
        None => return Ok(first_page),
    };
    let mut last_page = first_page;
    let mut pages_fetched = 1usize;
    while let Some(cursor) = history_next_cursor(&last_page) {
        // A remaining cursor at the page cap means the merged result would
        // be partial; reporting it as success would silently lose history,
        // the same failure mode the unrecognized-shape error below guards
        // against. Fail loudly for both response shapes.
        if pages_fetched >= MAX_HISTORY_PAGES {
            return Err(Error::Other(format!(
                "History pagination hit the limit of {MAX_HISTORY_PAGES} \
                 pages and a next cursor ({cursor:?}) still remains; the \
                 merged result would silently report partial history as \
                 complete. Re-run without fetch_all and continue manually \
                 from cursor {cursor:?}."
            )));
        }
        let mut page_query = query.to_vec();
        page_query.push(("cursor", cursor.clone()));
        let next_page = client
            .request(Method::GET, base_path, Some(&page_query), None)
            .await?;
        // The fetch succeeded, so an unrecognized shape is a real
        // contract change — silently dropping the page (or stopping with
        // a stale cursor that looks like "done") would report a partial
        // merge as success. Fail loudly, naming the offending page.
        let Some(items) = history_page_items(&next_page) else {
            return Err(Error::Other(format!(
                "History page {} (cursor {:?}) had no recognizable items \
                 array (looked for a top-level array, or data/items/history); \
                 returning unmerged results would silently lose this page.",
                pages_fetched + 1,
                cursor
            )));
        };
        merged.extend(items);
        last_page = next_page;
        pages_fetched += 1;
    }
    // Keep the last page's envelope (its cursor state is the current one)
    // and swap its items array for the merged list.
    let mut result = last_page;
    match &mut result {
        Value::Array(_) => result = Value::Array(merged),
        Value::Object(map) => {
            if let Some(key) = ["data", "items", "history"]
                .into_iter()
                .find(|key| map.get(*key).is_some_and(Value::is_array))
            {
                map.insert(key.to_string(), Value::Array(merged));
            }
            map.insert("pages_fetched".into(), json!(pages_fetched));
        }
        _ => {}
    }
    Ok(result)
}

/// Candidates for the next-page cursor of a history response: top level or a
/// nested `pagination` object (the web client expands it as the `cursor`
/// query param). A null or empty value means there is no next page.
fn history_next_cursor(page: &Value) -> Option<String> {
    let cursor_at = |value: &Value| -> Option<String> {
        ["next_cursor", "next", "cursor"].iter().find_map(|key| {
            value
                .get(*key)
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_string)
        })
    };
    cursor_at(page)
        .or_else(|| page.get("pagination").and_then(cursor_at))
        .or_else(|| page.get("meta").and_then(cursor_at))
}

/// The item list of a history page: the page itself when it is an array,
/// otherwise the first array under one of the usual list keys.
fn history_page_items(page: &Value) -> Option<Vec<Value>> {
    match page {
        Value::Array(items) => Some(items.clone()),
        Value::Object(_) => ["data", "items", "history"]
            .iter()
            .find_map(|key| page.get(*key).and_then(Value::as_array).cloned()),
        _ => None,
    }
}

/// Element of the `appflowy_list_rows` output (AppFlowy-Cloud
/// `AFDatabaseRow`): one row id. Unlisted keys are allowed.
#[derive(Debug, rmcp::schemars::JsonSchema)]
pub struct ListRowsItemOutput {
    /// Row id.
    pub id: String,
}

/// Element of the `appflowy_get_row_details` output (AppFlowy-Cloud
/// `AFDatabaseRowDetail`): the row's cells keyed by database field id.
/// Only `id` is guaranteed; `doc` is present only when requested with
/// `with_doc`; unlisted keys are allowed.
#[derive(Debug, rmcp::schemars::JsonSchema)]
pub struct RowDetailsItemOutput {
    /// Row id.
    pub id: String,
    /// Cell values keyed by database field id.
    pub cells: Option<std::collections::HashMap<String, Value>>,
    /// Whether the row has an own document (page body).
    pub has_doc: Option<bool>,
    /// The row's document payload, when `with_doc` was set.
    pub doc: Option<String>,
}

#[tool_router(router = database_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_list_databases",
        description = "List all databases in a workspace.",
        annotations(
            title = "List databases",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_list_databases(
        &self,
        Parameters(request): Parameters<ListDatabasesRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let data = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/database", request.workspace_id),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_get_database_fields",
        description = "Get fields of a specific database.",
        annotations(
            title = "Get database fields",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_get_database_fields(
        &self,
        Parameters(request): Parameters<GetDatabaseFieldsRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let fields = self
            .client
            .get_database_fields(&request.workspace_id, &request.database_id)
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data_or_empty_list(fields)))
    }

    #[tool(
        name = "appflowy_list_rows",
        description = "List all row IDs in a database.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<Vec<ListRowsItemOutput>>(),
        annotations(
            title = "List rows",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_list_rows(
        &self,
        Parameters(request): Parameters<ListRowsRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let rows = self
            .client
            .get_database_row_ids(&request.workspace_id, &request.database_id)
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data_or_empty_list(rows)))
    }

    #[tool(
        name = "appflowy_get_row_details",
        description = "Get details of specific rows by IDs.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<Vec<RowDetailsItemOutput>>(),
        annotations(
            title = "Get row details",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_get_row_details(
        &self,
        Parameters(request): Parameters<GetRowDetailsRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let ids: Vec<String> = request
            .row_ids
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect();
        if ids.is_empty() {
            return Err(ToolError::validation("At least one row ID is required."));
        }
        let details = self
            .client
            .get_database_row_details(
                &request.workspace_id,
                &request.database_id,
                &ids,
                Some(request.with_doc),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(details))
    }

    #[tool(
        name = "appflowy_create_row",
        description = "Create a database row. IMPORTANT: when the row needs a page body, pass the Markdown in `document` in this same call — it atomically initializes the row's separate document collab. Do NOT create an empty row and then call a normal page append tool (e.g. appflowy_append_markdown_to_page) with the row ID: a row is a DatabaseRow collab and its page body is a different Document collab, so page append with the row ID fails with 'is not active'. For later additions use appflowy_append_markdown_to_row. Checklist cells through this tool need an id on every option; appflowy_update_row accepts plain task names.",
        annotations(title = "Create row", open_world_hint = true)
    )]
    async fn appflowy_create_row(
        &self,
        Parameters(request): Parameters<CreateRowRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let initialized = request
            .document
            .as_deref()
            .is_some_and(|document| !document.is_empty());
        let row_id = self
            .client
            .create_database_row(
                &request.workspace_id,
                &request.database_id,
                cells_or_none(request.cells),
                request.document,
            )
            .await
            .map_err(map_tool_error)?;
        // The API returns the id as a bare JSON string; Value::to_string on a
        // String would re-encode it with quotes (double-encoded id).
        let row_id = match &row_id {
            Value::String(id) => id.clone(),
            other => other.to_string(),
        };
        let mut result = json!({
            "id": row_id,
            "document_initialized": initialized,
        });
        if initialized {
            let document_id = database_row_document_id(&row_id).map_err(map_tool_error)?;
            result
                .as_object_mut()
                .expect("result is an object")
                .insert("document_id".into(), json!(document_id));
        }
        Ok(Json(result))
    }

    #[tool(
        name = "appflowy_upsert_row",
        description = "Create-or-update a row keyed by `pre_hash` (reuse the same non-empty value for the same logical row; `document` Markdown replaces the row's page body). WARNING: the server derives the row id from pre_hash (not a valid v4 UUID) and never adds the row to any view, so it does not show up in boards/grids. To create a row use appflowy_create_row; to edit cells use appflowy_update_row.",
        annotations(title = "Upsert row", open_world_hint = true)
    )]
    async fn appflowy_upsert_row(
        &self,
        Parameters(request): Parameters<UpsertRowRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let initialized = request
            .document
            .as_deref()
            .is_some_and(|document| !document.is_empty());
        let row_id = self
            .client
            .upsert_database_row(
                &request.workspace_id,
                &request.database_id,
                &request.pre_hash.unwrap_or_default(),
                cells_or_none(request.cells),
                request.document,
            )
            .await
            .map_err(map_tool_error)?;
        // The API returns the id as a bare JSON string; Value::to_string on a
        // String would re-encode it with quotes (double-encoded id).
        let row_id = match &row_id {
            Value::String(id) => id.clone(),
            other => other.to_string(),
        };
        let mut result = json!({
            "id": row_id,
            "document_initialized": initialized,
        });
        if initialized {
            // upsert-derived ids are not v4 UUIDs, but the v5 derivation
            // works on any valid UUID text; a non-UUID id makes the error
            // explicit instead of silently dropping document_id.
            match database_row_document_id(&row_id) {
                Ok(document_id) => {
                    result
                        .as_object_mut()
                        .expect("result is an object")
                        .insert("document_id".into(), json!(document_id));
                }
                Err(_) => {
                    result
                        .as_object_mut()
                        .expect("result is an object")
                        .insert("document_id".into(), Value::Null);
                }
            }
        }
        Ok(Json(result))
    }

    #[tool(
        name = "appflowy_get_updated_rows",
        description = "Find updated rows in a database after a specific datetime.",
        annotations(
            title = "Get updated rows",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_get_updated_rows(
        &self,
        Parameters(request): Parameters<GetUpdatedRowsRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let updated_rows = self
            .client
            .get_database_row_ids_updated(
                &request.workspace_id,
                &request.database_id,
                Some(&request.after),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data_or_empty_list(updated_rows)))
    }

    #[tool(
        name = "appflowy_append_markdown_to_row",
        description = "Append Markdown to an existing database row's page body. This resolves the row's separate deterministic Document collab (UUIDv5 derived from the row id) instead of wrongly using the row id as a page id — appending with appflowy_append_markdown_to_page and a row id targets the wrong collab and fails with 'is not active'. The row document must already exist: pass `document` when creating the row with appflowy_create_row / appflowy_upsert_row.",
        annotations(title = "Append markdown to row", open_world_hint = true)
    )]
    async fn appflowy_append_markdown_to_row(
        &self,
        Parameters(request): Parameters<AppendMarkdownToRowRequest>,
    ) -> Result<Json<Value>, ToolError> {
        if request.content.trim().is_empty() {
            return Err(ToolError::validation("content must not be empty"));
        }
        let details = self
            .client
            .get_database_row_details(
                &request.workspace_id,
                &request.database_id,
                std::slice::from_ref(&request.row_id),
                Some(false),
            )
            .await
            .map_err(map_tool_error)?;
        let detail = match &details {
            Value::Array(items) if !items.is_empty() => &items[0],
            _ => {
                return Err(ToolError::validation(format!(
                    "Database row {} was not found in database {}.",
                    request.row_id, request.database_id
                )));
            }
        };
        if !detail
            .get("has_doc")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(map_tool_error(Error::Other(
                "This row was created without a document, so AppFlowy has not \
                 initialized its row-document collab. Create the row with the \
                 initial Markdown in appflowy_create_row's `document`, or use \
                 appflowy_upsert_row with a stable pre_hash and a document."
                    .into(),
            )));
        }
        let document_id = database_row_document_id(&request.row_id).map_err(map_tool_error)?;
        let blocks: Vec<Value> =
            serde_json::to_value(parse_markdown_to_blocks(&request.content, None))
                .map_err(Error::Json)
                .map_err(map_tool_error)?
                .as_array()
                .cloned()
                .unwrap_or_default();
        let block_count = blocks.len();
        let calls = blocks.len();
        let result = append_blocks(&self.client, &request.workspace_id, &document_id, blocks)
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({
            "result": result,
            "row_id": request.row_id,
            "document_id": document_id,
            "block_count": block_count,
            "calls": calls,
        })))
    }

    #[tool(
        name = "appflowy_update_row",
        description = "Edit cells of an EXISTING row (the REST API cannot edit rows; this writes through the collab web-update the way the web client does). `cells` maps a field name (or field id) to a value: text/number/url -> string; single select -> option name; multi select -> list of option names; checkbox -> bool; date -> ISO date/datetime, unix seconds, or {start, end?, include_time?}; checklist -> list of task names or {name, done} items (replaces the list), or {check, uncheck, add, remove: [task names]} (edits the current list). Nothing is written if any value is invalid.",
        annotations(title = "Update row", open_world_hint = true)
    )]
    async fn appflowy_update_row(
        &self,
        Parameters(request): Parameters<UpdateRowRequest>,
    ) -> Result<Json<Value>, ToolError> {
        let Value::Object(cells) = request.cells else {
            return Err(ToolError::validation(
                "cells must be an object mapping a field name (or id) to a value.",
            ));
        };
        if cells.is_empty() {
            return Err(ToolError::validation("cells is empty."));
        }
        let details = self
            .client
            .get_database_row_details(
                &request.workspace_id,
                &request.database_id,
                std::slice::from_ref(&request.row_id),
                Some(false),
            )
            .await
            .map_err(map_tool_error)?;
        if !matches!(&details, Value::Array(items) if !items.is_empty()) {
            return Err(ToolError::validation(format!(
                "Database row {} was not found in database {}.",
                request.row_id, request.database_id
            )));
        }
        let fields_value = self
            .client
            .get_database_fields(&request.workspace_id, &request.database_id)
            .await
            .map_err(map_tool_error)?;
        let fields = collab_edit::parse_fields(&fields_value);
        if fields.is_empty() {
            return Err(map_tool_error(Error::Other(format!(
                "Database {} has no fields.",
                request.database_id
            ))));
        }
        let doc = collab_edit::fetch_collab_doc(
            &self.client,
            &request.workspace_id,
            &request.row_id,
            collab_edit::COLLAB_TYPE_DATABASE_ROW,
        )
        .await
        .map_err(map_tool_error)?;

        // Encode every value before touching the doc: a bad value anywhere
        // rejects the request with nothing written.
        let mut staged = Vec::with_capacity(cells.len());
        for (key, value) in &cells {
            let field = collab_edit::field_for(&fields, key).map_err(map_tool_error)?;
            let current = collab_edit::current_cell_data(&doc, &field.id);
            let entries = collab_edit::encode_cell(field, value, current.as_deref())
                .map_err(ToolError::validation)?;
            staged.push(collab_edit::StagedCell {
                field: field.clone(),
                entries,
            });
        }

        let since = doc.transact().state_vector();
        let updated =
            collab_edit::mutate_row_cells(&doc, &request.database_id, &staged, &request.row_id)
                .map_err(map_tool_error)?;
        let update_bytes = collab_edit::push_web_update(
            &self.client,
            &request.workspace_id,
            &request.row_id,
            collab_edit::COLLAB_TYPE_DATABASE_ROW,
            &doc,
            &since,
        )
        .await
        .map_err(map_tool_error)?;
        Ok(Json(json!({
            "row_id": request.row_id,
            "updated": updated,
            "update_bytes": update_bytes,
        })))
    }

    /// Shared body of `appflowy_delete_row` / `appflowy_restore_row`: flag the
    /// row in every view of the database collab, the web UI's soft delete.
    async fn set_row_deleted(
        &self,
        params: RowRefRequest,
        deleted: bool,
    ) -> Result<Json<Value>, ToolError> {
        let details = self
            .client
            .get_database_row_details(
                &params.workspace_id,
                &params.database_id,
                std::slice::from_ref(&params.row_id),
                Some(false),
            )
            .await
            .map_err(map_tool_error)?;
        if !matches!(&details, Value::Array(items) if !items.is_empty()) {
            return Err(ToolError::validation(format!(
                "Database row {} was not found in database {}.",
                params.row_id, params.database_id
            )));
        }
        let doc = collab_edit::fetch_collab_doc(
            &self.client,
            &params.workspace_id,
            &params.database_id,
            collab_edit::COLLAB_TYPE_DATABASE,
        )
        .await
        .map_err(map_tool_error)?;
        let since = doc.transact().state_vector();
        let views =
            collab_edit::set_collab_row_deleted(&doc, &params.row_id, deleted, &params.database_id)
                .map_err(map_tool_error)?;
        let update_bytes = collab_edit::push_web_update(
            &self.client,
            &params.workspace_id,
            &params.database_id,
            collab_edit::COLLAB_TYPE_DATABASE,
            &doc,
            &since,
        )
        .await
        .map_err(map_tool_error)?;
        Ok(Json(json!({
            "row_id": params.row_id,
            "views": views,
            "deleted": deleted,
            "update_bytes": update_bytes,
        })))
    }

    #[tool(
        name = "appflowy_delete_row",
        description = "Delete a row from a database exactly as the web UI does: the row is flagged `is_deleted` in place in every view's row order (so it is hidden everywhere) and can be brought back with appflowy_restore_row.",
        annotations(title = "Delete row", destructive_hint = true, open_world_hint = true)
    )]
    async fn appflowy_delete_row(
        &self,
        Parameters(request): Parameters<RowRefRequest>,
    ) -> Result<Json<Value>, ToolError> {
        self.set_row_deleted(request, true).await
    }

    #[tool(
        name = "appflowy_restore_row",
        description = "Undo appflowy_delete_row: clear a deleted row's flag in every view so it becomes visible again.",
        annotations(title = "Restore row", destructive_hint = true, open_world_hint = true)
    )]
    async fn appflowy_restore_row(
        &self,
        Parameters(request): Parameters<RowRefRequest>,
    ) -> Result<Json<Value>, ToolError> {
        self.set_row_deleted(request, false).await
    }

    #[tool(
        name = "appflowy_create_database_field",
        description = "Add a new field (column) to a database.",
        annotations(title = "Create database field", open_world_hint = true)
    )]
    async fn appflowy_create_database_field(
        &self,
        Parameters(request): Parameters<CreateDatabaseFieldRequest>,
    ) -> Result<Json<Value>, ToolError> {
        if request.name.is_empty() {
            return Err(ToolError::validation("Field name must not be empty."));
        }
        if !(0..=7).contains(&request.field_type) {
            return Err(ToolError::validation(
                "field_type must be an AppFlowy field type id in 0..=7 \
                 (0=RichText, 1=Number, 2=DateTime, 3=SingleSelect, \
                 4=MultiSelect, 5=Checkbox, 6=URL, 7=Checklist).",
            ));
        }
        let data = self
            .client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/database/{}/fields",
                    request.workspace_id, request.database_id
                ),
                None,
                // The client strips top-level nulls, matching
                // `model_dump(exclude_none=True)`.
                Some(json!({
                    "name": request.name,
                    "field_type": request.field_type,
                    "type_option_data": request.type_option_data,
                })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_list_database_history",
        description = "List a database's version/checkpoint history (read-only; AppFlowy commercial cloud). Entries carry the version id and its timestamp, so they tell you which point a rollback would target; previewing a version's content and restoring from it are NOT supported. Pagination: `limit` (API default 30) plus an optional `since` (unix seconds) filter are sent directly; subsequent pages are addressed by echoing the response's next-cursor field back as the `cursor` query parameter — set fetch_all=true to follow it automatically and merge every page.",
        annotations(
            title = "List database history",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_list_database_history(
        &self,
        Parameters(request): Parameters<ListDatabaseHistoryRequest>,
    ) -> Result<Json<Value>, ToolError> {
        if request.workspace_id.is_empty() {
            return Err(ToolError::validation("workspace_id is required."));
        }
        if request.database_id.is_empty() {
            return Err(ToolError::validation("database_id is required."));
        }
        if let Some(limit) = request.limit {
            if limit <= 0 {
                return Err(ToolError::validation("limit must be greater than 0."));
            }
        }
        let base_path = format!(
            "/api/workspace/{}/database/{}/history",
            request.workspace_id, request.database_id
        );
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(limit) = request.limit {
            query.push(("limit", limit.to_string()));
        }
        if let Some(since) = request.since {
            query.push(("since", since.to_string()));
        }
        if !request.fetch_all.unwrap_or(false) {
            let first_page = self
                .client
                .request(Method::GET, &base_path, Some(&query), None)
                .await
                .map_err(map_tool_error)?;
            return Ok(Json(first_page));
        }
        let result = fetch_all_history_pages(&self.client, &base_path, &query)
            .await
            .map_err(map_tool_error)?;
        Ok(Json(result))
    }

    #[tool(
        name = "appflowy_get_database_restore_state",
        description = "Check whether a database is currently sitting on a history restore point (read-only; AppFlowy commercial cloud). Returns {database_restore_id, version}: both null when the database is at its live head; when set, the database was restored from that version of its history (see appflowy_list_database_history).",
        annotations(
            title = "Get database restore state",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_get_database_restore_state(
        &self,
        Parameters(request): Parameters<GetDatabaseRestoreStateRequest>,
    ) -> Result<Json<Value>, ToolError> {
        if request.workspace_id.is_empty() {
            return Err(ToolError::validation("workspace_id is required."));
        }
        if request.database_id.is_empty() {
            return Err(ToolError::validation("database_id is required."));
        }
        let data = self
            .client
            .request_ext(
                Method::GET,
                &format!(
                    "/api/workspace/{}/database/{}/restore-state",
                    request.workspace_id, request.database_id
                ),
                None,
                None,
                // The web client sends this so an in-flight restore is
                // reflected instead of a cached state.
                &[("Cache-Control", "no-cache")],
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The UUIDv5 vector published with PR#2 (its test_row_documents.py).
    #[test]
    fn row_document_id_matches_appflowy_uuid5_scheme() {
        assert_eq!(
            database_row_document_id("c89a178d-59a8-4331-90d9-f5198cfba937").unwrap(),
            "9f592119-0028-502d-86fb-91279cc9b3d2"
        );
    }

    #[test]
    fn row_document_id_rejects_non_uuid() {
        let err = database_row_document_id("not-a-row-id").unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "{err:?}");
    }

    #[test]
    fn server_order_reverses_children_recursively() {
        let block = json!({
            "type": "t",
            "children": [
                {"id": 1, "children": [{"id": "a"}, {"id": "b"}]},
                {"id": 2},
            ],
        });
        assert_eq!(
            server_order(block),
            json!({
                "type": "t",
                "children": [
                    {"id": 2},
                    {"id": 1, "children": [{"id": "b"}, {"id": "a"}]},
                ],
            })
        );
    }

    #[test]
    fn history_page_items_recognizes_known_shapes() {
        // A follow-up page the fetch_all loop cannot parse is a hard error
        // (silent drop would lose data), so the recognized shapes matter.
        assert_eq!(
            history_page_items(&json!({"data": [{"v": 1}]})),
            Some(vec![json!({"v": 1})])
        );
        assert_eq!(
            history_page_items(&json!([{"v": 1}, {"v": 2}])),
            Some(vec![json!({"v": 1}), json!({"v": 2})])
        );
        // Unrecognized: a bare object with no known array key.
        assert_eq!(history_page_items(&json!({"items": "not-an-array"})), None);
        assert_eq!(history_page_items(&json!({"foo": 1})), None);
    }

    #[test]
    fn history_next_cursor_finds_nested_cursors() {
        assert_eq!(
            history_next_cursor(&json!({"next_cursor": "abc"})),
            Some("abc".to_string())
        );
        assert_eq!(
            history_next_cursor(&json!({"pagination": {"next": "abc"}})),
            Some("abc".to_string())
        );
        // Empty string or null means "no next page".
        assert_eq!(history_next_cursor(&json!({"next_cursor": ""})), None);
        assert_eq!(history_next_cursor(&json!({})), None);
    }

    // ---------------------------------------------------------------
    // WIL-21 Bug 7: fetch_all must never silently report partial
    // history as complete. These drive `fetch_all_history_pages`
    // against an httpmock server paging one item at a time.
    // ---------------------------------------------------------------

    const HISTORY_PATH: &str = "/api/workspace/ws/database/db/history";

    /// Serve `page_count` one-item pages under the object `{"items": [...]}`
    /// shape: the cursor-less request yields page 1, `?cursor=cN` yields
    /// page N+1; `last_next_cursor` controls whether the final page still
    /// advertises a next page. Must not use a top-level `data` key —
    /// `client.request` unwraps that envelope and would drop the cursor.
    async fn mock_history_pages(
        server: &httpmock::MockServer,
        page_count: usize,
        last_next_cursor: Option<&str>,
    ) {
        let page_body = |page: usize| {
            if page < page_count {
                json!({"items": [{"v": page}], "next_cursor": format!("c{page}")})
            } else {
                match last_next_cursor {
                    Some(cursor) => json!({"items": [{"v": page}], "next_cursor": cursor}),
                    None => json!({"items": [{"v": page}]}),
                }
            }
        };
        // Specific cursor mocks must be registered before the unconstrained
        // page-1 mock: httpmock serves the first matching mock, so a
        // path-only mock created first would answer every follow-up page.
        for page in 2..=page_count {
            let body = page_body(page);
            let cursor = format!("c{}", page - 1);
            server
                .mock_async(move |when, then| {
                    when.method(httpmock::Method::GET)
                        .path(HISTORY_PATH)
                        .query_param("cursor", cursor);
                    then.status(200).json_body(body);
                })
                .await;
        }
        let body = page_body(1);
        server
            .mock_async(move |when, then| {
                when.method(httpmock::Method::GET).path(HISTORY_PATH);
                then.status(200).json_body(body);
            })
            .await;
    }

    #[tokio::test]
    async fn fetch_all_merges_follow_up_pages() {
        let server = httpmock::MockServer::start_async().await;
        mock_history_pages(&server, 3, None).await;
        let client = crate::client::AppFlowyClient::new(None, None, server.base_url());
        let result = fetch_all_history_pages(&client, HISTORY_PATH, &[])
            .await
            .expect("complete pagination merges");
        assert_eq!(result["items"].as_array().unwrap().len(), 3);
        assert_eq!(result["pages_fetched"], json!(3));
        // Normal termination: no truncation signal anywhere.
        assert!(result.get("next_cursor").is_none());
    }

    /// Exactly at the page cap with no cursor left is a normal, complete
    /// result — the cap only errors when a next page remains.
    #[tokio::test]
    async fn fetch_all_completes_exactly_at_page_cap() {
        let server = httpmock::MockServer::start_async().await;
        mock_history_pages(&server, MAX_HISTORY_PAGES, None).await;
        let client = crate::client::AppFlowyClient::new(None, None, server.base_url());
        let result = fetch_all_history_pages(&client, HISTORY_PATH, &[])
            .await
            .expect("cap without a remaining cursor is complete");
        assert_eq!(result["items"].as_array().unwrap().len(), MAX_HISTORY_PAGES);
        assert_eq!(result["pages_fetched"], json!(MAX_HISTORY_PAGES));
    }

    /// A cursor still live at the cap used to fall out of the loop and
    /// return the partial merge as success; it must now be a hard error.
    #[tokio::test]
    async fn fetch_all_errors_when_cursor_survives_page_cap() {
        let server = httpmock::MockServer::start_async().await;
        mock_history_pages(&server, MAX_HISTORY_PAGES, Some("c100")).await;
        let client = crate::client::AppFlowyClient::new(None, None, server.base_url());
        let err = fetch_all_history_pages(&client, HISTORY_PATH, &[])
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("limit of 100 pages"), "{msg}");
        assert!(msg.contains("c100"), "{msg}");
    }

    /// Bare-array pages carry no cursor, so the array shape terminates
    /// after the first page and comes back as a merged array.
    #[tokio::test]
    async fn fetch_all_array_shape_first_page_stays_an_array() {
        let server = httpmock::MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET).path(HISTORY_PATH);
                then.status(200).json_body(json!([{"v": 1}, {"v": 2}]));
            })
            .await;
        let client = crate::client::AppFlowyClient::new(None, None, server.base_url());
        let result = fetch_all_history_pages(&client, HISTORY_PATH, &[])
            .await
            .expect("array shape merges");
        assert_eq!(result, json!([{"v": 1}, {"v": 2}]));
    }
}

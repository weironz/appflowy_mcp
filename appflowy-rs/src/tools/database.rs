//! Database, row, and field tools.

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, Json, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::AppServer;
use crate::tools::util::map_tool_error;

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

#[tool_router(router = database_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_list_databases",
        description = "List all databases in a workspace."
    )]
    async fn appflowy_list_databases(
        &self,
        Parameters(request): Parameters<ListDatabasesRequest>,
    ) -> Result<Json<Value>, ErrorData> {
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
        description = "Get fields of a specific database."
    )]
    async fn appflowy_get_database_fields(
        &self,
        Parameters(request): Parameters<GetDatabaseFieldsRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let fields = self
            .client
            .get_database_fields(&request.workspace_id, &request.database_id)
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data_or_empty_list(fields)))
    }

    #[tool(
        name = "appflowy_list_rows",
        description = "List all row IDs in a database."
    )]
    async fn appflowy_list_rows(
        &self,
        Parameters(request): Parameters<ListRowsRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let rows = self
            .client
            .get_database_row_ids(&request.workspace_id, &request.database_id)
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data_or_empty_list(rows)))
    }

    #[tool(
        name = "appflowy_get_row_details",
        description = "Get details of specific rows by IDs."
    )]
    async fn appflowy_get_row_details(
        &self,
        Parameters(request): Parameters<GetRowDetailsRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let ids: Vec<String> = request
            .row_ids
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect();
        if ids.is_empty() {
            return Err(ErrorData::invalid_params(
                "At least one row ID is required.",
                None,
            ));
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
        description = "Create a new row in a database."
    )]
    async fn appflowy_create_row(
        &self,
        Parameters(request): Parameters<CreateRowRequest>,
    ) -> Result<Json<Value>, ErrorData> {
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
        Ok(Json(json!({ "id": row_id })))
    }

    #[tool(
        name = "appflowy_upsert_row",
        description = "Update existing row or create if it doesn't exist."
    )]
    async fn appflowy_upsert_row(
        &self,
        Parameters(request): Parameters<UpsertRowRequest>,
    ) -> Result<Json<Value>, ErrorData> {
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
        Ok(Json(json!({ "id": row_id })))
    }

    #[tool(
        name = "appflowy_get_updated_rows",
        description = "Find updated rows in a database after a specific datetime."
    )]
    async fn appflowy_get_updated_rows(
        &self,
        Parameters(request): Parameters<GetUpdatedRowsRequest>,
    ) -> Result<Json<Value>, ErrorData> {
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
        name = "appflowy_create_database_field",
        description = "Add a new field (column) to a database."
    )]
    async fn appflowy_create_database_field(
        &self,
        Parameters(request): Parameters<CreateDatabaseFieldRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        if request.name.is_empty() {
            return Err(ErrorData::invalid_params(
                "Field name must not be empty.",
                None,
            ));
        }
        if !(0..=7).contains(&request.field_type) {
            return Err(ErrorData::invalid_params(
                "field_type must be an AppFlowy field type id in 0..=7 \
                 (0=RichText, 1=Number, 2=DateTime, 3=SingleSelect, \
                 4=MultiSelect, 5=Checkbox, 6=URL, 7=Checklist).",
                None,
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
}

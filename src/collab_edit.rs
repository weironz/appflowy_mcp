//! Row-level writes through AppFlowy's collab (yjs) layer.
//!
//! The REST API can create database rows but cannot edit or delete them; the
//! web client mutates the yjs docs and syncs only the diff. The server accepts
//! the same shape over `POST /api/workspace/v1/{ws}/collab/{id}/web-update`:
//! `{"doc_state": [<yjs v1 update bytes as ints>], "collab_type": <int>}`.
//! This module ports the community-verified `collab.py` flow:
//!
//! ```text
//! GET /api/workspace/v1/{ws}/collab/{id}?collab_type=N -> yrs Doc
//! -> mutate in a transaction -> diff since the fetched state vector
//! -> POST web-update
//! ```
//!
//! Collab layouts mirrored from the AppFlowy web client:
//!
//! * row collab ([`COLLAB_TYPE_DATABASE_ROW`]): `data.data.cells[field_id]`
//!   holds a Map `{data, field_type, created_at, last_modified, ...}` whose
//!   `data` is a string.
//! * database collab ([`COLLAB_TYPE_DATABASE`]):
//!   `data.database.views[view_id].row_orders` is an Array of *plain* (not
//!   shared-type) `{id, height}` values; a deleted row stays in place as
//!   `{id, height, is_deleted: true}` in every view (soft delete, restorable).

use reqwest::Method;
use serde_json::{json, Map as JsonMap, Value};
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Array, ArrayRef, Doc, In, Map, MapPrelim, MapRef, Out, ReadTxn, StateVector, Transact,
    Update,
};

use crate::b64::base64_decode;
use crate::client::AppFlowyClient;
use crate::error::{Error, Result};

/// CollabType::Database — carries the views and their `row_orders`.
pub const COLLAB_TYPE_DATABASE: i64 = 1;
/// CollabType::DatabaseRow — carries one row's cells.
pub const COLLAB_TYPE_DATABASE_ROW: i64 = 4;

// FieldType ids (AppFlowy collab-database).
pub const FIELD_TYPE_RICH_TEXT: i64 = 0;
pub const FIELD_TYPE_NUMBER: i64 = 1;
pub const FIELD_TYPE_DATE_TIME: i64 = 2;
pub const FIELD_TYPE_SINGLE_SELECT: i64 = 3;
pub const FIELD_TYPE_MULTI_SELECT: i64 = 4;
pub const FIELD_TYPE_CHECKBOX: i64 = 5;
pub const FIELD_TYPE_URL: i64 = 6;
pub const FIELD_TYPE_CHECKLIST: i64 = 7;
/// LastEditedTime/CreatedTime: computed by the app, rejected on write.
pub const READ_ONLY_FIELD_TYPES: [i64; 2] = [8, 9];

// ---------------------------------------------------------------------------
// fetch / diff / push
// ---------------------------------------------------------------------------

/// Port of `collab.py::_doc_state`. The collab GET payload arrives
/// envelope-unwrapped, so the doc_state may sit at the top level or under
/// `encode_collab`/`encoded_collab`, as an int array or a base64 string.
pub(crate) fn decode_doc_state(data: &Value) -> Result<Vec<u8>> {
    fn bytes_from(value: &Value) -> Option<Option<Vec<u8>>> {
        match value {
            Value::String(s) => Some(base64_decode(s)),
            Value::Array(items) if !items.is_empty() => Some(
                items
                    .iter()
                    .map(|item| item.as_u64().and_then(|n| u8::try_from(n).ok()))
                    .collect(),
            ),
            _ => None,
        }
    }
    let Value::Object(map) = data else {
        return Err(Error::Other(
            "collab response carried no doc_state (not an object)".into(),
        ));
    };
    if let Some(bytes) = map.get("doc_state").and_then(bytes_from) {
        return bytes.ok_or_else(|| {
            Error::Other("collab response doc_state is not a usable byte payload".into())
        });
    }
    for wrapper in ["encode_collab", "encoded_collab"] {
        if let Some(bytes) = map.get(wrapper).and_then(bytes_from) {
            return bytes.ok_or_else(|| {
                Error::Other(format!(
                    "collab response {wrapper} is not a usable byte payload"
                ))
            });
        }
    }
    Err(Error::Other("collab response carried no doc_state".into()))
}

/// `GET /api/workspace/v1/{ws}/collab/{object_id}?collab_type=N`, decoded into
/// a yrs doc; the doc is fully synced to the server's current state.
pub async fn fetch_collab_doc(
    client: &AppFlowyClient,
    workspace_id: &str,
    object_id: &str,
    collab_type: i64,
) -> Result<Doc> {
    let path = format!("/api/workspace/v1/{workspace_id}/collab/{object_id}");
    let params = [("collab_type", collab_type.to_string())];
    let data = client
        .request(Method::GET, &path, Some(&params), None)
        .await?;
    let bytes = decode_doc_state(&data)?;
    let update = Update::decode_v1(&bytes)
        .map_err(|e| Error::Other(format!("invalid yjs update for collab {object_id}: {e}")))?;
    let doc = Doc::new();
    doc.transact_mut().apply_update(update).map_err(|e| {
        Error::Other(format!(
            "failed to apply yjs update for collab {object_id}: {e}"
        ))
    })?;
    Ok(doc)
}

/// Send only what changed since `since`; returns the update size in bytes.
///
/// `encode_state_as_update_v1(&state_vector)` produces the minimal diff — the
/// alternative (re-encoding the whole doc state) costs the full collab size
/// in JSON-int form on every save; the diff is a few hundred bytes for a
/// typical cell edit.
pub async fn push_web_update(
    client: &AppFlowyClient,
    workspace_id: &str,
    object_id: &str,
    collab_type: i64,
    doc: &Doc,
    since: &StateVector,
) -> Result<usize> {
    let update = doc.transact().encode_state_as_update_v1(since);
    let body = json!({
        "doc_state": update.iter().map(|b| json!(*b)).collect::<Vec<_>>(),
        "collab_type": collab_type,
    });
    client
        .request(
            Method::POST,
            &format!("/api/workspace/v1/{workspace_id}/collab/{object_id}/web-update"),
            None,
            Some(body),
        )
        .await?;
    Ok(update.len())
}

// ---------------------------------------------------------------------------
// cell encoding
// ---------------------------------------------------------------------------

/// One database field as the REST `/fields` payload describes it.
#[derive(Clone)]
pub(crate) struct CellField {
    pub id: String,
    pub name: String,
    pub field_type: i64,
    /// Parsed `type_option_data.content` object (select options live here).
    pub content: Value,
}

impl CellField {
    /// Port of `_options`: `type_option.content.options` (content may itself
    /// still be a JSON string).
    fn options(&self) -> Vec<Value> {
        let content = match &self.content {
            Value::String(raw) => serde_json::from_str(raw).unwrap_or_else(|_| json!({})),
            other => other.clone(),
        };
        content
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    /// Port of `_option_id`: an option may be named by id or by name.
    fn option_id(&self, value: &str) -> std::result::Result<String, String> {
        let options = self.options();
        for opt in &options {
            let id = opt.get("id").and_then(Value::as_str);
            let name = opt.get("name").and_then(Value::as_str);
            if id == Some(value) || name == Some(value) {
                return Ok(id.unwrap_or_default().to_string());
            }
        }
        let names: Vec<String> = options
            .iter()
            .filter_map(|opt| opt.get("name").and_then(Value::as_str).map(str::to_string))
            .collect();
        Err(format!(
            "field '{}' has no option {value:?} (available: {})",
            self.name,
            names.join(", ")
        ))
    }
}

/// Port of `_field_for`: match a cells key against a field id or the trimmed
/// field name.
pub(crate) fn field_for<'a>(fields: &'a [CellField], key: &str) -> Result<&'a CellField> {
    let trimmed = key.trim();
    for field in fields {
        if key == field.id || trimmed == field.name.trim() {
            return Ok(field);
        }
    }
    let names: Vec<String> = fields
        .iter()
        .map(|field| format!("{:?}", field.name))
        .collect();
    Err(Error::Validation(format!(
        "No field {key:?} (fields: {}).",
        names.join(", ")
    )))
}

/// Parse the REST `/fields` payload into [`CellField`]s. Field entries
/// without a usable id/name are skipped rather than failing the whole write.
pub(crate) fn parse_fields(data: &Value) -> Vec<CellField> {
    let items = data.as_array().cloned().unwrap_or_default();
    let mut fields = Vec::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let field_type = item
            .get("field_type")
            .or_else(|| item.get("field_type_id"))
            .and_then(Value::as_i64)
            .unwrap_or(FIELD_TYPE_RICH_TEXT);
        let content = item
            .get("type_option_data")
            .and_then(|tod| tod.get("content").cloned())
            .unwrap_or_else(|| json!({}));
        fields.push(CellField {
            id: id.to_string(),
            name: name.to_string(),
            field_type,
            content,
        });
    }
    fields
}

/// A field's stage for one `cells` entry: encoding happened up front, so a
/// bad value anywhere rejects the whole request without writing anything.
pub(crate) struct StagedCell {
    pub field: CellField,
    pub entries: JsonMap<String, Value>,
}

/// Stringify a JSON scalar the way Python `str(value)` does for the
/// text/number/url cell forms. Objects/arrays are rejected as nonsense here.
fn scalar_to_string(value: &Value) -> std::result::Result<String, String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null => Ok(String::new()),
        other => Err(format!("expected a string/number/bool value, got {other}")),
    }
}

fn value_as_names(value: &Value) -> std::result::Result<Vec<String>, String> {
    match value {
        Value::String(s) => Ok(vec![s.clone()]),
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items.iter().map(scalar_to_string).collect(),
        other => Err(format!(
            "expected an option name or a list of option names, got {other}"
        )),
    }
}

// --- unix date parsing (no chrono in the dep tree) --------------------------

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01 for a
/// proleptic-Gregorian y/m/d.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month + 9).rem_euclid(12);
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parse_u32(s: &str, lo: u32, hi: u32) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u32 = s.parse().ok()?;
    (lo..=hi).contains(&value).then_some(value)
}

/// Parse an ISO 8601 date/datetime into unix seconds.
///
/// Accepted: `YYYY-MM-DD`, `YYYY-MM-DD[ T]HH:MM[:SS[.frac]]` with an optional
/// `Z` or `±HH:MM` offset. Naive datetimes are treated as UTC, matching the
/// Python reference (`dt.replace(tzinfo=timezone.utc)`).
pub(crate) fn parse_iso_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date_part, time_part) = match s.find(['T', ' ']) {
        Some(idx) => (&s[..idx], Some(&s[idx + 1..])),
        None => (s, None),
    };
    let mut date = date_part.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month = parse_u32(date.next()?, 1, 12)? as i64;
    let day = parse_u32(date.next()?, 1, 31)? as i64;
    date.next().is_none().then_some(())?;

    let (mut hour, mut minute, mut second, mut offset_secs) = (0i64, 0i64, 0i64, 0i64);
    if let Some(time_part) = time_part {
        let time_part = time_part.trim();
        let (hms, offset) = if let Some(zone) = time_part.strip_suffix('Z') {
            (zone, 0i64)
        } else if let Some(idx) = time_part[1..].find(['+', '-']).map(|idx| idx + 1) {
            let sign = if time_part.as_bytes()[idx] == b'-' {
                -1
            } else {
                1
            };
            let (oh, om) = time_part[idx + 1..]
                .split_once(':')
                .unwrap_or((&time_part[idx + 1..], "0"));
            let oh = parse_u32(oh, 0, 23)?;
            let om = parse_u32(om, 0, 59)?;
            (
                &time_part[..idx],
                sign * ((oh as i64) * 3600 + (om as i64) * 60),
            )
        } else {
            (time_part, 0)
        };
        offset_secs = offset;
        let hms = hms.split('.').next().unwrap_or(hms);
        let mut parts = hms.split(':');
        hour = parse_u32(parts.next()?, 0, 24)? as i64;
        minute = parts.next().map_or(Some(0), |m| parse_u32(m, 0, 59))? as i64;
        second = parts.next().map_or(Some(0), |s| parse_u32(s, 0, 60))? as i64;
        parts.next().is_none().then_some(())?;
    }
    Some(
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
            - offset_secs,
    )
}

/// Value -> unix seconds: numbers pass through; strings go through
/// [`parse_iso_timestamp`].
fn unix_seconds(value: &Value) -> std::result::Result<i64, String> {
    match value {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .ok_or_else(|| format!("not a unix timestamp: {n}")),
        Value::String(s) => {
            parse_iso_timestamp(s).ok_or_else(|| format!("not an ISO 8601 date/datetime: {s:?}"))
        }
        other => Err(format!("not a date value: {other}")),
    }
}

fn new_option_id() -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-";
    uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .bytes()
        .take(6)
        .map(|b| CHARS[(b % 64) as usize] as char)
        .collect()
}

/// Port of `_checklist`: either replace the whole list, or edit it by task
/// name via `{add, remove, check, uncheck}`.
fn encode_checklist(value: &Value, current: Option<&str>) -> std::result::Result<String, String> {
    let current: Value = current
        .filter(|raw| !raw.trim().is_empty())
        .map(|raw| serde_json::from_str(raw).unwrap_or_else(|_| json!({})))
        .unwrap_or_else(|| json!({}));
    let mut options: Vec<Value> = current
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut selected: Vec<Value> = current
        .get("selected_option_ids")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    fn name_of(opt: &Value) -> &str {
        opt.get("name").and_then(Value::as_str).unwrap_or("")
    }
    fn id_of(opt: &Value) -> &str {
        opt.get("id").and_then(Value::as_str).unwrap_or("")
    }
    let describe = |options: &[Value]| {
        options
            .iter()
            .map(|opt| format!("{:?}", name_of(opt)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let ensure = |name: &str, options: &mut Vec<Value>| -> usize {
        if let Some(idx) = options.iter().position(|opt| name_of(opt) == name) {
            idx
        } else {
            options.push(json!({"id": new_option_id(), "name": name, "color": "Purple"}));
            options.len() - 1
        }
    };

    match value {
        Value::Object(map) => {
            let known = ["add", "remove", "check", "uncheck"];
            if !known.iter().any(|key| map.contains_key(*key)) {
                return Err(
                    "checklist edits need one of {add, remove, check, uncheck: [task names]}"
                        .to_string(),
                );
            }
            for name in value_as_names(map.get("add").unwrap_or(&Value::Null))? {
                ensure(&name, &mut options);
            }
            for name in value_as_names(map.get("remove").unwrap_or(&Value::Null))? {
                let Some(idx) = options.iter().position(|opt| name_of(opt) == name) else {
                    return Err(format!(
                        "no checklist task {name:?} (tasks: {})",
                        describe(&options)
                    ));
                };
                let removed = options.remove(idx);
                let removed_id = id_of(&removed);
                selected.retain(|sel| sel.as_str() != Some(removed_id));
            }
            for name in value_as_names(map.get("check").unwrap_or(&Value::Null))? {
                let Some(opt) = options.iter().find(|opt| name_of(opt) == name) else {
                    return Err(format!(
                        "no checklist task {name:?} (tasks: {})",
                        describe(&options)
                    ));
                };
                let id = id_of(opt).to_string();
                if !selected.iter().any(|sel| sel.as_str() == Some(&id)) {
                    selected.push(json!(id));
                }
            }
            for name in value_as_names(map.get("uncheck").unwrap_or(&Value::Null))? {
                let Some(opt) = options.iter().find(|opt| name_of(opt) == name) else {
                    return Err(format!(
                        "no checklist task {name:?} (tasks: {})",
                        describe(&options)
                    ));
                };
                let id = id_of(opt);
                selected.retain(|sel| sel.as_str() != Some(id));
            }
        }
        Value::Array(items) => {
            let mut kept: Vec<Value> = Vec::with_capacity(items.len());
            let mut done: Vec<Value> = Vec::new();
            for item in items {
                let (name, is_done) = match item {
                    Value::String(name) => (name.clone(), false),
                    Value::Object(_) => {
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                format!("checklist item {item} has no name")
                            })?
                            .to_string();
                        let is_done = item
                            .get("done")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        (name, is_done)
                    }
                    other => return Err(format!("checklist item must be a name or an object, got {other}")),
                };
                let idx = ensure(&name, &mut options);
                let opt = options[idx].clone();
                if is_done {
                    done.push(json!(id_of(&opt)));
                }
                kept.push(opt);
            }
            options = kept;
            selected = done;
        }
        other => {
            return Err(format!(
                "checklist value must be a task list or a {{check, uncheck, add, remove}} dict, got {other}"
            ))
        }
    }
    serde_json::to_string(&json!({
        "options": options,
        "selected_option_ids": selected,
    }))
    .map_err(|e| e.to_string())
}

/// Port of `encode_cell`: cell-map entries (besides timestamps) for `value`
/// on `field`. `current` is the existing cell `data`, needed by checklist
/// toggle-by-name.
pub(crate) fn encode_cell(
    field: &CellField,
    value: &Value,
    current: Option<&str>,
) -> std::result::Result<JsonMap<String, Value>, String> {
    let field_err = |message: String| format!("Field {:?}: {message}", field.name);
    if READ_ONLY_FIELD_TYPES.contains(&field.field_type) {
        return Err(field_err(format!(
            "field type {} is computed and cannot be set",
            field.field_type
        )));
    }
    let data_cell = |data: std::result::Result<String, String>| -> std::result::Result<JsonMap<String, Value>, String> {
        let mut entries = JsonMap::new();
        entries.insert("data".to_string(), json!(data.map_err(field_err)?));
        Ok(entries)
    };
    match field.field_type {
        FIELD_TYPE_RICH_TEXT | FIELD_TYPE_NUMBER | FIELD_TYPE_URL => {
            if value.is_null() {
                data_cell(Ok(String::new()))
            } else {
                data_cell(scalar_to_string(value))
            }
        }
        FIELD_TYPE_SINGLE_SELECT => {
            if scalar_to_string(value).unwrap_or_default().is_empty() {
                data_cell(Ok(String::new()))
            } else {
                let name = scalar_to_string(value)?;
                data_cell(field.option_id(&name))
            }
        }
        FIELD_TYPE_MULTI_SELECT => {
            let names = value_as_names(value).map_err(field_err)?;
            let mut ids = Vec::with_capacity(names.len());
            for name in &names {
                ids.push(field.option_id(name).map_err(field_err)?);
            }
            data_cell(Ok(ids.join(",")))
        }
        FIELD_TYPE_CHECKBOX => {
            let truthy = *value == Value::Bool(true)
                || value
                    .as_str()
                    .is_some_and(|s| ["yes", "true", "1"].contains(&s.to_lowercase().as_str()))
                || value.as_i64() == Some(1);
            data_cell(Ok(if truthy { "Yes" } else { "No" }.to_string()))
        }
        FIELD_TYPE_CHECKLIST => data_cell(encode_checklist(value, current)),
        FIELD_TYPE_DATE_TIME => {
            let (start, end, include_time) = match value {
                Value::Object(map) => {
                    let start = map.get("start").cloned().ok_or_else(|| {
                        field_err("a date object needs a 'start' value".to_string())
                    })?;
                    let end = map.get("end").cloned().filter(|v| !v.is_null());
                    let include_time = map
                        .get("include_time")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    (start, end, include_time)
                }
                other => (other.clone(), None, false),
            };
            let mut entries = JsonMap::new();
            entries.insert(
                "data".to_string(),
                json!(unix_seconds(&start).map_err(field_err)?.to_string()),
            );
            entries.insert("include_time".to_string(), json!(include_time));
            entries.insert("is_range".to_string(), json!(end.is_some()));
            if let Some(end) = end {
                entries.insert(
                    "end_timestamp".to_string(),
                    json!(unix_seconds(&end).map_err(field_err)?.to_string()),
                );
            }
            Ok(entries)
        }
        other => Err(field_err(format!(
            "field type {other} is not supported yet"
        ))),
    }
}

// ---------------------------------------------------------------------------
// yjs mutations on a fetched doc
// ---------------------------------------------------------------------------

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// A timestamp keeps the type the row already uses: the web writes strings,
/// some older rows carry numbers.
#[derive(Clone, Copy)]
enum Stamp {
    Text(i64),
    Number(i64),
}

impl Stamp {
    fn any(self) -> Any {
        match self {
            Stamp::Text(secs) => Any::String(secs.to_string().into()),
            Stamp::Number(secs) => Any::from(secs),
        }
    }
}

fn expect_map(out: Out, what: &str) -> Result<MapRef> {
    match out {
        Out::YMap(map) => Ok(map),
        other => Err(Error::Other(format!(
            "collab path {what} is not a map (found {other:?})"
        ))),
    }
}

fn value_to_in(value: &Value) -> Result<In> {
    let any: Any = serde_json::from_value(value.clone())
        .map_err(|e| Error::Other(format!("cell value not representable in the collab: {e}")))?;
    Ok(In::from(any))
}

/// Apply staged cell mutations to a fetched row collab; returns the field
/// names written, in input order.
///
/// Root refs are resolved before the write transaction opens: `get` on a
/// root map is read-only, while resolving a missing root takes the store
/// write lock and would deadlock against an open transaction.
pub(crate) fn mutate_row_cells(
    doc: &Doc,
    database_id: &str,
    staged: &[StagedCell],
    row_id: &str,
) -> Result<Vec<String>> {
    let root = doc.get_or_insert_map("data");
    let stamp;
    let row;
    {
        let txn = doc.transact();
        row = expect_map(
            root.get(&txn, "data").ok_or_else(|| {
                Error::Validation(format!(
                    "Database row {row_id} has no row data in its collab."
                ))
            })?,
            "root/data",
        )?;
        if let Some(database_of_row) = row.get(&txn, "database_id") {
            let database_of_row = database_of_row.to_string(&txn);
            if database_of_row != database_id {
                return Err(Error::Validation(format!(
                    "Database row {row_id} belongs to database {database_of_row}."
                )));
            }
        }
        stamp = match row.get(&txn, "last_modified") {
            Some(Out::Any(Any::String(_))) => Stamp::Text(now_unix()),
            _ => Stamp::Number(now_unix()),
        };
    }

    let mut txn = doc.transact_mut();
    let cells: MapRef = match row.get(&txn, "cells") {
        Some(Out::YMap(map)) => map,
        _ => row.insert(
            &mut txn,
            "cells",
            MapPrelim::from_iter(Vec::<(&str, In)>::new()),
        ),
    };

    let mut updated = Vec::with_capacity(staged.len());
    for staged in staged {
        let field = &staged.field;
        updated.push(field.name.clone());
        let existing = match cells.get(&txn, &field.id) {
            Some(Out::YMap(map)) => Some(map),
            _ => None,
        };
        match existing {
            None => {
                let mut entries: Vec<(&str, In)> = staged
                    .entries
                    .iter()
                    .map(|(key, value)| value_to_in(value).map(|input| (key.as_str(), input)))
                    .collect::<Result<_>>()?;
                entries.push(("field_type", In::from(Any::from(field.field_type))));
                entries.push(("created_at", In::from(stamp.any())));
                entries.push(("last_modified", In::from(stamp.any())));
                cells.insert(&mut txn, field.id.as_str(), MapPrelim::from_iter(entries));
            }
            Some(cell) => {
                for (key, value) in &staged.entries {
                    cell.insert(&mut txn, key.as_str(), value_to_in(value)?);
                }
                if field.field_type == FIELD_TYPE_DATE_TIME
                    && !staged.entries.contains_key("end_timestamp")
                {
                    cell.remove(&mut txn, "end_timestamp");
                }
                cell.insert(
                    &mut txn,
                    "field_type",
                    In::from(Any::from(field.field_type)),
                );
                // Left over from a previous field-type change; the web client
                // clears it once the cell is rewritten with the new type.
                cell.remove(&mut txn, "source_field_type");
                cell.insert(&mut txn, "last_modified", In::from(stamp.any()));
            }
        }
    }
    row.insert(&mut txn, "last_modified", In::from(stamp.any()));
    txn.commit();
    Ok(updated)
}

/// The current cell `data` string for a field (needed by checklist
/// toggle-by-name), or `None` when the row/cell/data is absent.
pub(crate) fn current_cell_data(doc: &Doc, field_id: &str) -> Option<String> {
    let root = doc.get_or_insert_map("data");
    let txn = doc.transact();
    let row = match root.get(&txn, "data") {
        Some(Out::YMap(map)) => map,
        _ => return None,
    };
    let cells = match row.get(&txn, "cells") {
        Some(Out::YMap(map)) => map,
        _ => return None,
    };
    match cells.get(&txn, field_id) {
        Some(Out::YMap(cell)) => match cell.get(&txn, "data") {
            Some(Out::Any(Any::String(s))) => Some(s.to_string()),
            Some(other) => Some(other.to_string(&txn)),
            None => None,
        },
        _ => None,
    }
}

/// Port of `_set_row_deleted`: flag a row in every view's `row_orders`,
/// the way deleting/restoring it in the web UI does. Entries are plain yjs
/// values (not shared maps), so an entry is replaced by
/// remove-index + insert-index. Returns how many view entries were touched.
pub(crate) fn set_collab_row_deleted(
    doc: &Doc,
    row_id: &str,
    deleted: bool,
    database_id: &str,
) -> Result<usize> {
    let root = doc.get_or_insert_map("data");
    let row_order_arrays: Vec<ArrayRef>;
    {
        let txn = doc.transact();
        let database = expect_map(
            root.get(&txn, "database").ok_or_else(|| {
                Error::Other(format!(
                    "Database collab {database_id} has no data/database map."
                ))
            })?,
            "root/database",
        )?;
        let views = expect_map(
            database.get(&txn, "views").ok_or_else(|| {
                Error::Other(format!(
                    "Database collab {database_id} has no database/views map."
                ))
            })?,
            "database/views",
        )?;
        row_order_arrays = views
            .iter(&txn)
            .filter_map(|(_, out)| match out {
                Out::YMap(view) => match view.get(&txn, "row_orders") {
                    Some(Out::YArray(array)) => Some(array),
                    _ => None,
                },
                _ => None,
            })
            .collect();
    }

    let mut txn = doc.transact_mut();
    let mut touched = 0usize;
    for orders in &row_order_arrays {
        for index in 0..orders.len(&txn) {
            let Some(Out::Any(any)) = orders.get(&txn, index) else {
                continue;
            };
            let Ok(mut entry) = serde_json::to_value(&any) else {
                continue;
            };
            if entry.get("id").and_then(Value::as_str) != Some(row_id) {
                continue;
            }
            let was_deleted = entry
                .get("is_deleted")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            touched += 1;
            if was_deleted == deleted {
                continue;
            }
            if deleted {
                entry
                    .as_object_mut()
                    .map(|map| map.insert("is_deleted".to_string(), json!(true)));
            } else {
                entry.as_object_mut().map(|map| map.remove("is_deleted"));
            }
            let replacement: Any = serde_json::from_value(entry)
                .map_err(|e| Error::Other(format!("row_orders entry not encodable: {e}")))?;
            orders.remove(&mut txn, index);
            orders.insert(&mut txn, index, In::from(replacement));
        }
    }
    txn.commit();
    if touched == 0 {
        return Err(Error::Validation(format!(
            "Row {row_id} is not in any view of database {database_id}."
        )));
    }
    Ok(touched)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(id: &str, name: &str, field_type: i64, content: Value) -> CellField {
        CellField {
            id: id.to_string(),
            name: name.to_string(),
            field_type,
            content,
        }
    }

    fn status_field() -> CellField {
        field(
            "S",
            "Status",
            FIELD_TYPE_SINGLE_SELECT,
            json!({"options": [{"id": "b", "name": "Backlog"}, {"id": "d", "name": "Doing"}]}),
        )
    }

    fn multi_field() -> CellField {
        field(
            "K",
            "Kategori ",
            FIELD_TYPE_MULTI_SELECT,
            json!({"options": [{"id": "t", "name": "theme"}, {"id": "f", "name": "features"}]}),
        )
    }

    fn empty_map() -> MapPrelim {
        MapPrelim::from_iter(Vec::<(&str, In)>::new())
    }

    fn any_json(value: Value) -> In {
        In::from(serde_json::from_value::<Any>(value).unwrap())
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

    fn out_json(out: &Out) -> Value {
        match out {
            Out::Any(any) => serde_json::to_value(any).unwrap(),
            other => json!(format!("{other:?}")),
        }
    }

    fn read_cell(doc: &Doc, cell_id: &str) -> serde_json::Map<String, Value> {
        let root = doc.get_or_insert_map("data");
        let txn = doc.transact();
        let Some(Out::YMap(row)) = root.get(&txn, "data") else {
            panic!("no row map");
        };
        let Some(Out::YMap(cells)) = row.get(&txn, "cells") else {
            panic!("no cells map");
        };
        let Some(Out::YMap(cell)) = cells.get(&txn, cell_id) else {
            panic!("no cell {cell_id}");
        };
        cell.iter(&txn)
            .map(|(key, out)| (key.to_string(), out_json(&out)))
            .collect()
    }

    fn read_row_key(doc: &Doc, key: &str) -> Value {
        let root = doc.get_or_insert_map("data");
        let txn = doc.transact();
        let Some(Out::YMap(row)) = root.get(&txn, "data") else {
            panic!("no row map");
        };
        out_json(&row.get(&txn, key).expect("row key"))
    }

    /// All views' row_orders, as JSON (one array per view).
    fn row_orders_json(doc: &Doc) -> Vec<Value> {
        let root = doc.get_or_insert_map("data");
        let txn = doc.transact();
        let Some(Out::YMap(database)) = root.get(&txn, "database") else {
            panic!("no database map");
        };
        let Some(Out::YMap(views)) = database.get(&txn, "views") else {
            panic!("no views map");
        };
        views
            .iter(&txn)
            .map(|(_, out)| {
                let Out::YMap(view) = out else {
                    panic!("view is not a map");
                };
                let Some(Out::YArray(orders)) = view.get(&txn, "row_orders") else {
                    panic!("no row_orders");
                };
                Value::Array(orders.iter(&txn).map(|out| out_json(&out)).collect())
            })
            .collect()
    }

    fn row_doc() -> Doc {
        let doc = Doc::new();
        {
            let root = doc.get_or_insert_map("data");
            let mut txn = doc.transact_mut();
            let row = root.insert(&mut txn, "data", empty_map());
            row.insert(&mut txn, "id", In::from("R"));
            row.insert(&mut txn, "database_id", In::from("DB"));
            row.insert(&mut txn, "last_modified", In::from("100"));
            let cells = row.insert(&mut txn, "cells", empty_map());
            cells.insert(
                &mut txn,
                "S",
                MapPrelim::from([
                    ("data", In::from("b")),
                    ("field_type", any_json(json!(3))),
                    ("last_modified", In::from("100")),
                    ("source_field_type", any_json(json!(0))),
                ]),
            );
            txn.commit();
        }
        doc
    }

    fn db_doc() -> Doc {
        let doc = Doc::new();
        {
            let root = doc.get_or_insert_map("data");
            let mut txn = doc.transact_mut();
            let database = root.insert(&mut txn, "database", empty_map());
            let views = database.insert(&mut txn, "views", empty_map());
            for view_id in ["v1", "v2"] {
                let view = views.insert(&mut txn, view_id, empty_map());
                view.insert(
                    &mut txn,
                    "row_orders",
                    yrs::ArrayPrelim::from([
                        any_json(json!({"id": "A", "height": 36.0})),
                        any_json(json!({"id": "R", "height": 30.0})),
                    ]),
                );
            }
            txn.commit();
        }
        doc
    }

    #[test]
    fn select_by_name_and_unknown_option() {
        assert_eq!(
            encode_cell(&status_field(), &json!("Doing"), None).unwrap()["data"],
            json!("d")
        );
        assert_eq!(
            encode_cell(&multi_field(), &json!(["theme", "features"]), None).unwrap()["data"],
            json!("t,f")
        );
        let err = encode_cell(&status_field(), &json!("Nope"), None).unwrap_err();
        assert!(err.contains("Backlog"), "{err}");
    }

    #[test]
    fn checkbox_and_readonly() {
        let done = field("X", "Done", FIELD_TYPE_CHECKBOX, json!({}));
        assert_eq!(
            encode_cell(&done, &json!(true), None).unwrap()["data"],
            json!("Yes")
        );
        assert_eq!(
            encode_cell(&done, &json!("no"), None).unwrap()["data"],
            json!("No")
        );
        let computed = field("M", "Last modified", 8, json!({}));
        assert!(encode_cell(&computed, &json!("x"), None).is_err());
    }

    #[test]
    fn date_range() {
        let due = field("D", "Due Date", FIELD_TYPE_DATE_TIME, json!({}));
        let cell = encode_cell(
            &due,
            &json!({"start": "2026-10-01", "end": "2026-10-03"}),
            None,
        )
        .unwrap();
        assert_eq!(cell["data"], json!("1790812800"));
        assert_eq!(cell["end_timestamp"], json!("1790985600"));
        assert_eq!(cell["is_range"], json!(true));
        assert_eq!(cell["include_time"], json!(false));
    }

    #[test]
    fn checklist_toggle_keeps_ids_and_rejects_unknown() {
        let tasks = field("C", "Tasks", FIELD_TYPE_CHECKLIST, json!({}));
        let current = json!({
            "options": [{"id": "a1", "name": "one", "color": "Purple"},
                        {"id": "b2", "name": "two", "color": "Purple"}],
            "selected_option_ids": ["a1"],
        })
        .to_string();
        let entries = encode_cell(
            &tasks,
            &json!({"check": ["two"], "uncheck": ["one"]}),
            Some(&current),
        )
        .unwrap();
        let out: Value = serde_json::from_str(entries["data"].as_str().unwrap()).unwrap();
        let ids: Vec<&str> = out["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["a1", "b2"]);
        assert_eq!(out["selected_option_ids"], json!(["b2"]));
        let err = encode_cell(&tasks, &json!({"check": ["three"]}), Some(&current)).unwrap_err();
        assert!(err.contains("three"), "{err}");
    }

    #[test]
    fn checklist_replace_generates_ids() {
        let tasks = field("C", "Tasks", FIELD_TYPE_CHECKLIST, json!({}));
        let entries =
            encode_cell(&tasks, &json!(["x", {"name": "y", "done": true}]), None).unwrap();
        let out: Value = serde_json::from_str(entries["data"].as_str().unwrap()).unwrap();
        let options = out["options"].as_array().unwrap();
        assert!(options.iter().all(|o| o["id"].as_str().unwrap().len() == 6));
        assert_eq!(out["selected_option_ids"], json!([options[1]["id"]]));
    }

    #[test]
    fn iso_timestamp_parse() {
        assert_eq!(parse_iso_timestamp("2026-10-01"), Some(1_790_812_800));
        assert_eq!(
            parse_iso_timestamp("2026-10-01T00:00:00Z"),
            Some(1_790_812_800)
        );
        assert_eq!(
            parse_iso_timestamp("2026-10-01T08:00:00+08:00"),
            Some(1_790_812_800)
        );
        assert_eq!(parse_iso_timestamp("2026-10-01 02:30"), Some(1_790_821_800));
        assert_eq!(parse_iso_timestamp("not a date"), None);
        assert_eq!(parse_iso_timestamp("2026/10/01"), None);
        assert_eq!(parse_iso_timestamp("2026-13-01"), None);
    }

    #[test]
    fn mutate_cells_edits_existing_and_adds_cells() {
        let base = row_doc();
        let doc = clone_doc(&base);
        let staged = vec![
            StagedCell {
                field: status_field(),
                entries: encode_cell(&status_field(), &json!("Doing"), None).unwrap(),
            },
            StagedCell {
                field: field("T", "Description", FIELD_TYPE_RICH_TEXT, json!({})),
                entries: encode_cell(
                    &field("T", "Description", FIELD_TYPE_RICH_TEXT, json!({})),
                    &json!("hi"),
                    None,
                )
                .unwrap(),
            },
        ];
        let since = doc.transact().state_vector();
        let updated = mutate_row_cells(&doc, "DB", &staged, "R").unwrap();
        assert_eq!(updated, ["Status", "Description"]);

        // The diff carries the mutation to a fresh copy of the pre-edit doc.
        let diff = doc.transact().encode_state_as_update_v1(&since);
        assert!(!diff.is_empty());
        base.transact_mut()
            .apply_update(Update::decode_v1(&diff).unwrap())
            .unwrap();

        let status = read_cell(&base, "S");
        assert_eq!(status["data"], json!("d"));
        assert!(!status.contains_key("source_field_type"));
        let description = read_cell(&base, "T");
        assert_eq!(description["data"], json!("hi"));
        assert_eq!(description["field_type"], json!(0));
        assert!(description.contains_key("created_at"));
        // String timestamps stay strings and move forward from "100".
        let modified = read_row_key(&base, "last_modified");
        assert!(
            modified.is_string() && modified != json!("100"),
            "{modified}"
        );
    }

    #[test]
    fn mutate_rejects_a_foreign_database() {
        let doc = row_doc();
        let staged = vec![StagedCell {
            field: status_field(),
            entries: encode_cell(&status_field(), &json!("Doing"), None).unwrap(),
        }];
        let err = mutate_row_cells(&doc, "OTHER", &staged, "R").unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "{err:?}");
    }

    #[test]
    fn delete_and_restore_row_in_every_view() {
        let base = db_doc();
        let doc = clone_doc(&base);

        let since = doc.transact().state_vector();
        assert_eq!(set_collab_row_deleted(&doc, "R", true, "DB").unwrap(), 2);
        let diff = doc.transact().encode_state_as_update_v1(&since);
        base.transact_mut()
            .apply_update(Update::decode_v1(&diff).unwrap())
            .unwrap();
        for orders in row_orders_json(&base) {
            // Any::Number serializes losslessly as an integer in JSON.
            assert_eq!(
                orders,
                json!([
                    {"id": "A", "height": 36},
                    {"id": "R", "height": 30, "is_deleted": true},
                ])
            );
        }

        let since = doc.transact().state_vector();
        assert_eq!(set_collab_row_deleted(&doc, "R", false, "DB").unwrap(), 2);
        let diff = doc.transact().encode_state_as_update_v1(&since);
        base.transact_mut()
            .apply_update(Update::decode_v1(&diff).unwrap())
            .unwrap();
        for orders in row_orders_json(&base) {
            assert_eq!(
                orders,
                json!([{"id": "A", "height": 36}, {"id": "R", "height": 30}])
            );
        }

        let err = set_collab_row_deleted(&doc, "missing", true, "DB").unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "{err:?}");
    }
}

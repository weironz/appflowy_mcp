//! Page-core tools (`server.py` L388-671 and L1316-1391).

use std::collections::HashMap;

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::server::AppServer;
use crate::tools::export::fetch_collab_markdown;
use crate::tools::util::{ensure_parent_is_not_workspace, map_tool_error, ToolError};

const SORT_BY_FIELDS: [&str; 3] = ["name", "created_at", "last_edited_time"];

/// Python `Field(..., min_length=1)` on strings, checked after trimming —
/// identical semantics to content.rs's helper of the same name, so id
/// validation behaves the same across the page modules. Guards against
/// empty ids reaching the API as URLs like /api/workspace//page-view/,
/// which fail with confusing server errors.
fn require_non_empty(value: &str, field: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(ToolError::validation(format!("{field} must not be empty")));
    }
    Ok(())
}

/// Mirrors Python `models.py::CreatePageRequest` (plus the path-level
/// `workspace_id` the FastMCP signature adds).
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct CreatePageParams {
    pub workspace_id: String,
    /// view_id of the parent Space or existing Page to create under. Must NOT
    /// be the workspace_id: AppFlowy's hierarchy is workspace -> space ->
    /// page, and a view created directly under the workspace root becomes a
    /// Space, not a document. Call appflowy_list_spaces to pick a space (or
    /// appflowy_create_space to make one) and pass its view_id here.
    pub parent_view_id: String,
    pub name: Option<String>,
    /// Page layout: 0=Document, 1=Grid, 2=Board, 3=Calendar, 4=Chat.
    pub layout: Option<i64>,
    pub page_data: Option<Value>,
    pub view_id: Option<String>,
    pub collab_id: Option<String>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct GetPageParams {
    pub workspace_id: String,
    pub page_id: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct GetCollabParams {
    pub workspace_id: String,
    pub page_id: String,
    pub title: Option<String>,
}

/// Mirrors Python `models.py::UpdatePageRequest`. `name` is required, like
/// the Python model.
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct UpdatePageParams {
    pub workspace_id: String,
    pub page_id: String,
    pub name: String,
    pub icon: Option<Value>,
    pub is_locked: Option<bool>,
    pub extra: Option<Value>,
}

/// Mirrors Python `models.py::MovePageRequest`.
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct MovePageParams {
    pub workspace_id: String,
    pub page_id: String,
    /// view_id of the destination parent Space or Page. To REORDER a page in
    /// place, pass its current parent_view_id here. Must NOT be the
    /// workspace_id (a child of the workspace root becomes a Space).
    pub new_parent_view_id: String,
    /// Ordering: view_id of the sibling to place this page immediately AFTER.
    /// Omit / null to make it the first child. This is how pages are
    /// reordered among their siblings.
    pub prev_view_id: Option<String>,
}

/// Mirrors Python `models.py::ReorderPagesRequest`.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ReorderPagesParams {
    pub workspace_id: String,
    /// view_id of the Space or Page whose direct children should be
    /// reordered. Must NOT be the workspace_id.
    pub parent_view_id: String,
    /// Explicit desired order of child view_ids. Listed pages are placed
    /// first in this order; unlisted children keep their current relative
    /// order after them. Mutually exclusive with sort_by.
    pub page_ids: Option<Vec<String>>,
    /// Sort the children by this field instead of giving an explicit
    /// page_ids order. One of: name, created_at, last_edited_time.
    /// Mutually exclusive with page_ids.
    pub sort_by: Option<String>,
    /// Reverse the sort_by order. Ignored with page_ids.
    pub descending: bool,
}

/// Shared `workspace_id` + `page_id` parameter shape.
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct PageRefParams {
    pub workspace_id: String,
    pub page_id: String,
}

/// Mirrors Python `models.py::FavoritePageRequest`.
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct FavoritePageParams {
    pub workspace_id: String,
    pub page_id: String,
    #[serde(default = "default_true")]
    pub is_favorite: bool,
    #[serde(default)]
    pub is_pinned: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct ReorderFavoriteParams {
    pub workspace_id: String,
    pub page_id: String,
    pub prev_view_id: Option<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct WorkspaceUnitsParams {
    pub workspace_id: String,
}

/// Mirrors Python `models.py::DuplicatePageRequest`.
#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct DuplicatePageParams {
    pub workspace_id: String,
    pub page_id: String,
    /// Name suffix for the copy, e.g. " (Copy)".
    pub suffix: Option<String>,
}

/// Python truthiness for JSON values (the `or` chains in
/// `_encoded_collab_bytes` / `fetch_collab_markdown` rely on it).
fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Bool(true) => true,
        Value::Number(n) => {
            n.as_i64() != Some(0) && n.as_u64() != Some(0) && n.as_f64() != Some(0.0)
        }
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Sort key for `reorder_pages`'s `sort_by` mode. Python compares the raw
/// values (or `""` / casefolded name); a missing value sorts as the empty
/// string. Numbers compare numerically here; Python would raise on a mixed
/// int/str comparison, which we instead order deterministically.
#[derive(Debug, PartialEq)]
enum SortKey {
    Empty,
    Num(f64),
    Str(String),
}

impl SortKey {
    fn from(value: Option<&Value>, field: &str) -> Self {
        match value.filter(|v| json_truthy(v)) {
            None => SortKey::Empty,
            Some(v) if field == "name" => {
                // Python: `(value or "").casefold()`.
                SortKey::Str(v.as_str().unwrap_or_default().to_lowercase())
            }
            Some(Value::Number(n)) => SortKey::Num(n.as_f64().unwrap_or(0.0)),
            Some(Value::String(s)) => SortKey::Str(s.clone()),
            Some(_) => SortKey::Empty,
        }
    }
}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let rank = |k: &SortKey| match k {
            SortKey::Empty => 0,
            SortKey::Str(_) => 1,
            SortKey::Num(_) => 2,
        };
        match (self, other) {
            (SortKey::Num(a), SortKey::Num(b)) => a.total_cmp(b),
            (SortKey::Str(a), SortKey::Str(b)) => a.cmp(b),
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for SortKey {}

/// Target indices whose pages need a real move request, in call order.
///
/// Python's loop tested `current[index] == page_id` and then did
/// `current.remove(page_id); current.insert(index, page_id)` — O(n^2) list
/// surgery. The linked list below keeps only the not-yet-placed suffix and
/// models the same evolving order: the page at absolute position `index` is
/// exactly the head of that suffix, because every position before it
/// already holds target's placed prefix.
///
/// Per iteration:
/// - head == page: already at `index`; unlink it — it joins the placed
///   prefix and the next suffix page becomes the head at `index+1`.
/// - otherwise: a move is needed. The POST itself places the page at
///   `index`, so after unlinking it the OLD head is the page at `index+1` —
///   no re-insertion. (Re-inserting the moved page as the new head would
///   point `head` at an already-placed page, so every later page would also
///   look displaced and get a redundant move: an adjacent swap cost 3
///   writes instead of 1.)
///
/// Each page is unlinked at most once, so the simulation stays O(n).
fn reorder_move_positions(current: &[String], target: &[String]) -> Vec<usize> {
    let in_children: HashMap<&str, usize> = current
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    // Slots index into `current` (target reorders the same ids), so size
    // the list state by `current`, not `target`.
    let n = current.len();
    let mut prev_slot: Vec<Option<usize>> = (0..n).map(|i| i.checked_sub(1)).collect();
    let mut next_slot: Vec<Option<usize>> = (0..n).map(|i| (i + 1 < n).then_some(i + 1)).collect();
    let mut in_list: Vec<bool> = vec![true; n];
    let mut head: Option<usize> = (!current.is_empty()).then_some(0);
    let unlink = |slot: usize,
                  head: &mut Option<usize>,
                  prev_slot: &mut [Option<usize>],
                  next_slot: &mut [Option<usize>],
                  in_list: &mut [bool]| {
        if !in_list[slot] {
            return;
        }
        in_list[slot] = false;
        let (prev, next) = (prev_slot[slot], next_slot[slot]);
        if let Some(prev) = prev {
            next_slot[prev] = next;
        } else {
            *head = next;
        }
        if let Some(next) = next {
            prev_slot[next] = prev;
        }
        prev_slot[slot] = None;
        next_slot[slot] = None;
    };
    let mut moves = Vec::new();
    for (index, page_id) in target.iter().enumerate() {
        let Some(&slot) = in_children.get(page_id.as_str()) else {
            // Defensive: `target` only ever holds known children.
            continue;
        };
        let already_in_place = in_list[slot] && head == Some(slot);
        // Both branches unlink; only a displaced page needs a real move.
        unlink(
            slot,
            &mut head,
            &mut prev_slot,
            &mut next_slot,
            &mut in_list,
        );
        if !already_in_place {
            moves.push(index);
        }
    }
    moves
}

#[tool_router(router = pages_tool_router, vis = "pub(crate)")]
impl AppServer {
    /// Return `json!` shape shared by the thin proxy tools.
    #[tool(
        name = "appflowy_create_page",
        description = "Create a page under a space or parent page.",
        annotations(title = "Create page", open_world_hint = true)
    )]
    async fn appflowy_create_page(
        &self,
        Parameters(p): Parameters<CreatePageParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        require_non_empty(&p.parent_view_id, "parent_view_id")?;
        ensure_parent_is_not_workspace(&p.workspace_id, &p.parent_view_id)
            .map_err(map_tool_error)?;
        if let Some(layout) = p.layout {
            if !(0..=4).contains(&layout) {
                return Err(ToolError::validation(
                    "layout must be between 0 and 4 (Field ge=0, le=4)",
                ));
            }
        }
        self.client
            .request(
                Method::POST,
                &format!("/api/workspace/{}/page-view", p.workspace_id),
                None,
                Some(json!({
                    "parent_view_id": p.parent_view_id,
                    "name": p.name,
                    "layout": p.layout.unwrap_or(0),
                    "page_data": p.page_data,
                    "view_id": p.view_id,
                    "collab_id": p.collab_id,
                })),
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_get_page",
        description = "Get page view details and collab data.",
        annotations(
            title = "Get page",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_get_page(
        &self,
        Parameters(p): Parameters<GetPageParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        require_non_empty(&p.page_id, "page_id")?;
        self.client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/page-view/{}", p.workspace_id, p.page_id),
                None,
                None,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_get_collab",
        description = "Get a document's content as Markdown via the raw collab endpoint. This reads collab storage directly, bypassing the lazy page-view path, so it can recover pages that appflowy_get_page / export report as 'Collab not found' or return empty.",
        annotations(
            title = "Get collab",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_get_collab(
        &self,
        Parameters(p): Parameters<GetCollabParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        require_non_empty(&p.page_id, "page_id")?;
        let (markdown, attempts) = fetch_collab_markdown(
            &self.client,
            &p.workspace_id,
            &p.page_id,
            p.title.as_deref(),
        )
        .await;
        match markdown {
            Some(markdown) => Ok(Json(
                json!({ "object_id": p.page_id, "markdown": markdown }),
            )),
            // The attempts trail carries the real per-try causes (network,
            // auth, decode); a bare "no document collab" hides them.
            None => Err(ToolError::internal(format!(
                "Could not fetch collab content for this object (no document collab). \
                 Attempts: {}",
                attempts.join(" | ")
            ))),
        }
    }

    #[tool(
        name = "appflowy_update_page",
        description = "Update a page name, icon, lock state, or extra metadata.",
        annotations(title = "Update page", open_world_hint = true)
    )]
    async fn appflowy_update_page(
        &self,
        Parameters(p): Parameters<UpdatePageParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        require_non_empty(&p.page_id, "page_id")?;
        self.client
            .request(
                Method::PATCH,
                &format!("/api/workspace/{}/page-view/{}", p.workspace_id, p.page_id),
                None,
                Some(json!({
                    "name": p.name,
                    "icon": p.icon,
                    "is_locked": p.is_locked,
                    "extra": p.extra,
                })),
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_move_page",
        description = "Move a page to a new parent and/or reorder it among its siblings. Set new_parent_view_id to the destination parent (pass the page's current parent to reorder in place) and prev_view_id to the sibling to place it after (omit to put it first).",
        annotations(title = "Move page", open_world_hint = true)
    )]
    async fn appflowy_move_page(
        &self,
        Parameters(p): Parameters<MovePageParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        require_non_empty(&p.page_id, "page_id")?;
        require_non_empty(&p.new_parent_view_id, "new_parent_view_id")?;
        ensure_parent_is_not_workspace(&p.workspace_id, &p.new_parent_view_id)
            .map_err(map_tool_error)?;
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/page-view/{}/move",
                    p.workspace_id, p.page_id
                ),
                None,
                Some(json!({
                    "new_parent_view_id": p.new_parent_view_id,
                    "prev_view_id": p.prev_view_id,
                })),
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_reorder_pages",
        description = "Reorder the direct child pages of a space or page in one call. Give either an explicit page_ids order or sort_by name/created_at/last_edited_time. AppFlowy has no bulk-sort endpoint, so this chains the per-page move endpoint, skipping pages already in place.",
        annotations(title = "Reorder pages", open_world_hint = true)
    )]
    async fn appflowy_reorder_pages(
        &self,
        Parameters(p): Parameters<ReorderPagesParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        require_non_empty(&p.parent_view_id, "parent_view_id")?;
        ensure_parent_is_not_workspace(&p.workspace_id, &p.parent_view_id)
            .map_err(map_tool_error)?;
        if p.page_ids.is_none() == p.sort_by.is_none() {
            return Err(ToolError::validation(
                "Provide exactly one of page_ids or sort_by.",
            ));
        }
        if let Some(sort_by) = &p.sort_by {
            if !SORT_BY_FIELDS.contains(&sort_by.as_str()) {
                return Err(ToolError::validation(format!(
                    "sort_by must be one of {:?}",
                    SORT_BY_FIELDS.map(str::to_owned)
                )));
            }
        }

        let params = [
            ("depth", "1".to_string()),
            ("root_view_id", p.parent_view_id.clone()),
        ];
        let data = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/folder", p.workspace_id),
                Some(&params),
                None,
            )
            .await
            .map_err(map_tool_error)?;
        let children: Vec<&Value> = data
            .get("children")
            .and_then(Value::as_array)
            .map(|children| children.iter().collect())
            .unwrap_or_default();
        let current: Vec<String> = children
            .iter()
            .map(|child| {
                child
                    .get("view_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        let names: HashMap<&str, Value> = children
            .iter()
            .map(|child| {
                (
                    child.get("view_id").and_then(Value::as_str).unwrap_or(""),
                    child.get("name").cloned().unwrap_or(Value::Null),
                )
            })
            .collect();
        // One O(n) membership index; Python rescanned `current` per id.
        let in_children: HashMap<&str, usize> = current
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();

        let target: Vec<String> = match &p.page_ids {
            Some(page_ids) => {
                let unknown: Vec<&str> = page_ids
                    .iter()
                    .map(String::as_str)
                    .filter(|pid| !in_children.contains_key(pid))
                    .collect();
                if !unknown.is_empty() {
                    let rendered = unknown
                        .iter()
                        .map(|pid| format!("'{pid}'"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(ToolError::validation(format!(
                        "Not direct children of {}: [{rendered}]",
                        p.parent_view_id
                    )));
                }
                let mut target = page_ids.clone();
                target.extend(current.iter().filter(|id| !page_ids.contains(id)).cloned());
                target
            }
            None => {
                let sort_by = p.sort_by.as_deref().unwrap_or_default();
                let mut keyed: Vec<(&Value, SortKey)> = children
                    .iter()
                    .map(|child| (*child, SortKey::from(child.get(sort_by), sort_by)))
                    .collect();
                // Python `sorted(key=..., reverse=...)`: stable, comparison
                // reversed — not ascend-and-flip (equal keys keep order).
                keyed.sort_by(|a, b| {
                    if p.descending {
                        b.1.cmp(&a.1)
                    } else {
                        a.1.cmp(&b.1)
                    }
                });
                keyed
                    .into_iter()
                    .map(|(child, _)| {
                        child
                            .get("view_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    })
                    .collect()
            }
        };

        // Which target positions need a real move is a pure function of the
        // current and target orders; compute it up front (see
        // `reorder_move_positions` for the linked-list invariant).
        let move_positions = reorder_move_positions(&current, &target);
        for &index in &move_positions {
            // Sequential on purpose: each move re-reads the order produced
            // by the previous one (AppFlowy has no bulk-sort endpoint).
            self.client
                .request(
                    Method::POST,
                    &format!(
                        "/api/workspace/{}/page-view/{}/move",
                        p.workspace_id, target[index]
                    ),
                    None,
                    Some(json!({
                        "new_parent_view_id": p.parent_view_id,
                        "prev_view_id": if index > 0 { Some(&target[index - 1]) } else { None },
                    })),
                )
                .await
                .map_err(map_tool_error)?;
        }

        Ok(Json(json!({
            "parent_view_id": p.parent_view_id,
            "order": target.iter().map(|pid| json!({
                "view_id": pid,
                "name": names.get(pid.as_str()).cloned().unwrap_or(Value::Null),
            })).collect::<Vec<_>>(),
            "moves_performed": move_positions.len(),
        })))
    }

    #[tool(
        name = "appflowy_move_page_to_trash",
        description = "Move a page to trash.",
        annotations(
            title = "Move page to trash",
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_move_page_to_trash(
        &self,
        Parameters(p): Parameters<PageRefParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        require_non_empty(&p.page_id, "page_id")?;
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/page-view/{}/move-to-trash",
                    p.workspace_id, p.page_id
                ),
                None,
                None,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_restore_page_from_trash",
        description = "Restore a page from trash.",
        annotations(
            title = "Restore page from trash",
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_restore_page_from_trash(
        &self,
        Parameters(p): Parameters<PageRefParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        require_non_empty(&p.page_id, "page_id")?;
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/page-view/{}/restore-from-trash",
                    p.workspace_id, p.page_id
                ),
                None,
                None,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_delete_page_from_trash",
        description = "Permanently delete a trashed page.",
        annotations(
            title = "Delete page from trash",
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_delete_page_from_trash(
        &self,
        Parameters(p): Parameters<PageRefParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        require_non_empty(&p.page_id, "page_id")?;
        self.client
            .request(
                Method::DELETE,
                &format!("/api/workspace/{}/trash/{}", p.workspace_id, p.page_id),
                None,
                None,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_favorite_page",
        description = "Favorite, unfavorite, pin, or unpin a page.",
        annotations(title = "Favorite page", open_world_hint = true)
    )]
    async fn appflowy_favorite_page(
        &self,
        Parameters(p): Parameters<FavoritePageParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        require_non_empty(&p.page_id, "page_id")?;
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/page-view/{}/favorite",
                    p.workspace_id, p.page_id
                ),
                None,
                Some(json!({
                    "is_favorite": p.is_favorite,
                    "is_pinned": p.is_pinned,
                })),
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_reorder_favorite",
        description = "Reorder a page within the favorites list. prev_view_id is the favorite to place it after (omit to put it first).",
        annotations(title = "Reorder favorite", open_world_hint = true)
    )]
    async fn appflowy_reorder_favorite(
        &self,
        Parameters(p): Parameters<ReorderFavoriteParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        require_non_empty(&p.page_id, "page_id")?;
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/page-view/{}/reorder-favorite",
                    p.workspace_id, p.page_id
                ),
                None,
                Some(json!({ "prev_view_id": p.prev_view_id })),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(
            json!({ "page_id": p.page_id, "prev_view_id": p.prev_view_id }),
        ))
    }

    #[tool(
        name = "appflowy_list_trash",
        description = "List trashed pages in a workspace.",
        annotations(
            title = "List trash",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_list_trash(
        &self,
        Parameters(p): Parameters<WorkspaceUnitsParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        self.client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/trash", p.workspace_id),
                None,
                None,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_list_favorites",
        description = "List favorite pages in a workspace.",
        annotations(
            title = "List favorites",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_list_favorites(
        &self,
        Parameters(p): Parameters<WorkspaceUnitsParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        self.client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/favorite", p.workspace_id),
                None,
                None,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_duplicate_page",
        description = "Duplicate a page and all of its child pages.",
        annotations(title = "Duplicate page", open_world_hint = true)
    )]
    async fn appflowy_duplicate_page(
        &self,
        Parameters(p): Parameters<DuplicatePageParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        require_non_empty(&p.page_id, "page_id")?;
        // DuplicatePageRequest only carries an optional name `suffix` — the
        // endpoint duplicates in place under the same parent, so there is no
        // caller-supplied parent to guard with ensure_parent_is_not_workspace.
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/page-view/{}/duplicate",
                    p.workspace_id, p.page_id
                ),
                None,
                Some(json!({ "suffix": p.suffix })),
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_list_recent",
        description = "List recently viewed pages in a workspace.",
        annotations(
            title = "List recent",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_list_recent(
        &self,
        Parameters(p): Parameters<WorkspaceUnitsParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        self.client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/recent", p.workspace_id),
                None,
                None,
            )
            .await
            .map(Json)
            .map_err(map_tool_error)
    }

    #[tool(
        name = "appflowy_restore_all_from_trash",
        description = "Restore all trashed pages in a workspace.",
        annotations(
            title = "Restore all from trash",
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn appflowy_restore_all_from_trash(
        &self,
        Parameters(p): Parameters<WorkspaceUnitsParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/restore-all-pages-from-trash",
                    p.workspace_id
                ),
                None,
                Some(json!({})),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "restored_all" })))
    }

    #[tool(
        name = "appflowy_empty_trash",
        description = "Permanently delete ALL trashed pages in a workspace. Irreversible.",
        annotations(title = "Empty trash", destructive_hint = true, open_world_hint = true)
    )]
    async fn appflowy_empty_trash(
        &self,
        Parameters(p): Parameters<WorkspaceUnitsParams>,
    ) -> Result<Json<Value>, ToolError> {
        require_non_empty(&p.workspace_id, "workspace_id")?;
        self.client
            .request(
                Method::POST,
                &format!(
                    "/api/workspace/{}/delete-all-pages-from-trash",
                    p.workspace_id
                ),
                None,
                Some(json!({})),
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({ "status": "emptied" })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_keys_order_like_python() {
        // name: casefolded; missing value behaves like "".
        assert_eq!(
            SortKey::from(Some(&json!("Banana")), "name"),
            SortKey::from(Some(&json!("banana")), "name")
        );
        assert!(SortKey::from(None, "name") < SortKey::from(Some(&json!("a")), "name"));
        assert!(
            SortKey::from(Some(&json!(10)), "created_at")
                < SortKey::from(Some(&json!(20)), "created_at")
        );
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn reorder_already_sorted_needs_no_moves() {
        let current = ids(&["a", "b", "c"]);
        assert_eq!(
            reorder_move_positions(&current, &current),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn reorder_adjacent_swap_needs_one_move() {
        // Regression: the old re-insert-as-head simulation issued 3 moves
        // here; Python's list semantics (and reality) need exactly 1 —
        // moving b to the front puts [a, c] after it in the right order.
        let current = ids(&["a", "b", "c"]);
        let target = ids(&["b", "a", "c"]);
        assert_eq!(reorder_move_positions(&current, &target), vec![0]);
    }

    #[test]
    fn reorder_full_reverse_needs_all_but_last_moves() {
        // Python's algorithm: each of the first n-1 pages moves to the
        // front; the last one is then already in place.
        let current = ids(&["a", "b", "c", "d"]);
        let target = ids(&["d", "c", "b", "a"]);
        assert_eq!(reorder_move_positions(&current, &target), vec![0, 1, 2]);
    }

    #[test]
    fn reorder_rotation_moves_each_displaced_page() {
        // Rotating the first page to the end displaces every position, so
        // Python's algorithm moves b, c, d forward one by one (a's single
        // move to the end would also suffice, but that is not what the
        // reference algorithm computes — keep parity with it).
        let current = ids(&["a", "b", "c", "d"]);
        let target = ids(&["b", "c", "d", "a"]);
        assert_eq!(reorder_move_positions(&current, &target), vec![0, 1, 2]);
    }

    #[test]
    fn reorder_moves_only_displaced_positions() {
        // Only the head of the suffix matters: a, b are consumed in place,
        // then the d/e swap costs one move.
        let current = ids(&["a", "b", "c", "d", "e"]);
        let target = ids(&["c", "a", "b", "e", "d"]);
        assert_eq!(reorder_move_positions(&current, &target), vec![0, 3]);
    }

    #[test]
    fn require_non_empty_rejects_blank_ids() {
        // Empty / whitespace-only ids would otherwise hit the API as
        // /api/workspace//page-view// and produce confusing server errors.
        assert!(require_non_empty("", "workspace_id").is_err());
        assert!(require_non_empty("   \t ", "page_id").is_err());
        assert!(require_non_empty("ws-1", "workspace_id").is_ok());
    }
}

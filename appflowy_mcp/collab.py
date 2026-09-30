"""Write database rows through AppFlowy's collab (yjs) layer.

The REST API can create rows but cannot edit or delete them (PUT upserts by
pre_hash into a new, non-v4 row id that never reaches any view). The web client
edits rows by mutating yjs docs and syncing the update; the server also accepts
such an update over ``POST /api/workspace/v1/{ws}/collab/{id}/web-update``.
These helpers do the same:

    GET collab -> pycrdt Doc -> mutate in a transaction -> diff since the
    fetched state -> POST web-update

Layouts mirrored from the AppFlowy web client:

* row collab (DatabaseRow = 4): ``data.data.cells[field_id] = Map{data,
  field_type, created_at, last_modified, ...}``; cell ``data`` is a string.
* database collab (Database = 1): ``data.database.views[view_id].row_orders``
  is an Array of plain ``{id, height}`` dicts. A deleted row stays in place
  as ``{id, height, is_deleted: true}`` (soft delete, restorable) in every view.
"""

import base64
import json
import secrets
import string
import time
from datetime import date, datetime, timezone
from typing import Any

from pycrdt import Array, Doc, Map

COLLAB_DATABASE = 1
COLLAB_DATABASE_ROW = 4

# FieldType ids (AppFlowy collab-database).
RICH_TEXT = 0
NUMBER = 1
DATE_TIME = 2
SINGLE_SELECT = 3
MULTI_SELECT = 4
CHECKBOX = 5
URL = 6
CHECKLIST = 7
READ_ONLY_TYPES = {8, 9}  # LastEditedTime, CreatedTime

_OPTION_ID_CHARS = string.ascii_letters + string.digits + "_-"


class CollabWriteError(Exception):
    pass


def _doc_state(payload: Any) -> bytes:
    data = payload.get("data", payload) if isinstance(payload, dict) else payload
    if isinstance(data, dict):
        data = (
            data.get("doc_state")
            or (data.get("encode_collab") or {}).get("doc_state")
            or (data.get("encoded_collab") or {}).get("doc_state")
        )
    if isinstance(data, str):
        return base64.b64decode(data)
    if isinstance(data, (list, bytes, bytearray)) and data:
        return bytes(data)
    raise CollabWriteError("collab response carried no doc_state")


def load_collab(client, workspace_id: str, object_id: str, collab_type: int) -> Doc:
    body = client._request(
        "GET",
        f"/api/workspace/v1/{workspace_id}/collab/{object_id}",
        params={"collab_type": collab_type},
    )
    doc = Doc()
    doc.apply_update(_doc_state(body))
    return doc


def push_update(
    client, workspace_id: str, object_id: str, collab_type: int, doc: Doc, since: bytes
) -> int:
    """Send only what changed since state vector ``since``; returns its size."""
    update = doc.get_update(since)
    client._request(
        "POST",
        f"/api/workspace/v1/{workspace_id}/collab/{object_id}/web-update",
        json_body={"doc_state": list(update), "collab_type": collab_type},
    )
    return len(update)


def new_option_id() -> str:
    return "".join(secrets.choice(_OPTION_ID_CHARS) for _ in range(6))


# ==================== cell encoding ====================


def _options(field: dict) -> list[dict]:
    content = (field.get("type_option") or {}).get("content") or {}
    if isinstance(content, str):
        content = json.loads(content or "{}")
    return content.get("options") or []


def _option_id(field: dict, value: str) -> str:
    options = _options(field)
    for opt in options:
        if value in (opt.get("id"), opt.get("name")):
            return opt["id"]
    names = ", ".join(repr(o.get("name")) for o in options)
    raise CollabWriteError(
        f"{field['name']!r} has no option {value!r} (available: {names})"
    )


def _unix(value: Any) -> int:
    if isinstance(value, (int, float)):
        return int(value)
    if isinstance(value, datetime):
        dt = value
    elif isinstance(value, date):
        dt = datetime(value.year, value.month, value.day)
    else:
        dt = datetime.fromisoformat(str(value).replace("Z", "+00:00"))
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return int(dt.timestamp())


def _checklist(value: Any, current: str | None) -> str:
    """Checklist value forms:

    * ``[{"name": "...", "done": bool}, ...]`` or ``["name", ...]`` replaces the
      list (ids of tasks whose name already exists are kept);
    * ``{"check": [...], "uncheck": [...], "add": [...], "remove": [...]}``
      edits the current list by task name.
    """
    cur = json.loads(current) if current else {}
    options = list(cur.get("options") or [])
    selected = list(cur.get("selected_option_ids") or [])
    by_name = {o["name"]: o for o in options}

    def ensure(name: str) -> dict:
        if name not in by_name:
            opt = {"id": new_option_id(), "name": name, "color": "Purple"}
            options.append(opt)
            by_name[name] = opt
        return by_name[name]

    def need(name: str) -> dict:
        if name not in by_name:
            raise CollabWriteError(
                f"no checklist task {name!r} (tasks: {', '.join(map(repr, by_name))})"
            )
        return by_name[name]

    if isinstance(value, dict):
        for name in value.get("add") or []:
            ensure(name)
        for name in value.get("remove") or []:
            opt = need(name)
            options.remove(opt)
            del by_name[name]
            selected = [i for i in selected if i != opt["id"]]
        for name in value.get("check") or []:
            oid = need(name)["id"]
            if oid not in selected:
                selected.append(oid)
        for name in value.get("uncheck") or []:
            oid = need(name)["id"]
            selected = [i for i in selected if i != oid]
    elif isinstance(value, list):
        kept, selected = [], []
        for item in value:
            task = {"name": item} if isinstance(item, str) else dict(item)
            opt = by_name.get(task["name"]) or {
                "id": new_option_id(),
                "name": task["name"],
                "color": "Purple",
            }
            kept.append(opt)
            if task.get("done"):
                selected.append(opt["id"])
        options = kept
    else:
        raise CollabWriteError("checklist value must be a list or a {check,...} dict")

    return json.dumps(
        {"options": options, "selected_option_ids": selected}, ensure_ascii=False
    )


def encode_cell(field: dict, value: Any, current: str | None = None) -> dict[str, Any]:
    """Cell map entries (besides timestamps) for ``value`` in ``field``."""
    ft = int(field["field_type_id"])
    if ft in READ_ONLY_TYPES:
        raise CollabWriteError(f"{field['name']!r} is computed and cannot be set")
    if ft in (RICH_TEXT, NUMBER, URL):
        return {"data": "" if value is None else str(value)}
    if ft == SINGLE_SELECT:
        return {"data": "" if value in (None, "") else _option_id(field, str(value))}
    if ft == MULTI_SELECT:
        values = [value] if isinstance(value, str) else list(value or [])
        return {"data": ",".join(_option_id(field, str(v)) for v in values)}
    if ft == CHECKBOX:
        truthy = value is True or str(value).lower() in ("yes", "true", "1")
        return {"data": "Yes" if truthy else "No"}
    if ft == CHECKLIST:
        return {"data": _checklist(value, current)}
    if ft == DATE_TIME:
        if isinstance(value, dict):
            start, end = value.get("start"), value.get("end")
            include_time = bool(value.get("include_time", False))
        else:
            start, end, include_time = value, None, False
        cell = {
            "data": str(_unix(start)),
            "include_time": include_time,
            "is_range": end is not None,
        }
        if end is not None:
            cell["end_timestamp"] = str(_unix(end))
        return cell
    raise CollabWriteError(
        f"{field['name']!r}: field type {field.get('field_type')} is not supported yet"
    )


def _field_for(fields: list[dict], key: str) -> dict:
    for f in fields:
        if key == f["id"] or key.strip() == f["name"].strip():
            return f
    names = ", ".join(repr(f["name"]) for f in fields)
    raise CollabWriteError(f"no field {key!r} (fields: {names})")


# ==================== row operations ====================


def update_row(
    client, workspace_id: str, database_id: str, row_id: str, cells: dict[str, Any]
) -> dict[str, Any]:
    """Set cells of an existing row; keys are field names (or field ids)."""
    if not cells:
        raise CollabWriteError("cells is empty")
    fields = client.get_database_fields(workspace_id, database_id)
    doc = load_collab(client, workspace_id, row_id, COLLAB_DATABASE_ROW)
    root = doc.get("data", type=Map)
    if "data" not in root:
        raise CollabWriteError(f"row {row_id} has no row data")
    row = root["data"]
    if row.get("database_id") not in (None, database_id):
        raise CollabWriteError(f"row {row_id} belongs to database {row['database_id']}")

    # Keep the timestamp type the row already uses (the web writes strings).
    stamp: Any = int(time.time())
    if isinstance(row.get("last_modified"), str):
        stamp = str(stamp)

    # Encode everything first so a bad value writes nothing.
    staged = []
    for key, value in cells.items():
        field = _field_for(fields, key)
        existing = row["cells"].get(field["id"]) if "cells" in row else None
        current = existing.get("data") if existing is not None else None
        staged.append((field, encode_cell(field, value, current)))

    since = doc.get_state()
    with doc.transaction():
        if "cells" not in row:
            row["cells"] = Map()
        row_cells = row["cells"]
        for field, entries in staged:
            ft = int(field["field_type_id"])
            cell = row_cells.get(field["id"])
            if cell is None:
                row_cells[field["id"]] = Map(
                    {**entries, "field_type": ft, "created_at": stamp, "last_modified": stamp}
                )
                continue
            for k, v in entries.items():
                cell[k] = v
            if "end_timestamp" not in entries and "end_timestamp" in cell and ft == DATE_TIME:
                del cell["end_timestamp"]
            cell["field_type"] = ft
            if "source_field_type" in cell:
                del cell["source_field_type"]
            cell["last_modified"] = stamp
        row["last_modified"] = stamp

    size = push_update(client, workspace_id, row_id, COLLAB_DATABASE_ROW, doc, since)
    return {"row_id": row_id, "updated": [f["name"] for f, _ in staged], "update_bytes": size}


def _set_row_deleted(
    client, workspace_id: str, database_id: str, row_id: str, deleted: bool
) -> dict[str, Any]:
    doc = load_collab(client, workspace_id, database_id, COLLAB_DATABASE)
    database = doc.get("data", type=Map)["database"]
    since = doc.get_state()
    touched = 0
    with doc.transaction():
        for view in database["views"].values():
            orders: Array = view.get("row_orders")
            if orders is None:
                continue
            for i, entry in enumerate(list(orders)):
                entry = dict(entry)
                if entry.get("id") != row_id:
                    continue
                if bool(entry.get("is_deleted")) == deleted:
                    touched += 1
                    continue
                if deleted:
                    entry["is_deleted"] = True
                else:
                    entry.pop("is_deleted", None)
                # row_orders entries are plain values, not maps: replace in place.
                del orders[i]
                orders.insert(i, entry)
                touched += 1
    if not touched:
        raise CollabWriteError(f"row {row_id} is not in any view of {database_id}")
    size = push_update(client, workspace_id, database_id, COLLAB_DATABASE, doc, since)
    return {"row_id": row_id, "views": touched, "deleted": deleted, "update_bytes": size}


def delete_row(client, workspace_id: str, database_id: str, row_id: str) -> dict[str, Any]:
    """Soft-delete a row in every view, the way the web UI does; restorable."""
    return _set_row_deleted(client, workspace_id, database_id, row_id, True)


def restore_row(client, workspace_id: str, database_id: str, row_id: str) -> dict[str, Any]:
    return _set_row_deleted(client, workspace_id, database_id, row_id, False)

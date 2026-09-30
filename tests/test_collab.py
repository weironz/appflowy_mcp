"""Offline tests for row edits through the collab layer."""

import json

import pytest
from pycrdt import Array, Doc, Map

from appflowy_mcp import collab
from appflowy_mcp.collab import CollabWriteError, encode_cell
from appflowy_mcp.server import _server_order

FIELDS = [
    {"id": "T", "name": "Description", "field_type_id": 0, "type_option": {}},
    {
        "id": "S",
        "name": "Status",
        "field_type_id": 3,
        "type_option": {"content": {"options": [
            {"id": "b", "name": "Backlog"}, {"id": "d", "name": "Doing"}]}},
    },
    {
        "id": "K",
        "name": "Kategori ",
        "field_type_id": 4,
        "type_option": {"content": {"options": [
            {"id": "t", "name": "theme"}, {"id": "f", "name": "features"}]}},
    },
    {"id": "C", "name": "Tasks", "field_type_id": 7, "type_option": {}},
    {"id": "D", "name": "Due Date", "field_type_id": 2, "type_option": {}},
    {"id": "X", "name": "Done", "field_type_id": 5, "type_option": {}},
    {"id": "M", "name": "Last modified", "field_type_id": 8, "type_option": {}},
]
F = {f["name"].strip(): f for f in FIELDS}


def test_select_by_name_and_unknown_option():
    assert encode_cell(F["Status"], "Doing") == {"data": "d"}
    assert encode_cell(F["Kategori"], ["theme", "features"]) == {"data": "t,f"}
    with pytest.raises(CollabWriteError, match="Backlog"):
        encode_cell(F["Status"], "Nope")


def test_checkbox_and_readonly():
    assert encode_cell(F["Done"], True) == {"data": "Yes"}
    assert encode_cell(F["Done"], "no") == {"data": "No"}
    with pytest.raises(CollabWriteError):
        encode_cell(F["Last modified"], "x")


def test_date_range():
    cell = encode_cell(F["Due Date"], {"start": "2026-10-01", "end": "2026-10-03"})
    assert cell["data"] == "1790812800" and cell["end_timestamp"] == "1790985600"
    assert cell["is_range"] is True and cell["include_time"] is False


def test_checklist_toggle_keeps_ids_and_rejects_unknown():
    cur = json.dumps({"options": [{"id": "a1", "name": "one", "color": "Purple"},
                                  {"id": "b2", "name": "two", "color": "Purple"}],
                      "selected_option_ids": ["a1"]})
    out = json.loads(encode_cell(F["Tasks"], {"check": ["two"], "uncheck": ["one"]}, cur)["data"])
    assert [o["id"] for o in out["options"]] == ["a1", "b2"]
    assert out["selected_option_ids"] == ["b2"]
    with pytest.raises(CollabWriteError, match="three"):
        encode_cell(F["Tasks"], {"check": ["three"]}, cur)


def test_checklist_replace_generates_ids():
    out = json.loads(encode_cell(F["Tasks"], ["x", {"name": "y", "done": True}])["data"])
    assert all(len(o["id"]) == 6 for o in out["options"])
    assert out["selected_option_ids"] == [out["options"][1]["id"]]


class FakeClient:
    """Serves collabs from pycrdt docs and applies posted web-updates to them."""

    def __init__(self, docs):
        self.docs = docs
        self.posts = []

    def get_database_fields(self, ws, db):
        return FIELDS

    def _request(self, method, path, params=None, json_body=None):
        object_id = path.split("/collab/")[1].split("/")[0]
        if method == "GET":
            return {"data": {"doc_state": list(self.docs[object_id].get_update())}}
        self.posts.append((object_id, json_body["collab_type"]))
        self.docs[object_id].apply_update(bytes(json_body["doc_state"]))
        return {"code": 0}


def row_doc():
    doc = Doc()
    root = doc.get("data", type=Map)
    root["data"] = Map({
        "id": "R", "database_id": "DB", "last_modified": "100",
        "cells": Map({"S": Map({"data": "b", "field_type": 3, "last_modified": "100",
                                "source_field_type": 0})}),
    })
    return doc


def test_update_row_edits_existing_and_adds_cells():
    c = FakeClient({"R": row_doc()})
    out = collab.update_row(c, "WS", "DB", "R", {"Status": "Doing", "Description": "hi"})
    assert out["updated"] == ["Status", "Description"]
    assert c.posts == [("R", 4)]
    row = c.docs["R"].get("data", type=Map)["data"].to_py()
    assert row["cells"]["S"]["data"] == "d" and "source_field_type" not in row["cells"]["S"]
    assert row["cells"]["T"]["data"] == "hi" and row["cells"]["T"]["field_type"] == 0
    assert isinstance(row["last_modified"], str) and row["last_modified"] != "100"


def test_update_row_bad_value_writes_nothing():
    c = FakeClient({"R": row_doc()})
    with pytest.raises(CollabWriteError):
        collab.update_row(c, "WS", "DB", "R", {"Description": "ok", "Status": "Nope"})
    assert c.posts == []


def db_doc():
    doc = Doc()
    views = Map({
        v: Map({"row_orders": Array([{"id": "A", "height": 36.0}, {"id": "R", "height": 30.0}])})
        for v in ("v1", "v2")
    })
    doc.get("data", type=Map)["database"] = Map({"views": views})
    return doc


def test_delete_and_restore_row_in_every_view():
    c = FakeClient({"DB": db_doc()})
    assert collab.delete_row(c, "WS", "DB", "R")["views"] == 2
    views = c.docs["DB"].get("data", type=Map)["database"]["views"].to_py()
    for v in views.values():
        assert v["row_orders"] == [{"id": "A", "height": 36.0},
                                   {"id": "R", "height": 30.0, "is_deleted": True}]
    collab.restore_row(c, "WS", "DB", "R")
    views = c.docs["DB"].get("data", type=Map)["database"]["views"].to_py()
    assert all(v["row_orders"][1] == {"id": "R", "height": 30.0} for v in views.values())
    with pytest.raises(CollabWriteError):
        collab.delete_row(c, "WS", "DB", "missing")


def test_server_order_reverses_children_recursively():
    block = {"type": "t", "children": [{"id": 1, "children": [{"id": "a"}, {"id": "b"}]}, {"id": 2}]}
    assert _server_order(block) == {"type": "t", "children": [
        {"id": 2}, {"id": 1, "children": [{"id": "b"}, {"id": "a"}]}]}
    assert block["children"][0]["id"] == 1  # input untouched

import pytest

from appflowy_mcp import server
from appflowy_mcp.models import AppendMarkdownRequest, RowCreateRequest


ROW_ID = "c89a178d-59a8-4331-90d9-f5198cfba937"
ROW_DOCUMENT_ID = "9f592119-0028-502d-86fb-91279cc9b3d2"


def test_database_row_document_id_matches_appflowy_uuid5_scheme():
    assert server.database_row_document_id(ROW_ID) == ROW_DOCUMENT_ID


def test_database_row_document_id_rejects_non_uuid():
    with pytest.raises(ValueError, match="valid UUID"):
        server.database_row_document_id("not-a-row-id")


def test_create_row_with_markdown_returns_document_id(monkeypatch):
    monkeypatch.setattr(server, "ensure_authenticated", lambda: None)
    monkeypatch.setattr(
        server.client,
        "create_database_row",
        lambda *args, **kwargs: ROW_ID,
    )

    result = server.appflowy_create_row(
        "ws",
        "db",
        RowCreateRequest(cells={"Name": "Meeting"}, document="# Notes\n\nBody"),
    )

    assert result["id"] == ROW_ID
    assert result["document_id"] == ROW_DOCUMENT_ID
    assert result["document_initialized"] is True


def test_append_markdown_to_row_targets_document_id(monkeypatch):
    calls = []
    monkeypatch.setattr(server, "ensure_authenticated", lambda: None)
    monkeypatch.setattr(
        server.client,
        "get_database_row_details",
        lambda *args, **kwargs: [{"id": ROW_ID, "has_doc": True}],
    )

    def fake_request(method, path, json_body=None):
        calls.append((method, path, json_body))
        return {"code": 0, "data": None}

    monkeypatch.setattr(server.client, "_request", fake_request)

    result = server.appflowy_append_markdown_to_row(
        "ws",
        "db",
        ROW_ID,
        AppendMarkdownRequest(content="## Decisions\n\n- Approved"),
    )

    assert calls[0][1] == (
        f"/api/workspace/ws/page-view/{ROW_DOCUMENT_ID}/append-block"
    )
    assert result["document_id"] == ROW_DOCUMENT_ID
    assert result["block_count"] == 2


def test_append_refuses_uninitialized_row_document(monkeypatch):
    monkeypatch.setattr(server, "ensure_authenticated", lambda: None)
    monkeypatch.setattr(
        server.client,
        "get_database_row_details",
        lambda *args, **kwargs: [{"id": ROW_ID, "has_doc": False}],
    )

    with pytest.raises(Exception, match="created without a document"):
        server.appflowy_append_markdown_to_row(
            "ws",
            "db",
            ROW_ID,
            AppendMarkdownRequest(content="Body"),
        )

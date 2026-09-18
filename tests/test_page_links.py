from unittest.mock import MagicMock

from appflowy_mcp.models import AppendPageLinkRequest
from appflowy_mcp import server


def test_append_page_link_builds_native_linked_page_block(monkeypatch):
    mock_client = MagicMock()

    mock_client._request.return_value = {
        "data": {
            "ok": True
        }
    }

    monkeypatch.setattr(server, "client", mock_client)
    monkeypatch.setattr(server, "ensure_authenticated", lambda: None)

    request = AppendPageLinkRequest(
        target_page_id="target-page-123"
    )

    result = server.appflowy_append_page_link(
        workspace_id="workspace-123",
        page_id="parent-page-123",
        request=request,
    )

    mock_client._request.assert_called_once_with(
        "POST",
        "/api/workspace/workspace-123/page-view/parent-page-123/append-block",
        json_body={
            "blocks": [
                {
                    "type": "linked_page",
                    "data": {
                        "view_id": "target-page-123"
                    },
                }
            ]
        },
    )

    assert result["page_id"] == "parent-page-123"
    assert result["target_page_id"] == "target-page-123"
    assert result["block_type"] == "linked_page"
    assert result["result"] == {"ok": True}

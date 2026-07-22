"""Offline tests for AppFlowy document -> Markdown export."""

from appflowy_mcp.export import (
    delta_to_markdown,
    document_to_markdown,
    sanitize_filename,
)
from appflowy_mcp.markdown import parse_markdown_to_blocks


def make_document(block_specs):
    """Build a decoded-document dict from (ty, data, text_delta, children) specs."""
    blocks = {"root": {"ty": "page", "data": {}, "children": "c_root",
                       "external_id": None}}
    children_map = {"c_root": []}
    text_map = {}

    def add(spec, parent_children):
        ty, data, delta, kids = spec
        bid = f"b{len(blocks)}"
        blocks[bid] = {"ty": ty, "data": data, "children": f"c_{bid}",
                       "external_id": f"t_{bid}" if delta is not None else None}
        children_map[f"c_{bid}"] = []
        if delta is not None:
            text_map[f"t_{bid}"] = delta
        children_map[parent_children].append(bid)
        for kid in kids:
            add(kid, f"c_{bid}")

    for spec in block_specs:
        add(spec, "c_root")
    return {"page_id": "root", "blocks": blocks,
            "children_map": children_map, "text_map": text_map}


def test_delta_inline_formatting():
    delta = [
        {"insert": "a "},
        {"insert": "b", "attributes": {"bold": True}},
        {"insert": " "},
        {"insert": "c", "attributes": {"italic": True}},
        {"insert": " "},
        {"insert": "d", "attributes": {"code": True}},
        {"insert": " "},
        {"insert": "e", "attributes": {"strikethrough": True}},
        {"insert": " "},
        {"insert": "f", "attributes": {"href": "https://x.example"}},
    ]
    assert delta_to_markdown(delta) == "a **b** *c* `d` ~~e~~ [f](https://x.example)"


def test_document_to_markdown_block_types():
    doc = make_document([
        ("heading", {"level": 2}, [{"insert": "Head"}], []),
        ("paragraph", {}, [{"insert": "para"}], []),
        ("bulleted_list", {}, [{"insert": "item"}], [
            ("bulleted_list", {}, [{"insert": "nested"}], []),
        ]),
        ("numbered_list", {}, [{"insert": "one"}], []),
        ("numbered_list", {}, [{"insert": "two"}], []),
        ("todo_list", {"checked": True}, [{"insert": "done"}], []),
        ("quote", {}, [{"insert": "quoted"}], []),
        ("code", {"language": "python"}, [{"insert": "print(1)"}], []),
        ("divider", {}, None, []),
        ("image", {"url": "https://x/i.png", "caption": "cap"}, None, []),
    ])
    md = document_to_markdown(doc, title="Title")
    assert md.startswith("# Title\n")
    assert "## Head" in md
    assert "- item\n  - nested" in md
    assert "1. one\n2. two" in md
    assert "- [x] done" in md
    assert "> quoted" in md
    assert "```python\nprint(1)\n```" in md
    assert "---" in md
    assert "![cap](https://x/i.png)" in md


def test_export_reimports_cleanly():
    """Exported Markdown should parse back into equivalent block types."""
    doc = make_document([
        ("heading", {"level": 1}, [{"insert": "H"}], []),
        ("paragraph", {}, [{"insert": "body "},
                           {"insert": "bold", "attributes": {"bold": True}}], []),
        ("todo_list", {"checked": False}, [{"insert": "task"}], []),
    ])
    md = document_to_markdown(doc)
    blocks = parse_markdown_to_blocks(md)
    assert [b["type"] for b in blocks] == ["heading", "paragraph", "todo_list"]
    para = blocks[1]["data"]["delta"]
    assert {"insert": "bold", "attributes": {"bold": True}} in para


def test_sanitize_filename():
    assert sanitize_filename('a/b:c*d?"e"', "fb") == 'a-b-c-d--e-'
    assert sanitize_filename("", "fb") == "fb"
    assert sanitize_filename("...", "fb") == "fb"


def test_fetch_page_markdown_retries_on_collab_not_found(monkeypatch):
    """A transient 'Collab not found' is retried, not dropped."""
    from appflowy_mcp import server

    calls = {"n": 0}

    def fake_request(method, path):
        calls["n"] += 1
        if calls["n"] < 3:
            raise Exception("Record not found:Collab not found (code -2)")
        return {"data": {"data": {"encoded_collab": [1, 2, 3]}}}

    monkeypatch.setattr(server.client, "_request", fake_request)
    monkeypatch.setattr(server, "decode_document", lambda e: {})
    monkeypatch.setattr(server, "document_to_markdown", lambda d, title=None: f"# {title}\n\nbody")
    monkeypatch.setattr(server.time, "sleep", lambda *a, **k: None)

    out = server.fetch_page_markdown("ws", "vid", "T")
    assert out == "# T\n\nbody"
    assert calls["n"] == 3


def test_fetch_page_markdown_retries_on_empty_body(monkeypatch):
    """A page that transiently decodes to only its title is retried."""
    from appflowy_mcp import server

    calls = {"n": 0}

    def fake_request(method, path):
        calls["n"] += 1
        return {"data": {"data": {"encoded_collab": [1]}}}

    def fake_md(d, title=None):
        return f"# {title}" if calls["n"] < 3 else f"# {title}\n\nbody"

    monkeypatch.setattr(server.client, "_request", fake_request)
    monkeypatch.setattr(server, "decode_document", lambda e: {})
    monkeypatch.setattr(server, "document_to_markdown", fake_md)
    monkeypatch.setattr(server.time, "sleep", lambda *a, **k: None)

    out = server.fetch_page_markdown("ws", "vid", "T")
    assert "body" in out
    assert calls["n"] == 3


def test_markdown_body_is_empty():
    from appflowy_mcp import server

    assert server._markdown_body_is_empty("# 概述", "概述") is True
    assert server._markdown_body_is_empty("# 概述\n", "概述") is True
    assert server._markdown_body_is_empty("# 概述\n\ncontent", "概述") is False


def test_encoded_collab_bytes_shapes():
    import base64
    from appflowy_mcp import server

    assert server._encoded_collab_bytes([1, 2, 3]) == [1, 2, 3]
    assert server._encoded_collab_bytes({"doc_state": [4, 5]}) == [4, 5]
    b64 = base64.b64encode(bytes([6, 7])).decode()
    assert server._encoded_collab_bytes(b64) == [6, 7]
    assert server._encoded_collab_bytes({"doc_state": b64}) == [6, 7]
    assert server._encoded_collab_bytes(None) is None
    assert server._encoded_collab_bytes([]) is None


def test_fetch_collab_markdown_tries_both_collab_types(monkeypatch):
    from appflowy_mcp import server

    seen = []

    def fake_request(method, path, params=None, json_body=None):
        seen.append(params.get("collab_type"))
        if params.get("collab_type") == 0:
            raise Exception("invalid collab_type")
        return {"data": {"encode_collab": {"doc_state": [9]}}}

    monkeypatch.setattr(server.client, "_request", fake_request)
    monkeypatch.setattr(server, "decode_document", lambda e: {})
    monkeypatch.setattr(server, "document_to_markdown", lambda d, title=None: f"# {title}\n\nx")

    out = server.fetch_collab_markdown("ws", "oid", "T")
    assert out == "# T\n\nx"
    assert seen == [0, "Document"]


def test_page_markdown_falls_back_to_collab(monkeypatch):
    from appflowy_mcp import server

    def fake_request(method, path, params=None, json_body=None):
        if "page-view" in path:
            raise Exception("Record not found:Collab not found (code -2)")
        return {"data": {"encode_collab": {"doc_state": [1, 2, 3]}}}

    monkeypatch.setattr(server.client, "_request", fake_request)
    monkeypatch.setattr(server, "decode_document", lambda e: {})
    monkeypatch.setattr(server, "document_to_markdown", lambda d, title=None: f"# {title}\n\nrecovered")
    monkeypatch.setattr(server.time, "sleep", lambda *a, **k: None)

    out = server.fetch_page_markdown("ws", "vid", "T")
    assert out == "# T\n\nrecovered"

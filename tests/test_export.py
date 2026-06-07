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

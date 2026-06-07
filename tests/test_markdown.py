"""Offline tests for Markdown -> AppFlowy block conversion."""

from appflowy_mcp.markdown import (
    parse_markdown_to_blocks,
    parse_plain_text_to_blocks,
    parse_rich_text,
)


def test_blank_lines_are_not_emitted_as_blocks():
    """Regression: blank lines between paragraphs must not become empty
    paragraph blocks (imported docs were littered with blank lines)."""
    md = "# Title\n\nPara one.\n\n\nPara two.\n\n- a\n- b\n"
    blocks = parse_markdown_to_blocks(md)
    types = [b["type"] for b in blocks]
    assert types == [
        "heading",
        "paragraph",
        "paragraph",
        "bulleted_list",
        "bulleted_list",
    ]
    for block in blocks:
        delta = block["data"].get("delta", [])
        assert delta, f"block has empty delta: {block}"
        assert all(d["insert"] for d in delta), f"empty insert in {block}"


def test_blank_lines_inside_code_blocks_are_preserved():
    md = "```python\nline1\n\nline3\n```"
    blocks = parse_markdown_to_blocks(md)
    assert len(blocks) == 1
    assert blocks[0]["type"] == "code"
    assert blocks[0]["data"]["delta"][0]["insert"] == "line1\n\nline3"


def test_headings_lists_quotes_and_dividers():
    md = (
        "# H1\n## H2\n### H3\n---\n1. first\n- [ ] todo\n- [x] done\n> quoted\n"
    )
    blocks = parse_markdown_to_blocks(md)
    assert [b["type"] for b in blocks] == [
        "heading",
        "heading",
        "heading",
        "divider",
        "numbered_list",
        "todo_list",
        "todo_list",
        "quote",
    ]
    assert [b["data"].get("level") for b in blocks[:3]] == [1, 2, 3]
    assert blocks[5]["data"]["checked"] is False
    assert blocks[6]["data"]["checked"] is True


def test_inline_formatting():
    deltas = parse_rich_text("**bold** and `code` and [link](https://x.example)")
    assert {"insert": "bold", "attributes": {"bold": True}} in deltas
    assert {"insert": "code", "attributes": {"code": True}} in deltas
    assert {
        "insert": "link",
        "attributes": {"href": "https://x.example"},
    } in deltas


def test_plain_text_keeps_blank_lines():
    """plain_text format is literal: blank lines stay as empty paragraphs."""
    blocks = parse_plain_text_to_blocks("a\n\nb")
    assert [b["data"]["delta"][0]["insert"] for b in blocks] == ["a", "", "b"]

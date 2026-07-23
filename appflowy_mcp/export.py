"""Convert AppFlowy document collabs back to Markdown.

The page-view endpoint returns ``encoded_collab`` — a raw yjs v1 update.
Decoding it with pycrdt preserves inline formatting (bold, italic, links)
that the server's ``collab/{id}/json`` endpoint flattens to plain text.
"""
import json
from typing import Any

from pycrdt import Doc, Map, Text


def decode_document(encoded_collab: bytes | list[int]) -> dict[str, Any]:
    """Decode an AppFlowy document collab into plain Python structures.

    Returns {page_id, blocks: {id: {ty, data, children, external_id}},
    children_map: {id: [block ids]}, text_map: {id: delta list}}.
    """
    if isinstance(encoded_collab, list):
        encoded_collab = bytes(encoded_collab)

    doc = Doc()
    doc.apply_update(encoded_collab)
    document = doc.get("data", type=Map)["document"]

    blocks: dict[str, Any] = {}
    for block_id in document["blocks"].keys():
        block = document["blocks"][block_id]
        raw_data = block["data"] if "data" in block.keys() else "{}"
        try:
            data = json.loads(raw_data) if raw_data else {}
        except Exception:
            data = {}
        blocks[block_id] = {
            "ty": block["ty"] if "ty" in block.keys() else "",
            "data": data,
            "children": block["children"] if "children" in block.keys() else "",
            "external_id": (
                block["external_id"] if "external_id" in block.keys() else None
            ),
        }

    meta = document["meta"]
    children_map = {
        key: list(meta["children_map"][key]) for key in meta["children_map"].keys()
    }

    text_map: dict[str, list[dict[str, Any]]] = {}
    for text_id in meta["text_map"].keys():
        value = meta["text_map"][text_id]
        if isinstance(value, Text):
            text_map[text_id] = [
                {"insert": insert, **({"attributes": attrs} if attrs else {})}
                for insert, attrs in value.diff()
            ]
        else:
            # Plain string (e.g. from the lossy collab/json fallback).
            text_map[text_id] = [{"insert": str(value)}] if value else []

    return {
        "page_id": document["page_id"],
        "blocks": blocks,
        "children_map": children_map,
        "text_map": text_map,
    }


def delta_to_markdown(delta: list[dict[str, Any]]) -> str:
    parts: list[str] = []
    for op in delta:
        text = op.get("insert", "")
        if not isinstance(text, str):
            continue
        attrs = op.get("attributes") or {}
        if attrs.get("code"):
            text = f"`{text}`"
        if attrs.get("bold"):
            text = f"**{text}**"
        if attrs.get("italic"):
            text = f"*{text}*"
        if attrs.get("strikethrough"):
            text = f"~~{text}~~"
        href = attrs.get("href")
        if href:
            text = f"[{text}]({href})"
        parts.append(text)
    return "".join(parts)


def _render_simple_table(table_block, blocks, children_map, block_text) -> list[str]:
    """Render an AppFlowy simple_table (table -> rows -> cells) as a GFM table.

    Returns un-prefixed Markdown lines; the first row is treated as the header.
    Each cell's text is its content blocks' text joined, with pipes/newlines
    neutralized so they don't break the table.
    """
    rows: list[list[str]] = []
    for row_id in children_map.get(table_block.get("children", ""), []):
        row = blocks.get(row_id)
        if not row or row.get("ty") != "simple_table_row":
            continue
        cells: list[str] = []
        for cell_id in children_map.get(row.get("children", ""), []):
            cell = blocks.get(cell_id)
            if not cell:
                continue
            parts: list[str] = []
            own = block_text(cell)
            if own:
                parts.append(own)
            for content_id in children_map.get(cell.get("children", ""), []):
                content = blocks.get(content_id)
                if content:
                    piece = block_text(content)
                    if piece:
                        parts.append(piece)
            text = " ".join(parts).replace("|", "\\|").replace("\n", " ").strip()
            cells.append(text)
        rows.append(cells)

    ncols = max((len(r) for r in rows), default=0)
    if not rows or ncols == 0:
        return []

    def fmt(cells: list[str]) -> str:
        padded = (cells + [""] * ncols)[:ncols]
        return "| " + " | ".join(padded) + " |"

    out = [fmt(rows[0]), "| " + " | ".join(["---"] * ncols) + " |"]
    out.extend(fmt(r) for r in rows[1:])
    return out


def document_to_markdown(document: dict[str, Any], title: str | None = None) -> str:
    """Render a decoded AppFlowy document as Markdown."""
    blocks = document["blocks"]
    children_map = document["children_map"]
    text_map = document["text_map"]

    lines: list[str] = []
    if title:
        lines.extend([f"# {title}", ""])

    def block_text(block: dict[str, Any]) -> str:
        external_id = block.get("external_id")
        if external_id and external_id in text_map:
            return delta_to_markdown(text_map[external_id])
        delta = block.get("data", {}).get("delta")
        if isinstance(delta, list):
            return delta_to_markdown(delta)
        return ""

    def render(block_ids: list[str], indent: int) -> None:
        prefix = "  " * indent
        number = 0
        for block_id in block_ids:
            block = blocks.get(block_id)
            if not block:
                continue
            ty = block["ty"]
            data = block["data"]
            text = block_text(block)
            number = number + 1 if ty == "numbered_list" else 0

            if ty == "heading":
                level = min(max(int(data.get("level", 1) or 1), 1), 6)
                lines.extend([f"{prefix}{'#' * level} {text}", ""])
            elif ty == "bulleted_list":
                lines.append(f"{prefix}- {text}")
            elif ty == "numbered_list":
                lines.append(f"{prefix}{number}. {text}")
            elif ty == "todo_list":
                mark = "x" if data.get("checked") else " "
                lines.append(f"{prefix}- [{mark}] {text}")
            elif ty == "quote":
                lines.extend([f"{prefix}> {text}", ""])
            elif ty == "code":
                language = data.get("language") or ""
                lines.extend([f"{prefix}```{language}", f"{text}", "```", ""])
            elif ty == "divider":
                lines.extend(["---", ""])
            elif ty == "image":
                url = data.get("url", "")
                caption = data.get("caption", "") or ""
                lines.extend([f"{prefix}![{caption}]({url})", ""])
            elif ty == "callout":
                icon = data.get("icon")
                callout = f"{icon} {text}".strip() if icon else text
                lines.extend([f"{prefix}> {callout}", ""])
            elif ty == "simple_table":
                table = _render_simple_table(block, blocks, children_map, block_text)
                if table:
                    lines.extend([prefix + row for row in table] + [""])
                else:
                    lines.extend([f"{prefix}<!-- unsupported block: {ty} -->", ""])
                continue  # rows/cells consumed here; skip generic child recursion
            elif ty == "math_equation":
                formula = (data.get("formula") or text or "").strip()
                if formula:
                    lines.extend([f"{prefix}$$", formula, "$$", ""])
            elif ty == "page":
                pass  # root container; children rendered below
            elif text:
                lines.append(f"{prefix}{text}")
                lines.append("")
            elif ty == "paragraph":
                pass  # empty paragraph -> blank separator already handled
            else:
                # A block type we don't render (e.g. table/grid/math). Leave a
                # visible marker so its presence isn't silently lost on export.
                lines.extend([f"{prefix}<!-- unsupported block: {ty} -->", ""])

            child_ids = children_map.get(block.get("children", ""), [])
            if child_ids:
                is_list = ty in {"bulleted_list", "numbered_list", "todo_list"}
                render(child_ids, indent + 1 if is_list else indent)
            if ty in {"bulleted_list", "numbered_list", "todo_list"}:
                next_index = block_ids.index(block_id) + 1
                next_block = (
                    blocks.get(block_ids[next_index])
                    if next_index < len(block_ids)
                    else None
                )
                if not next_block or next_block["ty"] != ty:
                    lines.append("")

    root_children = children_map.get(
        blocks.get(document["page_id"], {}).get("children", ""), []
    )
    render(root_children, 0)

    # Collapse trailing blank lines to a single newline.
    while lines and lines[-1] == "":
        lines.pop()
    return "\n".join(lines) + "\n"


def sanitize_filename(name: str, fallback: str) -> str:
    # Drop control chars (\r \n \t ...) — a title containing one produces an
    # invalid path and would otherwise crash the whole export (OSError 22).
    cleaned = "".join(
        "-" if ch in '/\\:*?"<>|' else ch
        for ch in (name or "")
        if ord(ch) >= 32
    ).strip().strip(".")
    return cleaned or fallback

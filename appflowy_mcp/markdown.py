import re
from typing import Any
from collections.abc import Callable


INLINE_PATTERN = re.compile(
    r"(?P<code>`[^`]+`)|"
    r"(?P<link>\[[^\]]+\]\([^)]+\))|"
    r"(?P<bold>\*\*[^*]+\*\*)|"
    r"(?P<strike>~~[^~]+~~)|"
    r"(?P<italic>\*[^*]+\*)"
)

HEADING_PATTERN = re.compile(r"^(#{1,6})\s+(.+)$")
ORDERED_LIST_PATTERN = re.compile(r"^\d+\.\s+(.+)$")
IMAGE_PATTERN = re.compile(r"^!\[(?P<alt>[^\]]*)\]\((?P<url>[^)]+)\)$")
INLINE_IMAGE_PATTERN = re.compile(r"!\[(?P<alt>[^\]]*)\]\((?P<url>[^)]+)\)")

TABLE_SEPARATOR_CELL_PATTERN = re.compile(r"^:?-+:?$")
UNESCAPED_PIPE_PATTERN = re.compile(r"(?<!\\)\|")
MATH_BLOCK_PATTERN = re.compile(r"^\$\$(?P<formula>.+?)\$\$$")


def parse_rich_text(text: str) -> list[dict[str, Any]]:
    if text == "":
        return [{"insert": ""}]

    deltas: list[dict[str, Any]] = []
    last_end = 0

    for match in INLINE_PATTERN.finditer(text):
        start, end = match.span()
        if start > last_end:
            deltas.append({"insert": text[last_end:start]})

        value = match.group()
        kind = match.lastgroup
        attributes: dict[str, Any] = {}
        content = value

        if kind == "code":
            content = value[1:-1]
            attributes["code"] = True
        elif kind == "link":
            link = re.match(r"\[(?P<text>[^\]]+)\]\((?P<url>[^)]+)\)", value)
            if link:
                content = link.group("text")
                attributes["href"] = link.group("url")
        elif kind == "bold":
            content = value[2:-2]
            attributes["bold"] = True
        elif kind == "strike":
            content = value[2:-2]
            attributes["strikethrough"] = True
        elif kind == "italic":
            content = value[1:-1]
            attributes["italic"] = True

        delta: dict[str, Any] = {"insert": content}
        if attributes:
            delta["attributes"] = attributes
        deltas.append(delta)
        last_end = end

    if last_end < len(text):
        deltas.append({"insert": text[last_end:]})

    return deltas


def parse_markdown_to_blocks(
    content: str,
    image_url_resolver: Callable[[str, str], str] | None = None,
) -> list[dict[str, Any]]:
    blocks: list[dict[str, Any]] = []
    code_lines: list[str] = []
    code_language = ""
    in_code_block = False

    # Stack of (indent_level, block) for the currently open list items, so that
    # more-indented items can be nested under the nearest less-indented one.
    list_stack: list[tuple[int, dict[str, Any]]] = []

    def add_list_item(level: int, block: dict[str, Any]) -> None:
        while list_stack and list_stack[-1][0] >= level:
            list_stack.pop()
        if list_stack:
            list_stack[-1][1].setdefault("children", []).append(block)
        else:
            blocks.append(block)
        list_stack.append((level, block))

    lines = _strip_front_matter(content.splitlines())
    total = len(lines)
    i = 0

    while i < total:
        line = lines[i]
        stripped = line.strip()

        if stripped.startswith("```"):
            if in_code_block:
                blocks.append(
                    {
                        "type": "code",
                        "data": {
                            "language": code_language or "text",
                            "delta": [{"insert": "\n".join(code_lines)}],
                        },
                    }
                )
                code_lines = []
                code_language = ""
                in_code_block = False
            else:
                list_stack.clear()
                code_language = stripped[3:].strip()
                in_code_block = True
            i += 1
            continue

        if in_code_block:
            code_lines.append(line)
            i += 1
            continue

        # Blank lines are paragraph separators, not content; skip them without
        # clearing the list stack so a blank between items doesn't break nesting.
        if not stripped:
            i += 1
            continue

        if stripped in {"---", "***"}:
            list_stack.clear()
            blocks.append({"type": "divider", "data": {}})
            i += 1
            continue

        if stripped.startswith("|") and i + 1 < total and _is_table_separator(
            lines[i + 1]
        ):
            header = lines[i]
            body: list[str] = []
            j = i + 2
            while j < total and lines[j].strip().startswith("|"):
                body.append(lines[j])
                j += 1
            list_stack.clear()
            blocks.append(_table_block(header, body))
            i = j
            continue

        if stripped == "$$":
            formula_lines: list[str] = []
            j = i + 1
            while j < total and lines[j].strip() != "$$":
                formula_lines.append(lines[j])
                j += 1
            list_stack.clear()
            blocks.append(_math_block("\n".join(formula_lines)))
            i = j + 1 if j < total else j
            continue

        math = MATH_BLOCK_PATTERN.match(stripped)
        if math:
            list_stack.clear()
            blocks.append(_math_block(math.group("formula")))
            i += 1
            continue

        image = IMAGE_PATTERN.match(stripped)
        if image:
            image_url = image.group("url")
            image_caption = image.group("alt")
            if image_url_resolver:
                image_url = image_url_resolver(image_url, image_caption)
            list_stack.clear()
            blocks.append(
                {
                    "type": "image",
                    "data": {
                        "url": image_url,
                        "caption": image_caption,
                    },
                }
            )
            i += 1
            continue

        heading = HEADING_PATTERN.match(stripped)
        if heading:
            list_stack.clear()
            blocks.append(_heading_block(len(heading.group(1)), heading.group(2)))
            i += 1
            continue

        if stripped.startswith("- [ ] ") or stripped.startswith("- [x] "):
            add_list_item(
                _list_indent_level(line),
                {
                    "type": "todo_list",
                    "data": {
                        "checked": stripped.startswith("- [x] "),
                        "delta": parse_rich_text(stripped[6:]),
                    },
                },
            )
            i += 1
            continue

        if stripped.startswith("- ") or stripped.startswith("* "):
            add_list_item(
                _list_indent_level(line),
                {
                    "type": "bulleted_list",
                    "data": {"delta": parse_rich_text(stripped[2:])},
                },
            )
            i += 1
            continue

        ordered = ORDERED_LIST_PATTERN.match(stripped)
        if ordered:
            add_list_item(
                _list_indent_level(line),
                {
                    "type": "numbered_list",
                    "data": {"delta": parse_rich_text(ordered.group(1))},
                },
            )
            i += 1
            continue

        if stripped.startswith("> "):
            list_stack.clear()
            blocks.append(
                {
                    "type": "quote",
                    "data": {"delta": parse_rich_text(stripped[2:])},
                }
            )
            i += 1
            continue

        if stripped != "":
            list_stack.clear()
        blocks.extend(_blocks_from_inline_images(line, image_url_resolver))
        i += 1

    if in_code_block and code_lines:
        blocks.append(
            {
                "type": "code",
                "data": {
                    "language": code_language or "text",
                    "delta": [{"insert": "\n".join(code_lines)}],
                },
            }
        )

    return blocks


def parse_plain_text_to_blocks(content: str) -> list[dict[str, Any]]:
    lines = content.splitlines() or [""]
    return [
        {
            "type": "paragraph",
            "data": {"delta": [{"insert": line}]},
        }
        for line in lines
    ]


def parse_content_to_blocks(
    content: str, content_format: str = "markdown"
) -> list[dict[str, Any]]:
    normalized = content_format.strip().lower()
    if normalized in {"markdown", "md"}:
        return parse_markdown_to_blocks(content)
    if normalized in {"plain_text", "plaintext", "text", "plain"}:
        return parse_plain_text_to_blocks(content)
    raise ValueError("content_format must be 'markdown' or 'plain_text'.")


def _heading_block(level: int, text: str) -> dict[str, Any]:
    return {
        "type": "heading",
        "data": {
            "level": level,
            "delta": parse_rich_text(text),
        },
    }


def _blocks_from_inline_images(
    line: str,
    image_url_resolver: Callable[[str, str], str] | None,
) -> list[dict[str, Any]]:
    blocks: list[dict[str, Any]] = []
    last_end = 0

    for match in INLINE_IMAGE_PATTERN.finditer(line):
        start, end = match.span()
        if start > last_end:
            blocks.append(_paragraph_block(line[last_end:start]))

        image_url = match.group("url")
        image_caption = match.group("alt")
        if image_url_resolver:
            image_url = image_url_resolver(image_url, image_caption)
        blocks.append(_image_block(image_url, image_caption))
        last_end = end

    if not blocks:
        return [_paragraph_block(line)]

    if last_end < len(line):
        blocks.append(_paragraph_block(line[last_end:]))

    return blocks


def _paragraph_block(text: str) -> dict[str, Any]:
    return {
        "type": "paragraph",
        "data": {"delta": parse_rich_text(text)},
    }


def _image_block(url: str, caption: str) -> dict[str, Any]:
    return {
        "type": "image",
        "data": {
            "url": url,
            "caption": caption,
        },
    }


def _strip_front_matter(lines: list[str]) -> list[str]:
    if not lines or lines[0].strip() != "---":
        return lines
    for idx in range(1, len(lines)):
        if lines[idx].strip() == "---":
            return lines[idx + 1 :]
    return lines


def _list_indent_level(line: str) -> int:
    width = 0
    for char in line:
        if char == "\t":
            width += 2
        elif char == " ":
            width += 1
        else:
            break
    return width // 2


def _split_table_row(row: str) -> list[str]:
    row = row.strip()
    if row.startswith("|"):
        row = row[1:]
    if row.endswith("|"):
        row = row[:-1]
    parts = UNESCAPED_PIPE_PATTERN.split(row)
    return [part.strip().replace("\\|", "|") for part in parts]


def _is_table_separator(line: str) -> bool:
    stripped = line.strip()
    if "|" not in stripped:
        return False
    cells = _split_table_row(stripped)
    if not cells:
        return False
    return all(TABLE_SEPARATOR_CELL_PATTERN.match(cell) for cell in cells)


def _table_block(header: str, body: list[str]) -> dict[str, Any]:
    ncols = len(_split_table_row(header))
    column_widths = {str(col): 150 for col in range(ncols)}
    rows = [_table_row_block(header, ncols)]
    rows.extend(_table_row_block(row, ncols) for row in body)
    return {
        "type": "simple_table",
        "data": {
            "enable_header_row": True,
            "enable_header_column": False,
            "column_widths": column_widths,
        },
        "children": rows,
    }


def _table_row_block(row: str, ncols: int) -> dict[str, Any]:
    cells = _split_table_row(row)
    cells = (cells + [""] * ncols)[:ncols]
    return {
        "type": "simple_table_row",
        "data": {},
        "children": [_table_cell_block(cell) for cell in cells],
    }


def _table_cell_block(text: str) -> dict[str, Any]:
    return {
        "type": "simple_table_cell",
        "data": {},
        "children": [
            {"type": "paragraph", "data": {"delta": parse_rich_text(text)}}
        ],
    }


def _math_block(formula: str) -> dict[str, Any]:
    return {"type": "math_equation", "data": {"formula": formula}}

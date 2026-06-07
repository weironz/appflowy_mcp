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

ORDERED_LIST_PATTERN = re.compile(r"^\d+\.\s+(.+)$")
IMAGE_PATTERN = re.compile(r"^!\[(?P<alt>[^\]]*)\]\((?P<url>[^)]+)\)$")
INLINE_IMAGE_PATTERN = re.compile(r"!\[(?P<alt>[^\]]*)\]\((?P<url>[^)]+)\)")


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

    for line in content.splitlines():
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
                code_language = stripped[3:].strip()
                in_code_block = True
            continue

        if in_code_block:
            code_lines.append(line)
            continue

        # Blank lines are paragraph separators in Markdown, not content;
        # emitting them as empty paragraphs litters imported docs with
        # blank lines.
        if not stripped:
            continue

        if stripped in {"---", "***"}:
            blocks.append({"type": "divider", "data": {}})
            continue

        image = IMAGE_PATTERN.match(stripped)
        if image:
            image_url = image.group("url")
            image_caption = image.group("alt")
            if image_url_resolver:
                image_url = image_url_resolver(image_url, image_caption)
            blocks.append(
                {
                    "type": "image",
                    "data": {
                        "url": image_url,
                        "caption": image_caption,
                    },
                }
            )
            continue

        if stripped.startswith("### "):
            blocks.append(_heading_block(3, stripped[4:]))
            continue
        if stripped.startswith("## "):
            blocks.append(_heading_block(2, stripped[3:]))
            continue
        if stripped.startswith("# "):
            blocks.append(_heading_block(1, stripped[2:]))
            continue

        if stripped.startswith("- [ ] ") or stripped.startswith("- [x] "):
            blocks.append(
                {
                    "type": "todo_list",
                    "data": {
                        "checked": stripped.startswith("- [x] "),
                        "delta": parse_rich_text(stripped[6:]),
                    },
                }
            )
            continue

        if stripped.startswith("- ") or stripped.startswith("* "):
            blocks.append(
                {
                    "type": "bulleted_list",
                    "data": {"delta": parse_rich_text(stripped[2:])},
                }
            )
            continue

        ordered = ORDERED_LIST_PATTERN.match(stripped)
        if ordered:
            blocks.append(
                {
                    "type": "numbered_list",
                    "data": {"delta": parse_rich_text(ordered.group(1))},
                }
            )
            continue

        if stripped.startswith("> "):
            blocks.append(
                {
                    "type": "quote",
                    "data": {"delta": parse_rich_text(stripped[2:])},
                }
            )
            continue

        blocks.extend(_blocks_from_inline_images(line, image_url_resolver))

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

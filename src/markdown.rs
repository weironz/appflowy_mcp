//! Markdown and plain-text -> AppFlowy block conversion.
//!
//! Faithful port of `appflowy_mcp/markdown.py`. The Python block dicts
//! (`{"type", "data", "children?"}`) are represented by [`Block`]; its
//! serialization shape matches the Python dicts exactly (an empty `children`
//! is omitted). Rich text runs are [`crate::delta::DeltaOp`].

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::delta::DeltaOp;

// ---------------------------------------------------------------------------
// Regexes (compiled once; regex crate has no lookaround, so UNESCAPED_PIPE is
// a hand-written scan in `split_table_row` instead of Rust `Regex`).
// ---------------------------------------------------------------------------

// The former single-alternation INLINE_PATTERN and link_detail_pattern were
// replaced by the scanning tokenizer below (nested inline parsing).

fn heading_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(#{1,6})\s+(.+)$").expect("HEADING_PATTERN must compile"))
}

fn ordered_list_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d+\.\s+(.+)$").expect("ORDERED_LIST_PATTERN must compile"))
}

fn image_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^!\[(?P<alt>[^\]]*)\]\((?P<url>[^)]+)\)$").expect("IMAGE_PATTERN must compile")
    })
}

fn inline_image_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"!\[(?P<alt>[^\]]*)\]\((?P<url>[^)]+)\)")
            .expect("INLINE_IMAGE_PATTERN must compile")
    })
}

fn table_separator_cell_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^:?-+:?$").expect("TABLE_SEPARATOR_CELL_PATTERN must compile"))
}

fn math_block_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\$\$(?P<formula>.+?)\$\$$").expect("MATH_BLOCK_PATTERN must compile")
    })
}

// ---------------------------------------------------------------------------
// Block
// ---------------------------------------------------------------------------

/// An AppFlowy block, mirroring the Python `{"type", "data", "children?"}`
/// dict. `data` is always a JSON object; `children` is omitted from the
/// serialized form when empty (simple_table / table rows / cells always carry
/// non-empty children, so the omission never affects them).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Block {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub data: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Block>,
}

impl Block {
    pub fn new(ty: impl Into<String>, data: Value) -> Self {
        Self {
            ty: ty.into(),
            data,
            children: Vec::new(),
        }
    }

    pub fn with_children(ty: impl Into<String>, data: Value, children: Vec<Block>) -> Self {
        Self {
            ty: ty.into(),
            data,
            children,
        }
    }
}

// ---------------------------------------------------------------------------
// Rich text
// ---------------------------------------------------------------------------

/// Split `text` into delta ops. A small scanning tokenizer recognizes
/// `` `code` ``, `[text](url)`, `**bold**`, `~~strike~~`, and `*italic*`
/// (in that priority order) at each position; container contents (bold,
/// strike, italic, link text) are re-parsed recursively so nested inline
/// formatting like `**[t](u)**` or `**a *b* c**` becomes ops whose
/// attributes are the union of the enclosing and inner marks. Behavior
/// notes (old single-alternation-regex parity unless listed otherwise):
///
/// - Marker candidates that fail to close are literal text — `**x`, `*x`,
///   `` `x ``, `~~x`, `[x](`, `[]()` etc. pass through unchanged.
/// - Opener must be followed by a non-marker char (mirrors `[^*]+` /
///   `[^~]+`): `****` and `~~~~` stay literal.
/// - A `*` pair that fails as `**bold**` falls back to literal `*` + a
///   re-scan of the second star, matching the old regex's restart.
/// - Repeated same-kind markers pair nearest-first ("先开先闭"):
///   `*bold *inner* tail*` is italic("bold ") + "inner" + italic(" tail").
///   A marker with mismatched kinds (`**a *b**`) now parses the outer
///   container with the unpaired inner marker kept as literal text — new
///   nesting capability the old regex refused.
/// - `_underline_` emphasis is still NOT parsed (snake_case safety), same
///   as the old pattern.
/// - Backslash escapes: `\X` for X in {`\`, `` ` ``, `*`, `[`, `]`} yields
///   literal X (this is the set the exporter escapes); any other `\X` keeps
///   its backslash. Escapes are honored everywhere except code spans
///   (verbatim) and link URLs (copied raw up to the first `)`).
pub fn parse_rich_text(text: &str) -> Vec<DeltaOp> {
    if text.is_empty() {
        return vec![DeltaOp::plain("")];
    }
    parse_inline(text, &Map::new())
}

/// Characters import un-escapes after a backslash. Kept in sync with
/// `crate::render::escape_inline` — the exporter escapes exactly this set.
fn is_escapable(b: u8) -> bool {
    matches!(b, b'\\' | b'`' | b'*' | b'[' | b']')
}

/// Next index of `marker` at or after `from`, treating `\X` (X escapable)
/// as literal text rather than a marker.
fn find_unescaped(text: &str, from: usize, marker: u8, pair: bool) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && is_escapable(bytes[i + 1]) {
            i += 2;
            continue;
        }
        if bytes[i] == marker && (!pair || bytes.get(i + 1) == Some(&marker)) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `` `...` ``: the first following backtick closes the span and the content
/// must be non-empty (the old `[^`]+` rule). Verbatim: no escapes, no marks
/// inside. Returns (content, end-exclusive).
fn try_code(text: &str, start: usize) -> Option<(&str, usize)> {
    let offset = text[start + 1..].find('`').filter(|off| *off > 0)?;
    Some((&text[start + 1..start + 1 + offset], start + 2 + offset))
}

/// `[text](url)`: the text runs to the first un-escaped `]` and must be
/// non-empty (`\]` may appear inside); the URL is the non-empty run up to
/// the first raw `)` — a literal `)` inside a URL is not representable,
/// same as the old `[^)]+` pattern. Returns (text, url, end-exclusive).
fn try_link(text: &str, start: usize) -> Option<(&str, &str, usize)> {
    let close = find_unescaped(text, start + 1, b']', false)?;
    if close == start + 1 || text.as_bytes().get(close + 1) != Some(&b'(') {
        return None;
    }
    let url_start = close + 2;
    let url_end = url_start + text[url_start..].find(')')?;
    if url_end == url_start {
        return None;
    }
    Some((
        &text[start + 1..close],
        &text[url_start..url_end],
        url_end + 1,
    ))
}

/// `(...)...(...)` style container (`**bold**`, `*italic*`, `~~strike~~`):
/// the opener must be followed by a non-marker char and a matching closer
/// must exist. Returns (content, end-exclusive).
fn try_container(text: &str, start: usize, marker: u8, width: usize) -> Option<(&str, usize)> {
    let content_start = start + width;
    match text.as_bytes().get(content_start) {
        Some(&b) if b != marker => {}
        _ => return None,
    }
    let pair = width == 2;
    let end = find_unescaped(text, content_start + 1, marker, pair)?;
    Some((&text[content_start..end], end + width))
}

/// Recursive body of [`parse_rich_text`]: every op emitted here carries the
/// container attributes in `extra` merged with the marks found at this
/// level. The merge order mirrors `render::delta_to_markdown`'s wrapping
/// (code -> bold -> italic -> strike -> href from the inside out), which is
/// what makes export-then-import stable.
fn parse_inline(text: &str, extra: &Map<String, Value>) -> Vec<DeltaOp> {
    let mut ops: Vec<DeltaOp> = Vec::new();
    let mut plain = String::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;

    let flush = |ops: &mut Vec<DeltaOp>, plain: &mut String| {
        if !plain.is_empty() {
            let text = std::mem::take(plain);
            ops.push(if extra.is_empty() {
                DeltaOp::plain(text)
            } else {
                DeltaOp::with_attributes(text, extra.clone())
            });
        }
    };
    let push_op = |ops: &mut Vec<DeltaOp>, insert: &str, key: &str, value: Value| {
        let mut attrs = extra.clone();
        attrs.insert(key.to_string(), value);
        ops.push(if attrs.is_empty() {
            DeltaOp::plain(insert)
        } else {
            DeltaOp::with_attributes(insert, attrs)
        });
    };

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() && is_escapable(bytes[i + 1]) {
            // Recognized escape: literal X, drop the backslash.
            plain.push(bytes[i + 1] as char);
            i += 2;
            continue;
        }
        match b {
            b'`' => match try_code(text, i) {
                Some((content, end)) => {
                    flush(&mut ops, &mut plain);
                    push_op(&mut ops, content, "code", json!(true));
                    i = end;
                }
                None => {
                    plain.push('`');
                    i += 1;
                }
            },
            b'[' => match try_link(text, i) {
                Some((link_text, url, end)) => {
                    flush(&mut ops, &mut plain);
                    let mut attrs = extra.clone();
                    attrs.insert("href".to_string(), json!(url));
                    ops.extend(parse_inline(link_text, &attrs));
                    i = end;
                }
                None => {
                    plain.push('[');
                    i += 1;
                }
            },
            b'*' => {
                let pair = bytes.get(i + 1) == Some(&b'*');
                let (width, key) = if pair { (2, "bold") } else { (1, "italic") };
                match try_container(text, i, b'*', width) {
                    Some((content, end)) => {
                        flush(&mut ops, &mut plain);
                        let mut attrs = extra.clone();
                        attrs.insert(key.to_string(), json!(true));
                        ops.extend(parse_inline(content, &attrs));
                        i = end;
                    }
                    // On a failed `**` only the first star goes literal; the
                    // second is re-examined (it may still open italic).
                    None => {
                        plain.push('*');
                        i += 1;
                    }
                }
            }
            b'~' if bytes.get(i + 1) == Some(&b'~') => match try_container(text, i, b'~', 2) {
                Some((content, end)) => {
                    flush(&mut ops, &mut plain);
                    let mut attrs = extra.clone();
                    attrs.insert("strikethrough".to_string(), json!(true));
                    ops.extend(parse_inline(content, &attrs));
                    i = end;
                }
                None => {
                    plain.push('~');
                    i += 1;
                }
            },
            // Non-ASCII bytes are UTF-8 sequences; step by whole chars.
            _ => {
                let ch = text[i..].chars().next().expect("non-empty tail");
                plain.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    flush(&mut ops, &mut plain);
    ops
}

// ---------------------------------------------------------------------------
// Block builders (mirror _heading_block/_paragraph_block/... in Python)
// ---------------------------------------------------------------------------

fn delta_data(ops: Vec<DeltaOp>) -> Value {
    json!({ "delta": ops })
}

pub fn heading_block(level: usize, text: &str) -> Block {
    Block::new(
        "heading",
        json!({ "level": level, "delta": parse_rich_text(text) }),
    )
}

pub fn paragraph_block(text: &str) -> Block {
    Block::new("paragraph", delta_data(parse_rich_text(text)))
}

pub fn image_block(url: impl Into<String>, caption: impl Into<String>) -> Block {
    Block::new(
        "image",
        json!({ "url": url.into(), "caption": caption.into() }),
    )
}

pub fn math_block(formula: impl Into<String>) -> Block {
    Block::new("math_equation", json!({ "formula": formula.into() }))
}

pub fn table_block(header: &str, body: &[String]) -> Block {
    let ncols = split_table_row(header).len();
    let mut column_widths = Map::new();
    for col in 0..ncols {
        column_widths.insert(col.to_string(), json!(150));
    }
    let mut rows = vec![table_row_block(header, ncols)];
    for row in body {
        rows.push(table_row_block(row, ncols));
    }
    Block::with_children(
        "simple_table",
        json!({
            "enable_header_row": true,
            "enable_header_column": false,
            "column_widths": Value::Object(column_widths),
        }),
        rows,
    )
}

pub fn table_row_block(row: &str, ncols: usize) -> Block {
    let mut cells = split_table_row(row);
    cells.resize(ncols, String::new());
    cells.truncate(ncols);
    Block::with_children(
        "simple_table_row",
        json!({}),
        cells.iter().map(|c| table_cell_block(c)).collect(),
    )
}

pub fn table_cell_block(text: &str) -> Block {
    Block::with_children(
        "simple_table_cell",
        json!({}),
        vec![Block::new("paragraph", delta_data(parse_rich_text(text)))],
    )
}

// ---------------------------------------------------------------------------
// Table helpers
// ---------------------------------------------------------------------------

/// Split a table row on unescaped pipes. Python uses the regex `(?<!\\)\|`;
/// the regex crate has no lookbehind, so this is a manual per-character scan
/// with identical semantics: a `|` splits only when the immediately preceding
/// character is not `\` (no parity counting, matching lookbehind exactly).
fn split_on_unescaped_pipes(row: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut prev: Option<char> = None;
    for ch in row.chars() {
        if ch == '|' && prev != Some('\\') {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
        prev = Some(ch);
    }
    parts.push(current);
    parts
}

fn split_table_row(row: &str) -> Vec<String> {
    let mut row = row.trim();
    if let Some(rest) = row.strip_prefix('|') {
        row = rest;
    }
    if let Some(rest) = row.strip_suffix('|') {
        row = rest;
    }
    split_on_unescaped_pipes(row)
        .into_iter()
        .map(|part| part.trim().replace("\\|", "|"))
        .collect()
}

fn is_table_separator(line: &str) -> bool {
    let stripped = line.trim();
    if !stripped.contains('|') {
        return false;
    }
    let cells = split_table_row(stripped);
    if cells.is_empty() {
        return false;
    }
    cells
        .iter()
        .all(|cell| table_separator_cell_pattern().is_match(cell))
}

// ---------------------------------------------------------------------------
// Front matter / list helpers
// ---------------------------------------------------------------------------

/// Strip YAML front matter: if the first line is `---`, drop everything up to
/// and including the next `---` line; keep the input unchanged if no closing
/// marker exists.
fn strip_front_matter(lines: &mut Vec<String>) {
    if lines.first().map(|l| l.trim()) != Some("---") {
        return;
    }
    for idx in 1..lines.len() {
        if lines[idx].trim() == "---" {
            lines.drain(..=idx);
            return;
        }
    }
}

/// Indent level of a list item: tab counts as width 2, space as width 1, and
/// each 2 units of width make one level.
fn list_indent_level(line: &str) -> usize {
    let mut width = 0usize;
    for ch in line.chars() {
        match ch {
            '\t' => width += 2,
            ' ' => width += 1,
            _ => break,
        }
    }
    width / 2
}

/// Navigate to the block at `path` (indices of children, top-down).
fn block_at_path_mut<'a>(blocks: &'a mut [Block], path: &[usize]) -> &'a mut Block {
    let mut current = &mut blocks[path[0]];
    for &idx in &path[1..] {
        current = &mut current.children[idx];
    }
    current
}

// ---------------------------------------------------------------------------
// Markdown -> blocks
// ---------------------------------------------------------------------------

/// An `image_url_resolver(url, caption) -> url` callback, modeled on the
/// Python Callable parameter.
pub type ImageUrlResolver<'a> = dyn FnMut(&str, &str) -> String + 'a;

/// Parse markdown into AppFlowy blocks. Mirrors `parse_markdown_to_blocks`;
/// list item nesting uses a stack of `(indent_level, path)` so that more-
/// indented items nest under the nearest less-indented open item.
pub fn parse_markdown_to_blocks(
    content: &str,
    image_url_resolver: Option<&mut ImageUrlResolver<'_>>,
) -> Vec<Block> {
    let mut resolver = image_url_resolver;
    let mut blocks: Vec<Block> = Vec::new();
    let mut code_lines: Vec<String> = Vec::new();
    let mut code_language = String::new();
    let mut in_code_block = false;

    // Stack of (indent_level, path) for the currently open list items; a path
    // locates the live block inside `blocks` so nested children are attached
    // to the real parent (Python mutates the same dict in place).
    let mut list_stack: Vec<(usize, Vec<usize>)> = Vec::new();

    fn push_list_item(
        blocks: &mut Vec<Block>,
        list_stack: &mut Vec<(usize, Vec<usize>)>,
        level: usize,
        block: Block,
    ) {
        while matches!(list_stack.last(), Some(&(top, _)) if top >= level) {
            list_stack.pop();
        }
        let path = match list_stack.last() {
            Some((_, parent_path)) => {
                let parent = block_at_path_mut(blocks, parent_path);
                parent.children.push(block);
                let mut path = parent_path.clone();
                path.push(parent.children.len() - 1);
                path
            }
            None => {
                blocks.push(block);
                vec![blocks.len() - 1]
            }
        };
        list_stack.push((level, path));
    }

    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    strip_front_matter(&mut lines);
    let total = lines.len();
    let mut i = 0usize;

    while i < total {
        let line = lines[i].clone();
        let stripped = line.trim();

        if let Some(fence_info) = stripped.strip_prefix("```") {
            if in_code_block {
                blocks.push(Block::new(
                    "code",
                    json!({
                        "language": if code_language.is_empty() { "text" } else { &code_language },
                        "delta": [{ "insert": code_lines.join("\n") }],
                    }),
                ));
                code_lines.clear();
                code_language.clear();
                in_code_block = false;
            } else {
                list_stack.clear();
                code_language = fence_info.trim().to_string();
                in_code_block = true;
            }
            i += 1;
            continue;
        }

        if in_code_block {
            code_lines.push(line.clone());
            i += 1;
            continue;
        }

        // Blank lines are paragraph separators, not content; skip them
        // without clearing the list stack so a blank between items doesn't
        // break nesting (same rule as the Python original).
        if stripped.is_empty() {
            i += 1;
            continue;
        }

        if stripped == "---" || stripped == "***" {
            list_stack.clear();
            blocks.push(Block::new("divider", json!({})));
            i += 1;
            continue;
        }

        if stripped.starts_with('|') && i + 1 < total && is_table_separator(&lines[i + 1]) {
            let header = lines[i].clone();
            let mut body: Vec<String> = Vec::new();
            let mut j = i + 2;
            while j < total && lines[j].trim().starts_with('|') {
                body.push(lines[j].clone());
                j += 1;
            }
            list_stack.clear();
            blocks.push(table_block(&header, &body));
            i = j;
            continue;
        }

        if stripped == "$$" {
            let mut formula_lines: Vec<&str> = Vec::new();
            let mut j = i + 1;
            while j < total && lines[j].trim() != "$$" {
                formula_lines.push(&lines[j]);
                j += 1;
            }
            list_stack.clear();
            blocks.push(math_block(formula_lines.join("\n")));
            i = if j < total { j + 1 } else { j };
            continue;
        }

        if let Some(math) = math_block_pattern().captures(stripped) {
            if let Some(formula) = math.name("formula") {
                list_stack.clear();
                blocks.push(math_block(formula.as_str().to_string()));
                i += 1;
                continue;
            }
        }

        if let Some(image) = image_pattern().captures(stripped) {
            let alt = image.name("alt").map(|m| m.as_str()).unwrap_or("");
            let url = image.name("url").map(|m| m.as_str()).unwrap_or("");
            let image_url = match resolver.as_deref_mut() {
                Some(resolve) => resolve(url, alt),
                None => url.to_string(),
            };
            list_stack.clear();
            blocks.push(image_block(image_url, alt.to_string()));
            i += 1;
            continue;
        }

        if let Some(heading) = heading_pattern().captures(stripped) {
            let level = heading.get(1).map(|m| m.as_str().len()).unwrap_or(0);
            let text = heading.get(2).map(|m| m.as_str()).unwrap_or("");
            list_stack.clear();
            blocks.push(heading_block(level, text));
            i += 1;
            continue;
        }

        if stripped.starts_with("- [ ] ") || stripped.starts_with("- [x] ") {
            push_list_item(
                &mut blocks,
                &mut list_stack,
                list_indent_level(&line),
                Block::new(
                    "todo_list",
                    json!({
                        "checked": stripped.starts_with("- [x] "),
                        "delta": parse_rich_text(&stripped[6..]),
                    }),
                ),
            );
            i += 1;
            continue;
        }

        if stripped.starts_with("- ") || stripped.starts_with("* ") {
            push_list_item(
                &mut blocks,
                &mut list_stack,
                list_indent_level(&line),
                Block::new("bulleted_list", delta_data(parse_rich_text(&stripped[2..]))),
            );
            i += 1;
            continue;
        }

        if let Some(ordered) = ordered_list_pattern().captures(stripped) {
            let text = ordered.get(1).map(|m| m.as_str()).unwrap_or("");
            push_list_item(
                &mut blocks,
                &mut list_stack,
                list_indent_level(&line),
                Block::new("numbered_list", delta_data(parse_rich_text(text))),
            );
            i += 1;
            continue;
        }

        if let Some(quote_text) = stripped.strip_prefix("> ") {
            list_stack.clear();
            blocks.push(Block::new("quote", delta_data(parse_rich_text(quote_text))));
            i += 1;
            continue;
        }

        if !stripped.is_empty() {
            list_stack.clear();
        }
        blocks.extend(blocks_from_inline_images(&line, resolver.as_deref_mut()));
        i += 1;
    }

    if in_code_block && !code_lines.is_empty() {
        blocks.push(Block::new(
            "code",
            json!({
                "language": if code_language.is_empty() { "text" } else { &code_language },
                "delta": [{ "insert": code_lines.join("\n") }],
            }),
        ));
    }

    blocks
}

/// Split a fallback paragraph line around inline image syntax, emitting an
/// image block (with resolved URL) per `![alt](url)` and paragraph blocks for
/// the surrounding text. Mirrors `_blocks_from_inline_images`.
fn blocks_from_inline_images(
    line: &str,
    resolver: Option<&mut ImageUrlResolver<'_>>,
) -> Vec<Block> {
    let mut resolver = resolver;
    let mut blocks: Vec<Block> = Vec::new();
    let mut last_end = 0usize;

    for caps in inline_image_pattern().captures_iter(line) {
        let m = match caps.get(0) {
            Some(m) => m,
            None => continue,
        };
        let (start, end) = (m.start(), m.end());
        if start > last_end {
            blocks.push(paragraph_block(&line[last_end..start]));
        }

        let alt = caps.name("alt").map(|m| m.as_str()).unwrap_or("");
        let url = caps.name("url").map(|m| m.as_str()).unwrap_or("");
        let image_url = match resolver.as_deref_mut() {
            Some(resolve) => resolve(url, alt),
            None => url.to_string(),
        };
        blocks.push(image_block(image_url, alt.to_string()));
        last_end = end;
    }

    if blocks.is_empty() {
        return vec![paragraph_block(line)];
    }

    if last_end < line.len() {
        blocks.push(paragraph_block(&line[last_end..]));
    }

    blocks
}

// ---------------------------------------------------------------------------
// Plain text / dispatcher
// ---------------------------------------------------------------------------

/// One paragraph block per line; blank lines become empty paragraphs.
/// Mirrors `parse_plain_text_to_blocks` (empty content yields one empty
/// paragraph, like Python's `content.splitlines() or [""]`). The delta is a
/// literal `{"insert": line}` — no inline rich-text parsing, unlike markdown
/// paragraphs.
pub fn parse_plain_text_to_blocks(content: &str) -> Vec<Block> {
    let lines: Vec<&str> = {
        let v: Vec<&str> = content.lines().collect();
        if v.is_empty() {
            vec![""]
        } else {
            v
        }
    };
    lines
        .into_iter()
        .map(|line| Block::new("paragraph", json!({ "delta": [{ "insert": line }] })))
        .collect()
}

/// Dispatch on `content_format` (`markdown`/`md` or
/// `plain_text`/`plaintext`/`text`/`plain`, case-insensitive), rejecting
/// anything else with the same message as Python's ValueError.
pub fn parse_content_to_blocks(content: &str, content_format: &str) -> Result<Vec<Block>, String> {
    let normalized = content_format.trim().to_lowercase();
    match normalized.as_str() {
        "markdown" | "md" => Ok(parse_markdown_to_blocks(content, None)),
        "plain_text" | "plaintext" | "text" | "plain" => Ok(parse_plain_text_to_blocks(content)),
        _ => Err("content_format must be 'markdown' or 'plain_text'.".to_string()),
    }
}

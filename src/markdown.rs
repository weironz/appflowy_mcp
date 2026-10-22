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
///
/// Inline `![alt](url)` images have no delta representation, so they degrade
/// to their literal syntax as a plain op — nothing is lost, and the exporter
/// re-escapes the brackets so the text round-trips. Callers that can split
/// into multiple blocks use [`parse_rich_blocks`] instead, which emits real
/// image blocks.
pub fn parse_rich_text(text: &str) -> Vec<DeltaOp> {
    if text.is_empty() {
        return vec![DeltaOp::plain("")];
    }
    let events = parse_inline_events(text, &Map::new());
    let mut ops = Vec::with_capacity(events.len());
    for event in events {
        match event {
            RichEvent::Op(op) => ops.push(op),
            RichEvent::Image { alt, url } => {
                ops.push(DeltaOp::plain(format!("![{alt}]({url})")));
            }
        }
    }
    if ops.is_empty() {
        ops.push(DeltaOp::plain(""));
    }
    ops
}

/// Parse inline markdown into a run of blocks: the rich-text delta, split
/// around any inline `![alt](url)` images into standalone image blocks (the
/// delta format has no image op). Image URLs pass through `resolver` like
/// full-line images do. Callers that render a single-line container (list
/// items, headings, quotes) use this so images inside them are not silently
/// flattened into `!` + link; the first block is the container's own text
/// run and any further blocks are the trailing images/paragraphs.
fn parse_rich_blocks(text: &str, resolver: Option<&mut ImageUrlResolver<'_>>) -> Vec<Block> {
    if text.is_empty() {
        return vec![paragraph_block("")];
    }
    let mut resolver = resolver;
    let events = parse_inline_events(text, &Map::new());
    if !events.iter().any(|e| matches!(e, RichEvent::Image { .. })) {
        let ops = events
            .into_iter()
            .filter_map(|e| match e {
                RichEvent::Op(op) => Some(op),
                RichEvent::Image { .. } => None,
            })
            .collect();
        return vec![Block::new("paragraph", delta_data(ops))];
    }
    // Events hold delta ops and images interleaved in text order; a plain run
    // accumulates ops until an image breaks it. When the text opens with an
    // image, an empty paragraph still leads the run: container callers (list
    // items, headings, quotes) take their delta from the first block.
    let mut blocks = Vec::new();
    let mut run: Vec<DeltaOp> = Vec::new();
    if matches!(events.first(), Some(RichEvent::Image { .. })) {
        blocks.push(paragraph_block(""));
    }
    for event in events {
        match event {
            RichEvent::Op(op) => run.push(op),
            RichEvent::Image { alt, url } => {
                let has_text = run.iter().any(|op| !op.insert.is_empty());
                if has_text {
                    blocks.push(Block::new(
                        "paragraph",
                        delta_data(std::mem::take(&mut run)),
                    ));
                } else {
                    run.clear();
                }
                let image_url = match resolver.as_deref_mut() {
                    Some(resolve) => resolve(&url, &alt),
                    None => url,
                };
                blocks.push(image_block(image_url, alt));
            }
        }
    }
    if !run.iter().all(|op| op.insert.is_empty()) {
        blocks.push(Block::new("paragraph", delta_data(run)));
    }
    blocks
}

/// One inline parse event: a delta op, or an image that must break out of the
/// delta (emitted in exactly the order they occur in the text).
enum RichEvent {
    Op(DeltaOp),
    Image { alt: String, url: String },
}

/// Recursive body of [`parse_rich_text`]: every op emitted here carries the
/// container attributes in `extra` merged with the marks found at this
/// level. The merge order mirrors `render::delta_to_markdown`'s wrapping
/// (code -> bold -> italic -> strike -> href from the inside out), which is
/// what makes export-then-import stable.
///
/// Inline images are emitted as `Image` events at the position they occur;
/// the delta cannot carry them, so the caller decides how to split.
fn parse_inline_events(text: &str, extra: &Map<String, Value>) -> Vec<RichEvent> {
    let mut events: Vec<RichEvent> = Vec::new();
    let mut plain = String::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;

    let flush = |events: &mut Vec<RichEvent>, plain: &mut String| {
        if !plain.is_empty() {
            let text = std::mem::take(plain);
            events.push(if extra.is_empty() {
                RichEvent::Op(DeltaOp::plain(text))
            } else {
                RichEvent::Op(DeltaOp::with_attributes(text, extra.clone()))
            });
        }
    };
    let push_op = |events: &mut Vec<RichEvent>, insert: &str, key: &str, value: Value| {
        let mut attrs = extra.clone();
        attrs.insert(key.to_string(), value);
        events.push(if attrs.is_empty() {
            RichEvent::Op(DeltaOp::plain(insert))
        } else {
            RichEvent::Op(DeltaOp::with_attributes(insert, attrs))
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
                    flush(&mut events, &mut plain);
                    push_op(&mut events, content, "code", json!(true));
                    i = end;
                }
                None => {
                    plain.push('`');
                    i += 1;
                }
            },
            b'!' if bytes.get(i + 1) == Some(&b'[') => {
                // Inline image: emitted as an Image event at this position;
                // the caller splits the delta around it (or keeps the
                // literal syntax, see `parse_rich_text`).
                match try_image(text, i) {
                    Some((alt, url, end)) => {
                        flush(&mut events, &mut plain);
                        events.push(RichEvent::Image {
                            alt: alt.to_string(),
                            url: url.to_string(),
                        });
                        i = end;
                    }
                    None => {
                        plain.push('!');
                        i += 1;
                    }
                }
            }
            b'[' => match try_link(text, i) {
                Some((link_text, url, end)) => {
                    flush(&mut events, &mut plain);
                    let mut attrs = extra.clone();
                    attrs.insert("href".to_string(), json!(url));
                    let nested = parse_inline_events(link_text, &attrs);
                    events.extend(nested);
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
                        flush(&mut events, &mut plain);
                        let mut attrs = extra.clone();
                        attrs.insert(key.to_string(), json!(true));
                        let nested = parse_inline_events(content, &attrs);
                        events.extend(nested);
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
                    flush(&mut events, &mut plain);
                    let mut attrs = extra.clone();
                    attrs.insert("strikethrough".to_string(), json!(true));
                    let nested = parse_inline_events(content, &attrs);
                    events.extend(nested);
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
    flush(&mut events, &mut plain);
    events
}

/// Characters import un-escapes after a backslash. Kept in sync with the
/// exporter: `crate::render::escape_inline` escapes `` \ ` * [ ] ~ `` on
/// every plain run, and the paragraph renderer additionally escapes
/// line-initial `#`, `-`, `>`, `.` and `$` (block markers) — both sides must
/// agree or exported escapes surface as literal backslashes.
fn is_escapable(b: u8) -> bool {
    matches!(
        b,
        b'\\' | b'`' | b'*' | b'[' | b']' | b'~' | b'#' | b'-' | b'>' | b'.' | b'$'
    )
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

/// `[text](url)`: the text runs to the first un-escaped `]` followed by `(`,
/// and must be non-empty (`\]` may appear inside); the URL is the non-empty
/// run up to the first raw `)` — a literal `)` inside a URL is not
/// representable, same as the old `[^)]+` pattern. When no un-escaped `]`
/// exists at all, the first `\]` followed by `(` closes instead: the
/// exporter escapes `]` inside link text, so `[a\](u)` is how a link whose
/// label ends in `]` is spelled; the recursive parse of the text un-escapes
/// the `\]` back to a literal `]`. Returns (text, url, end-exclusive).
fn try_link(text: &str, start: usize) -> Option<(&str, &str, usize)> {
    let bytes = text.as_bytes();
    let mut close = None;
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && is_escapable(bytes[i + 1]) {
            i += 2;
            continue;
        }
        if bytes[i] == b']' {
            close = Some(i);
            break;
        }
        i += 1;
    }
    let close = match close {
        Some(close) if bytes.get(close + 1) == Some(&b'(') => close,
        // A `]` not followed by `(` can't close a link — literal text.
        Some(_) => return None,
        None => {
            // Fallback closer: an escaped `\]` directly before `(`.
            let mut i = start + 1;
            let mut escaped = None;
            while i + 1 < bytes.len() {
                if bytes[i] == b'\\' && bytes[i + 1] == b']' && bytes.get(i + 2) == Some(&b'(') {
                    escaped = Some(i);
                    break;
                }
                i += 1;
            }
            escaped?
        }
    };
    if close == start + 1 {
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

/// `![alt](url)`: like [`try_link`] with a leading `!`; the alt may be empty,
/// the URL may not. Returns (alt, url, end-exclusive).
fn try_image(text: &str, start: usize) -> Option<(&str, &str, usize)> {
    let bytes = text.as_bytes();
    debug_assert!(bytes[start] == b'!' && bytes.get(start + 1) == Some(&b'['));
    let close = find_unescaped(text, start + 2, b']', false)?;
    if bytes.get(close + 1) != Some(&b'(') {
        return None;
    }
    let url_start = close + 2;
    let url_end = url_start + text[url_start..].find(')')?;
    if url_end == url_start {
        return None;
    }
    Some((
        &text[start + 2..close],
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

// ---------------------------------------------------------------------------
// Block builders (mirror _heading_block/_paragraph_block/... in Python)
// ---------------------------------------------------------------------------

fn delta_data(ops: Vec<DeltaOp>) -> Value {
    json!({ "delta": ops })
}

/// The delta array inside a block built by `parse_rich_blocks` (its first
/// block is always a paragraph whose data is `{"delta": [...]}`).
fn delta_ops_of(block: &Block) -> Vec<DeltaOp> {
    block
        .data
        .get("delta")
        .cloned()
        .and_then(|delta| serde_json::from_value(delta).ok())
        .unwrap_or_default()
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
/// marker exists. Every line between the markers must look like YAML (`key:
/// value`, `key:` or a `#` comment) — a lone `---` followed by plain content
/// is a leading divider + text, and stripping it would silently eat that
/// content (the exporter emits leading dividers as `***` precisely so a
/// genuine front matter stays unambiguous, but third-party Markdown may
/// still lead with a divider).
fn strip_front_matter(lines: &mut Vec<&str>) {
    if lines.first().map(|l| l.trim()) != Some("---") {
        return;
    }
    for idx in 1..lines.len() {
        let line = lines[idx].trim();
        if line == "---" {
            let is_front_matter = lines[1..idx].iter().all(|l| looks_like_yaml(l.trim()));
            if is_front_matter {
                lines.drain(..=idx);
            }
            // Not front matter: leave the lines untouched; the leading `---`
            // parses as a divider further down.
            return;
        }
    }
}

/// A line plausible inside YAML front matter: `key: value`, `key:`, a `#`
/// comment, or blank (blank lines never appear — the caller filters them —
/// but tolerate one defensively).
fn looks_like_yaml(line: &str) -> bool {
    if line.is_empty() || line.starts_with('#') {
        return true;
    }
    // A key is a non-empty run without `:`, followed by `:` + space or EOL.
    match line.find(':') {
        Some(colon) => {
            let key = &line[..colon];
            !key.is_empty()
                && !key.starts_with(' ')
                && (colon + 1 == line.len() || line.as_bytes().get(colon + 1) == Some(&b' '))
        }
        None => false,
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
    let mut code_indent = String::new();
    let mut code_level = 0usize;
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

    fn push_non_list_block(
        blocks: &mut Vec<Block>,
        list_stack: &mut Vec<(usize, Vec<usize>)>,
        level: usize,
        block: Block,
    ) {
        while matches!(list_stack.last(), Some(&(top, _)) if top >= level) {
            list_stack.pop();
        }
        match list_stack.last() {
            Some((_, parent_path)) => {
                block_at_path_mut(blocks, parent_path).children.push(block);
            }
            None => blocks.push(block),
        }
    }

    // `&str` borrows, not owned Strings: the loop below re-clones lines only
    // where they actually outlive the iteration (code blocks, tables).
    let mut lines: Vec<&str> = content.lines().collect();
    strip_front_matter(&mut lines);
    let total = lines.len();
    let mut i = 0usize;

    while i < total {
        let line = lines[i];
        let stripped = line.trim();

        if let Some(fence_info) = stripped.strip_prefix("```") {
            if in_code_block {
                let block = Block::new(
                    "code",
                    json!({
                        "language": if code_language.is_empty() { "text" } else { &code_language },
                        "delta": [{ "insert": code_lines.join("\n") }],
                    }),
                );
                push_non_list_block(&mut blocks, &mut list_stack, code_level, block);
                code_lines.clear();
                code_language.clear();
                code_indent.clear();
                in_code_block = false;
            } else {
                code_level = list_indent_level(line);
                code_indent =
                    line[..line.len() - line.trim_start_matches([' ', '\t']).len()].to_string();
                code_language = fence_info.trim().to_string();
                in_code_block = true;
            }
            i += 1;
            continue;
        }

        if in_code_block {
            code_lines.push(line.strip_prefix(&code_indent).unwrap_or(line).to_string());
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
            let level = list_indent_level(line);
            push_non_list_block(
                &mut blocks,
                &mut list_stack,
                level,
                Block::new("divider", json!({})),
            );
            i += 1;
            continue;
        }

        if stripped.starts_with('|') && i + 1 < total && is_table_separator(lines[i + 1]) {
            let header = lines[i];
            let mut body: Vec<String> = Vec::new();
            let mut j = i + 2;
            while j < total && lines[j].trim().starts_with('|') {
                body.push(lines[j].to_string());
                j += 1;
            }
            let level = list_indent_level(line);
            push_non_list_block(
                &mut blocks,
                &mut list_stack,
                level,
                table_block(header, &body),
            );
            i = j;
            continue;
        }

        if stripped == "$$" {
            let level = list_indent_level(line);
            let indent = &line[..line.len() - line.trim_start_matches([' ', '\t']).len()];
            let mut formula_lines: Vec<String> = Vec::new();
            let mut j = i + 1;
            while j < total && lines[j].trim() != "$$" {
                formula_lines.push(
                    lines[j]
                        .strip_prefix(indent)
                        .unwrap_or(lines[j])
                        .to_string(),
                );
                j += 1;
            }
            push_non_list_block(
                &mut blocks,
                &mut list_stack,
                level,
                math_block(formula_lines.join("\n")),
            );
            i = if j < total { j + 1 } else { j };
            continue;
        }

        if let Some(math) = math_block_pattern().captures(stripped) {
            if let Some(formula) = math.name("formula") {
                let level = list_indent_level(line);
                push_non_list_block(
                    &mut blocks,
                    &mut list_stack,
                    level,
                    math_block(formula.as_str().to_string()),
                );
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
            let level = list_indent_level(line);
            let block = image_block(image_url, alt.to_string());
            push_non_list_block(&mut blocks, &mut list_stack, level, block);
            i += 1;
            continue;
        }

        if let Some(heading) = heading_pattern().captures(stripped) {
            let heading_level = heading.get(1).map(|m| m.as_str().len()).unwrap_or(0);
            let text = heading.get(2).map(|m| m.as_str()).unwrap_or("");
            // Inline images inside the heading text break out as trailing
            // blocks (the heading delta cannot carry them).
            let mut run = parse_rich_blocks(text, resolver.as_deref_mut());
            let head = run.first().cloned().unwrap_or_else(|| paragraph_block(""));
            let block = Block::with_children(
                "heading",
                json!({ "level": heading_level, "delta": head.data.get("delta").cloned().unwrap_or(json!([])) }),
                run.drain(1..).collect(),
            );
            push_non_list_block(&mut blocks, &mut list_stack, list_indent_level(line), block);
            i += 1;
            continue;
        }

        if stripped.starts_with("- [ ] ") || stripped.starts_with("- [x] ") {
            let mut run = parse_rich_blocks(&stripped[6..], resolver.as_deref_mut());
            let head = run.first().cloned().unwrap_or_else(|| paragraph_block(""));
            let extra: Vec<Block> = run.drain(1..).collect();
            push_list_item(
                &mut blocks,
                &mut list_stack,
                list_indent_level(line),
                Block::with_children(
                    "todo_list",
                    json!({
                        "checked": stripped.starts_with("- [x] "),
                        "delta": head.data.get("delta").cloned().unwrap_or(json!([])),
                    }),
                    extra,
                ),
            );
            i += 1;
            continue;
        }

        if stripped.starts_with("- ") || stripped.starts_with("* ") {
            let mut run = parse_rich_blocks(&stripped[2..], resolver.as_deref_mut());
            let head = run.first().cloned().unwrap_or_else(|| paragraph_block(""));
            let extra: Vec<Block> = run.drain(1..).collect();
            push_list_item(
                &mut blocks,
                &mut list_stack,
                list_indent_level(line),
                Block::with_children("bulleted_list", delta_data(delta_ops_of(&head)), extra),
            );
            i += 1;
            continue;
        }

        if let Some(ordered) = ordered_list_pattern().captures(stripped) {
            let text = ordered.get(1).map(|m| m.as_str()).unwrap_or("");
            let mut run = parse_rich_blocks(text, resolver.as_deref_mut());
            let head = run.first().cloned().unwrap_or_else(|| paragraph_block(""));
            let extra: Vec<Block> = run.drain(1..).collect();
            push_list_item(
                &mut blocks,
                &mut list_stack,
                list_indent_level(line),
                Block::with_children("numbered_list", delta_data(delta_ops_of(&head)), extra),
            );
            i += 1;
            continue;
        }

        if let Some(quote_text) = stripped.strip_prefix("> ") {
            let run = parse_rich_blocks(quote_text, resolver.as_deref_mut());
            let mut run_iter = run.into_iter();
            let head = run_iter.next().unwrap_or_else(|| paragraph_block(""));
            let block =
                Block::with_children("quote", delta_data(delta_ops_of(&head)), run_iter.collect());
            push_non_list_block(&mut blocks, &mut list_stack, list_indent_level(line), block);
            i += 1;
            continue;
        }

        let level = list_indent_level(line);
        while matches!(list_stack.last(), Some(&(top, _)) if top >= level) {
            list_stack.pop();
        }
        let paragraph = if !list_stack.is_empty() {
            line.trim_start_matches([' ', '\t'])
        } else {
            line
        };
        for block in parse_rich_blocks(paragraph, resolver.as_deref_mut()) {
            push_non_list_block(&mut blocks, &mut list_stack, level, block);
        }
        i += 1;
    }

    if in_code_block && !code_lines.is_empty() {
        let block = Block::new(
            "code",
            json!({
                "language": if code_language.is_empty() { "text" } else { &code_language },
                "delta": [{ "insert": code_lines.join("\n") }],
            }),
        );
        push_non_list_block(&mut blocks, &mut list_stack, code_level, block);
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

//! Render a decoded AppFlowy document as Markdown (port of export.py).

use serde_json::Value;

use crate::collab::{BlockEntry, Document};
use crate::delta::DeltaOp;

/// Escape the literal Markdown metacharacters in a plain (non-code) text run
/// so that re-importing yields the same characters. Applied to the raw insert
/// *before* the bold/italic/strikethrough wrappers are added (the wrappers
/// themselves must stay markers), which is the "only when unavoidable"
/// boundary: everything a plain run contains of `\`, `` ` ``, `*`, `[`, `]`,
/// `~` IS escaped — anything less re-imports as formatting. Exemptions:
/// - code spans: content is verbatim (import parses nothing inside);
/// - href URLs: copied raw (import reads them raw up to the first `)`);
/// - image alt/caption text: written verbatim (import extracts it by regex
///   before inline parsing, so escapes there would surface as literal
///   backslashes in captions);
/// - `|`: handled by the table-cell rule (`\|` <-> `|`), not here — a `|`
///   outside a table needs no escape on either side.
///
/// `parse_rich_text` un-escapes exactly the same set, so plain text with
/// literal metacharacters round-trips.
fn escape_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '\\' | '`' | '*' | '[' | ']' | '~') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Escape block-level markers at the start of an exported paragraph line, so
/// a paragraph whose text begins like block syntax (`# x`, `- x`, `> x`,
/// `1. x`, `---`, ```` ``` ````, `$$x$$`) does not re-import as that block
/// type. Only the line-initial position matters: once any non-marker text
/// has been emitted the rest of the line is inside a paragraph. Un-escaped on
/// import by the same `is_escapable` set as [`escape_inline`].
///
/// Callers pass the inline-escaped text of the FIRST op of the line when the
/// line starts at offset 0 of that op; markers past the first character of
/// the op cannot start a block, so nothing else needs treatment.
fn escape_leading_block_markers(text: &str) -> String {
    let bytes = text.as_bytes();
    let Some(&first) = bytes.first() else {
        return String::new();
    };
    // A numbered-list opener is `\d+\.`: escape the dot right after a run of
    // leading digits, keeping the digits readable (`1\.` stays text).
    if first.is_ascii_digit() {
        if let Some(dot) = bytes.iter().position(|&b| !b.is_ascii_digit()) {
            if bytes.get(dot) == Some(&b'.') {
                let mut out = String::with_capacity(text.len() + 1);
                out.push_str(&text[..dot]);
                out.push('\\');
                out.push_str(&text[dot..]);
                return out;
            }
        }
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 2);
    let escapes = |first: u8| match first {
        // `#`, `>` and `$` open headings / quotes / math; escaping them at
        // the line start makes the line plain text.
        b'#' | b'>' | b'$' => true,
        b'-' => matches!(bytes.get(1), None | Some(b' ') | Some(b'-')),
        _ => false,
    };
    if escapes(first) {
        out.push('\\');
    }
    out.push_str(text);
    out
}

/// Render a rich-text delta as inline Markdown. Wrapping order matches
/// Python: code -> bold -> italic -> strikethrough -> link. Every attribute
/// combination here re-imports to the same op attributes except bold +
/// italic on a single span (`***x***`) — the parser reads layered same-span
/// emphasis as separate runs, and there is no unambiguous Markdown spelling
/// for it; that combination was lossy on the regex parser too.
pub fn delta_to_markdown(delta: &[DeltaOp]) -> String {
    let mut out = String::new();
    for op in delta {
        let empty = serde_json::Map::new();
        let attrs = op.attributes.as_ref().unwrap_or(&empty);
        let mut text = if attrs.get("code").is_some_and(py_truthy) {
            format!("`{}`", op.insert)
        } else {
            escape_inline(&op.insert)
        };
        if out.is_empty() && !text.is_empty() {
            // The first run of an exported line: escape block markers at its
            // start so the line can't re-import as a different block type.
            text = escape_leading_block_markers(&text);
        }
        if attrs.get("bold").is_some_and(py_truthy) {
            text = format!("**{text}**");
        }
        if attrs.get("italic").is_some_and(py_truthy) {
            text = format!("*{text}*");
        }
        if attrs.get("strikethrough").is_some_and(py_truthy) {
            text = format!("~~{text}~~");
        }
        if let Some(href) = attrs.get("href").filter(|v| py_truthy(v)) {
            text = format!("[{text}]({})", json_to_display(href));
        }
        out.push_str(&text);
    }
    out
}

/// Python-style truthiness for a JSON value (None/False/0/""/empty are falsy).
fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Display a JSON value the way Python's f-string would for the scalar
/// shapes that appear in block data (strings stay raw).
fn json_to_display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `data.get(key)` as a display string, `""` when absent.
fn data_str(data: &Value, key: &str) -> String {
    data.get(key).map(json_to_display).unwrap_or_default()
}

pub fn document_to_markdown(document: &Document, title: Option<&str>) -> String {
    let mut renderer = Renderer {
        document,
        lines: Vec::new(),
    };
    if let Some(title) = title.filter(|t| !t.is_empty()) {
        renderer.lines.push(format!("# {title}"));
        renderer.lines.push(String::new());
    }

    let root_children = document
        .blocks
        .get(&document.page_id)
        .and_then(|b| document.children_map.get(b.children.as_str()))
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    renderer.render(root_children, 0);

    // Collapse trailing blank lines to a single newline.
    while matches!(renderer.lines.last(), Some(line) if line.is_empty()) {
        renderer.lines.pop();
    }
    let mut out = renderer.lines.join("\n");
    out.push('\n');
    out
}

struct Renderer<'a> {
    document: &'a Document,
    lines: Vec<String>,
}

impl<'a> Renderer<'a> {
    fn children_of(&self, block: &'a BlockEntry) -> &'a [String] {
        self.document
            .children_map
            .get(block.children.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn block_text(&self, block: &BlockEntry) -> String {
        if let Some(external_id) = block.external_id.as_deref().filter(|id| !id.is_empty()) {
            if let Some(delta) = self.document.text_map.get(external_id) {
                return delta_to_markdown(delta);
            }
        }
        if let Some(delta) = block.data.get("delta").and_then(Value::as_array) {
            if let Ok(delta) = serde_json::from_value::<Vec<DeltaOp>>(Value::Array(delta.clone())) {
                return delta_to_markdown(&delta);
            }
        }
        String::new()
    }

    fn render(&mut self, block_ids: &'a [String], indent: usize) {
        let prefix = "  ".repeat(indent);
        let mut number = 0u32;
        for (i, block_id) in block_ids.iter().enumerate() {
            let Some(block) = self.document.blocks.get(block_id) else {
                continue;
            };
            let ty = block.ty.as_str();
            let data = &block.data;
            let text = self.block_text(block);
            number = if ty == "numbered_list" { number + 1 } else { 0 };

            match ty {
                "heading" => {
                    let level = heading_level(data);
                    self.lines
                        .push(format!("{prefix}{} {text}", "#".repeat(level)));
                    self.lines.push(String::new());
                }
                "bulleted_list" => self.lines.push(format!("{prefix}- {text}")),
                "numbered_list" => self.lines.push(format!("{prefix}{number}. {text}")),
                "todo_list" => {
                    let mark = if data.get("checked").is_some_and(py_truthy) {
                        "x"
                    } else {
                        " "
                    };
                    self.lines.push(format!("{prefix}- [{mark}] {text}"));
                }
                "quote" => {
                    self.lines.push(format!("{prefix}> {text}"));
                    self.lines.push(String::new());
                }
                "code" => {
                    let language = data
                        .get("language")
                        .filter(|v| py_truthy(v))
                        .map(json_to_display)
                        .unwrap_or_default();
                    self.lines.push(format!("{prefix}```{language}"));
                    self.lines
                        .extend(text.split('\n').map(|line| format!("{prefix}{line}")));
                    self.lines.push(format!("{prefix}```"));
                    self.lines.push(String::new());
                }
                "divider" => {
                    // `***` instead of `---`: a document whose FIRST block is a
                    // divider exports with a leading `---`, which the importer
                    // can mistake for YAML front matter and swallow up to the
                    // next `---`. `***` is an equally valid divider that never
                    // opens front matter.
                    self.lines.push(format!("{prefix}***"));
                    self.lines.push(String::new());
                }
                "image" => {
                    let url = data_str(data, "url");
                    let caption = data
                        .get("caption")
                        .filter(|v| py_truthy(v))
                        .map(json_to_display)
                        .unwrap_or_default();
                    self.lines.push(format!("{prefix}![{caption}]({url})"));
                    self.lines.push(String::new());
                }
                "callout" => {
                    let callout = match data.get("icon").filter(|v| py_truthy(v)) {
                        Some(icon) => format!("{} {text}", json_to_display(icon))
                            .trim()
                            .to_string(),
                        None => text.clone(),
                    };
                    self.lines.push(format!("{prefix}> {callout}"));
                    self.lines.push(String::new());
                }
                "simple_table" => {
                    let table = self.render_simple_table(block);
                    if table.is_empty() {
                        self.lines
                            .push(format!("{prefix}<!-- unsupported block: {ty} -->"));
                        self.lines.push(String::new());
                    } else {
                        for row in table {
                            self.lines.push(format!("{prefix}{row}"));
                        }
                        self.lines.push(String::new());
                    }
                    // rows/cells consumed here; skip generic child recursion
                    continue;
                }
                "math_equation" => {
                    let formula = match data.get("formula").filter(|v| py_truthy(v)) {
                        Some(f) => json_to_display(f),
                        None => text.clone(),
                    };
                    let formula = formula.trim();
                    if !formula.is_empty() {
                        self.lines.push(format!("{prefix}$$"));
                        self.lines
                            .extend(formula.split('\n').map(|line| format!("{prefix}{line}")));
                        self.lines.push(format!("{prefix}$$"));
                        self.lines.push(String::new());
                    }
                }
                // root container; children rendered below
                "page" => {}
                _ if !text.is_empty() => {
                    self.lines.push(format!("{prefix}{text}"));
                    self.lines.push(String::new());
                }
                // empty paragraph -> blank separator already handled
                "paragraph" => {}
                _ => {
                    // A block type we don't render (e.g. table/grid). Leave a
                    // visible marker so its presence isn't silently lost.
                    self.lines
                        .push(format!("{prefix}<!-- unsupported block: {ty} -->"));
                    self.lines.push(String::new());
                }
            }

            let child_ids = self.children_of(block);
            let is_list = matches!(ty, "bulleted_list" | "numbered_list" | "todo_list");
            if !child_ids.is_empty() {
                self.render(child_ids, if is_list { indent + 1 } else { indent });
            }
            if is_list {
                let next = block_ids
                    .get(i + 1)
                    .and_then(|id| self.document.blocks.get(id));
                if next.is_none_or(|b| b.ty != ty) {
                    self.lines.push(String::new());
                }
            }
        }
    }

    /// Render an AppFlowy simple_table (table -> rows -> cells) as a GFM
    /// table. The first row is the header; pipes/newlines in cell text are
    /// neutralized so they don't break the table.
    fn render_simple_table(&self, table_block: &'a BlockEntry) -> Vec<String> {
        let mut rows: Vec<Vec<String>> = Vec::new();
        for row_id in self.children_of(table_block) {
            let Some(row) = self.document.blocks.get(row_id) else {
                continue;
            };
            if row.ty != "simple_table_row" {
                continue;
            }
            let mut cells: Vec<String> = Vec::new();
            for cell_id in self.children_of(row) {
                let Some(cell) = self.document.blocks.get(cell_id) else {
                    continue;
                };
                let mut parts: Vec<String> = Vec::new();
                let own = self.block_text(cell);
                if !own.is_empty() {
                    parts.push(own);
                }
                for content_id in self.children_of(cell) {
                    if let Some(content) = self.document.blocks.get(content_id) {
                        let piece = self.block_text(content);
                        if !piece.is_empty() {
                            parts.push(piece);
                        }
                    }
                }
                let text = parts
                    .join(" ")
                    .replace('|', "\\|")
                    .replace('\n', " ")
                    .trim()
                    .to_string();
                cells.push(text);
            }
            rows.push(cells);
        }

        let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
        if rows.is_empty() || ncols == 0 {
            return Vec::new();
        }

        let fmt = |cells: &[String]| {
            let padded: Vec<&str> = cells
                .iter()
                .map(String::as_str)
                .chain(std::iter::repeat(""))
                .take(ncols)
                .collect();
            format!("| {} |", padded.join(" | "))
        };
        let mut out = vec![
            fmt(&rows[0]),
            format!("| {} |", vec!["---"; ncols].join(" | ")),
        ];
        out.extend(rows[1..].iter().map(|r| fmt(r)));
        out
    }
}

/// Python: `min(max(int(data.get("level", 1) or 1), 1), 6)` — falsy levels
/// (missing, 0, None) mean 1; floats truncate like `int()`.
fn heading_level(data: &Value) -> usize {
    let raw = match data.get("level") {
        Some(v) if py_truthy(v) => v,
        _ => return 1,
    };
    let level = match raw {
        Value::Number(n) => n.as_f64().unwrap_or(1.0) as i64,
        Value::Bool(true) => 1,
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(1),
        _ => 1,
    };
    level.clamp(1, 6) as usize
}

/// Make a document title safe for use as a file name.
pub fn sanitize_filename(name: &str, fallback: &str) -> String {
    // Drop control chars (\r \n \t ...) — a title containing one produces an
    // invalid path and would otherwise crash the whole export (OSError 22).
    let cleaned: String = name
        .chars()
        .filter(|ch| *ch as u32 >= 32)
        .map(|ch| if "/\\:*?\"<>|".contains(ch) { '-' } else { ch })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.');
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::document_to_markdown;
    use crate::collab::{BlockEntry, Document};
    use crate::markdown::{parse_markdown_to_blocks, Block};

    fn entry(ty: &str, text: &str, children: &str) -> BlockEntry {
        BlockEntry {
            ty: ty.to_string(),
            data: json!({ "delta": [{ "insert": text }] }),
            children: children.to_string(),
            external_id: None,
        }
    }

    fn list_document(extra_children: Vec<(&str, BlockEntry)>) -> Document {
        let mut blocks = HashMap::from([
            ("page".to_string(), entry("page", "", "root-children")),
            (
                "item".to_string(),
                entry("bulleted_list", "item", "item-children"),
            ),
            (
                "sub".to_string(),
                entry("bulleted_list", "sub", "sub-children"),
            ),
            (
                "next".to_string(),
                entry("bulleted_list", "next", "next-children"),
            ),
        ]);
        let mut item_children = Vec::new();
        for (id, block) in extra_children {
            blocks.insert(id.to_string(), block);
            item_children.push(id.to_string());
        }
        item_children.push("sub".to_string());

        Document {
            page_id: "page".to_string(),
            blocks,
            children_map: HashMap::from([
                (
                    "root-children".to_string(),
                    vec!["item".to_string(), "next".to_string()],
                ),
                ("item-children".to_string(), item_children),
            ]),
            text_map: HashMap::new(),
        }
    }

    fn text(block: &Block) -> &str {
        block.data["delta"][0]["insert"].as_str().unwrap()
    }

    #[test]
    fn list_paragraph_and_nested_list_round_trip_with_top_level_sibling() {
        let document = list_document(vec![(
            "paragraph",
            entry("paragraph", "under item", "paragraph-children"),
        )]);

        let markdown = document_to_markdown(&document, None);
        let blocks = parse_markdown_to_blocks(&markdown, None);

        assert_eq!(blocks.len(), 2, "markdown:\n{markdown}");
        assert_eq!(blocks[0].ty, "bulleted_list");
        assert_eq!(text(&blocks[0]), "item");
        assert_eq!(blocks[0].children.len(), 2);
        assert_eq!(blocks[0].children[0].ty, "paragraph");
        assert_eq!(text(&blocks[0].children[0]), "under item");
        assert_eq!(blocks[0].children[1].ty, "bulleted_list");
        assert_eq!(text(&blocks[0].children[1]), "sub");
        assert_eq!(blocks[1].ty, "bulleted_list");
        assert_eq!(text(&blocks[1]), "next");
    }

    #[test]
    fn list_code_and_nested_list_round_trip_with_top_level_sibling() {
        let mut code = entry("code", "let x = 1;\nprintln!(\"{x}\");", "code-children");
        code.data["language"] = json!("rust");
        let document = list_document(vec![("code", code)]);

        let markdown = document_to_markdown(&document, None);
        let blocks = parse_markdown_to_blocks(&markdown, None);

        assert!(markdown.contains("  let x = 1;\n  println!(\"{x}\");\n  ```"));
        assert_eq!(blocks.len(), 2, "markdown:\n{markdown}");
        assert_eq!(blocks[0].children.len(), 2);
        assert_eq!(blocks[0].children[0].ty, "code");
        assert_eq!(blocks[0].children[0].data["language"], "rust");
        assert_eq!(
            text(&blocks[0].children[0]),
            "let x = 1;\nprintln!(\"{x}\");"
        );
        assert_eq!(blocks[0].children[1].ty, "bulleted_list");
        assert_eq!(text(&blocks[0].children[1]), "sub");
        assert_eq!(blocks[1].ty, "bulleted_list");
        assert_eq!(text(&blocks[1]), "next");
    }

    #[test]
    fn unindented_paragraph_ends_list_while_nested_lists_still_nest() {
        let blocks =
            parse_markdown_to_blocks("- item\n  under item\n  - sub\nafter list\n- next\n", None);

        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].ty, "bulleted_list");
        assert_eq!(blocks[0].children.len(), 2);
        assert_eq!(blocks[0].children[0].ty, "paragraph");
        assert_eq!(text(&blocks[0].children[0]), "under item");
        assert_eq!(blocks[0].children[1].ty, "bulleted_list");
        assert_eq!(text(&blocks[0].children[1]), "sub");
        assert_eq!(blocks[1].ty, "paragraph");
        assert_eq!(text(&blocks[1]), "after list");
        assert_eq!(blocks[2].ty, "bulleted_list");
        assert_eq!(text(&blocks[2]), "next");

        let standalone = parse_markdown_to_blocks("  standalone indent", None);
        assert_eq!(text(&standalone[0]), "  standalone indent");
    }
}

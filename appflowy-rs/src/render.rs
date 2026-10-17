//! Render a decoded AppFlowy document as Markdown (port of export.py).

use serde_json::Value;

use crate::collab::{BlockEntry, Document};
use crate::delta::DeltaOp;

/// Render a rich-text delta as inline Markdown. Wrapping order matches
/// Python: code -> bold -> italic -> strikethrough -> link.
pub fn delta_to_markdown(delta: &[DeltaOp]) -> String {
    let mut out = String::new();
    for op in delta {
        let mut text = op.insert.clone();
        let empty = serde_json::Map::new();
        let attrs = op.attributes.as_ref().unwrap_or(&empty);
        if attrs.get("code").is_some_and(py_truthy) {
            text = format!("`{text}`");
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
                    self.lines.push(format!("{prefix}{} {text}", "#".repeat(level)));
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
                    self.lines.push(text.clone());
                    self.lines.push("```".to_string());
                    self.lines.push(String::new());
                }
                "divider" => {
                    self.lines.push("---".to_string());
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
                        Some(icon) => format!("{} {text}", json_to_display(icon)).trim().to_string(),
                        None => text.clone(),
                    };
                    self.lines.push(format!("{prefix}> {callout}"));
                    self.lines.push(String::new());
                }
                "simple_table" => {
                    let table = self.render_simple_table(block);
                    if table.is_empty() {
                        self.lines.push(format!("{prefix}<!-- unsupported block: {ty} -->"));
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
                        self.lines.push(formula.to_string());
                        self.lines.push("$$".to_string());
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
                    self.lines.push(format!("{prefix}<!-- unsupported block: {ty} -->"));
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
        let mut out = vec![fmt(&rows[0]), format!("| {} |", vec!["---"; ncols].join(" | "))];
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

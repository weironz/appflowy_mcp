//! Port of `tests/test_markdown.py`: Markdown -> AppFlowy block conversion.
//! Assertions compare serialized blocks (`serde_json::Value`) against the
//! Python dict expectations.

use appflowy_mcp::markdown::{
    parse_content_to_blocks, parse_markdown_to_blocks, parse_plain_text_to_blocks, parse_rich_text,
    Block,
};
use serde_json::{json, Value};

fn to_json(blocks: Vec<Block>) -> Value {
    serde_json::to_value(blocks).unwrap()
}

#[test]
fn test_blank_lines_are_not_emitted_as_blocks() {
    // Regression: blank lines between paragraphs must not become empty
    // paragraph blocks (imported docs were littered with blank lines).
    let md = "# Title\n\nPara one.\n\n\nPara two.\n\n- a\n- b\n";
    let blocks = parse_markdown_to_blocks(md, None);
    let types: Vec<&str> = blocks.iter().map(|b| b.ty.as_str()).collect();
    assert_eq!(
        types,
        [
            "heading",
            "paragraph",
            "paragraph",
            "bulleted_list",
            "bulleted_list"
        ]
    );
    for block in &blocks {
        let delta = block.data.get("delta").and_then(Value::as_array);
        match delta {
            Some(d) if !d.is_empty() => {
                for op in d {
                    assert!(
                        op.get("insert")
                            .and_then(Value::as_str)
                            .is_some_and(|s| !s.is_empty()),
                        "empty insert in {block:?}"
                    );
                }
            }
            _ => panic!("block has empty delta: {block:?}"),
        }
    }
}

#[test]
fn test_blank_lines_inside_code_blocks_are_preserved() {
    let md = "```python\nline1\n\nline3\n```";
    let blocks = parse_markdown_to_blocks(md, None);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].ty, "code");
    assert_eq!(
        blocks[0].data["delta"][0]["insert"],
        json!("line1\n\nline3")
    );
    assert_eq!(blocks[0].data["language"], json!("python"));
}

#[test]
fn test_headings_lists_quotes_and_dividers() {
    let md = "# H1\n## H2\n### H3\n---\n1. first\n- [ ] todo\n- [x] done\n> quoted\n";
    let blocks = parse_markdown_to_blocks(md, None);
    let types: Vec<&str> = blocks.iter().map(|b| b.ty.as_str()).collect();
    assert_eq!(
        types,
        [
            "heading",
            "heading",
            "heading",
            "divider",
            "numbered_list",
            "todo_list",
            "todo_list",
            "quote"
        ]
    );
    let levels: Vec<Value> = blocks
        .iter()
        .take(3)
        .map(|b| b.data.get("level").cloned().unwrap_or(Value::Null))
        .collect();
    assert_eq!(levels, vec![json!(1), json!(2), json!(3)]);
    assert_eq!(blocks[5].data["checked"], json!(false));
    assert_eq!(blocks[6].data["checked"], json!(true));
}

#[test]
fn test_inline_formatting() {
    let deltas = parse_rich_text("**bold** and `code` and [link](https://x.example)");
    let json = serde_json::to_value(&deltas).unwrap();
    let ops = json.as_array().unwrap();
    assert!(ops.contains(&json!({"insert": "bold", "attributes": {"bold": true}})));
    assert!(ops.contains(&json!({"insert": "code", "attributes": {"code": true}})));
    assert!(ops.contains(&json!({
        "insert": "link",
        "attributes": {"href": "https://x.example"}
    })));
    assert_eq!(
        json,
        json!([
            {"insert": "bold", "attributes": {"bold": true}},
            {"insert": " and "},
            {"insert": "code", "attributes": {"code": true}},
            {"insert": " and "},
            {"insert": "link", "attributes": {"href": "https://x.example"}},
        ])
    );
}

#[test]
fn test_plain_text_keeps_blank_lines() {
    // plain_text format is literal: blank lines stay as empty paragraphs.
    let blocks = parse_plain_text_to_blocks("a\n\nb");
    let inserts: Vec<Value> = blocks
        .iter()
        .map(|b| b.data["delta"][0]["insert"].clone())
        .collect();
    assert_eq!(inserts, vec![json!("a"), json!(""), json!("b")]);
}

// ---------------------------------------------------------------------------
// Additional coverage for block types the Python tests don't exercise
// (tables, math, front matter, inline images, nested lists, resolver).
// ---------------------------------------------------------------------------

#[test]
fn test_table_block_with_escaped_pipes() {
    let md = "| a | b \\| c |\n| :-- | --: |\n| **x** | y |\n";
    let blocks = parse_markdown_to_blocks(md, None);
    assert_eq!(
        to_json(blocks),
        json!([{
            "type": "simple_table",
            "data": {
                "enable_header_row": true,
                "enable_header_column": false,
                "column_widths": {"0": 150, "1": 150},
            },
            "children": [
                {
                    "type": "simple_table_row",
                    "data": {},
                    "children": [
                        {
                            "type": "simple_table_cell",
                            "data": {},
                            "children": [{"type": "paragraph", "data": {"delta": [{"insert": "a"}]}}],
                        },
                        {
                            "type": "simple_table_cell",
                            "data": {},
                            "children": [{"type": "paragraph", "data": {"delta": [{"insert": "b | c"}]}}],
                        },
                    ],
                },
                {
                    "type": "simple_table_row",
                    "data": {},
                    "children": [
                        {
                            "type": "simple_table_cell",
                            "data": {},
                            "children": [{"type": "paragraph", "data": {"delta": [
                                {"insert": "x", "attributes": {"bold": true}}
                            ]}}],
                        },
                        {
                            "type": "simple_table_cell",
                            "data": {},
                            "children": [{"type": "paragraph", "data": {"delta": [{"insert": "y"}]}}],
                        },
                    ],
                },
            ],
        }])
    );
}

#[test]
fn test_math_blocks_and_dividers() {
    let md = "$$\\int_0^1 x dx$$\n$$\nf(x) = y\n$$\n***\n";
    let blocks = parse_markdown_to_blocks(md, None);
    assert_eq!(
        to_json(blocks),
        json!([
            {"type": "math_equation", "data": {"formula": "\\int_0^1 x dx"}},
            {"type": "math_equation", "data": {"formula": "f(x) = y"}},
            {"type": "divider", "data": {}},
        ])
    );
}

#[test]
fn test_front_matter_is_stripped() {
    let md = "---\ntitle: hello\n---\n# Real heading\n";
    let blocks = parse_markdown_to_blocks(md, None);
    assert_eq!(
        to_json(blocks),
        json!([{"type": "heading", "data": {"level": 1, "delta": [{"insert": "Real heading"}]}}])
    );
}

#[test]
fn test_inline_images_split_paragraphs_and_resolver() {
    let md = "pre ![cap](https://img.example/a.png) post\n";
    let blocks = parse_markdown_to_blocks(md, None);
    assert_eq!(
        to_json(blocks),
        json!([
            {"type": "paragraph", "data": {"delta": [{"insert": "pre "}]}},
            {"type": "image", "data": {"url": "https://img.example/a.png", "caption": "cap"}},
            {"type": "paragraph", "data": {"delta": [{"insert": " post"}]}},
        ])
    );

    let mut resolve = |url: &str, _caption: &str| format!("cdn:{url}");
    let resolved =
        parse_markdown_to_blocks("![cap](https://img.example/a.png)\n", Some(&mut resolve));
    assert_eq!(
        to_json(resolved),
        json!([{"type": "image", "data": {"url": "cdn:https://img.example/a.png", "caption": "cap"}}])
    );
}

#[test]
fn test_nested_bulleted_lists() {
    // More-indented items nest under the nearest less-indented open item.
    let md = "- a\n  - b\n    - c\n- d\n";
    let blocks = parse_markdown_to_blocks(md, None);
    assert_eq!(
        to_json(blocks),
        json!([
            {
                "type": "bulleted_list",
                "data": {"delta": [{"insert": "a"}]},
                "children": [{
                    "type": "bulleted_list",
                    "data": {"delta": [{"insert": "b"}]},
                    "children": [{
                        "type": "bulleted_list",
                        "data": {"delta": [{"insert": "c"}]},
                    }],
                }],
            },
            {"type": "bulleted_list", "data": {"delta": [{"insert": "d"}]}},
        ])
    );
}

#[test]
fn test_unclosed_code_block_is_emitted() {
    let md = "```rust\nfn main() {}\n";
    let blocks = parse_markdown_to_blocks(md, None);
    assert_eq!(
        to_json(blocks),
        json!([{
            "type": "code",
            "data": {"language": "rust", "delta": [{"insert": "fn main() {}"}]},
        }])
    );
}

#[test]
fn test_parse_content_to_blocks_dispatch() {
    assert!(parse_content_to_blocks("# h", " Markdown ").is_ok());
    assert!(parse_content_to_blocks("# h", "md").is_ok());
    assert!(parse_content_to_blocks("# h", "PLAINTEXT").is_ok());
    let err = parse_content_to_blocks("# h", "html").unwrap_err();
    assert_eq!(err, "content_format must be 'markdown' or 'plain_text'.");
}

// ---------------------------------------------------------------------------
// Remaining parse cases from tests/test_export.py (heading levels 4-6, list
// nesting boundaries, front matter / divider edge cases, table padding).
// ---------------------------------------------------------------------------

#[test]
fn test_headings_h4_h6_import() {
    let blocks = parse_markdown_to_blocks("#### H4\n##### H5\n###### H6", None);
    let levels: Vec<Value> = blocks
        .iter()
        .filter(|b| b.ty == "heading")
        .map(|b| b.data["level"].clone())
        .collect();
    assert_eq!(levels, vec![json!(4), json!(5), json!(6)]);
}

#[test]
fn test_flat_bulleted_list_not_nested() {
    let blocks = parse_markdown_to_blocks("- a\n- b\n- c", None);
    assert_eq!(
        to_json(blocks),
        json!([
            {"type": "bulleted_list", "data": {"delta": [{"insert": "a"}]}},
            {"type": "bulleted_list", "data": {"delta": [{"insert": "b"}]}},
            {"type": "bulleted_list", "data": {"delta": [{"insert": "c"}]}},
        ])
    );
}

#[test]
fn test_non_list_line_ends_nesting() {
    let blocks = parse_markdown_to_blocks("- a\n  - b\n\n# Heading\n\n- x\n  - y", None);
    assert_eq!(
        to_json(blocks),
        json!([
            {
                "type": "bulleted_list",
                "data": {"delta": [{"insert": "a"}]},
                "children": [{
                    "type": "bulleted_list",
                    "data": {"delta": [{"insert": "b"}]},
                }],
            },
            {"type": "heading", "data": {"level": 1, "delta": [{"insert": "Heading"}]}},
            {
                "type": "bulleted_list",
                "data": {"delta": [{"insert": "x"}]},
                "children": [{
                    "type": "bulleted_list",
                    "data": {"delta": [{"insert": "y"}]},
                }],
            },
        ])
    );
}

#[test]
fn test_math_block_multiline() {
    let blocks = parse_markdown_to_blocks("$$\nx = 1\ny = 2\n$$", None);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].ty, "math_equation");
    assert_eq!(blocks[0].data["formula"], json!("x = 1\ny = 2"));
}

#[test]
fn test_front_matter_without_close_is_divider() {
    // A leading "---" with no closing fence is not front matter.
    let blocks = parse_markdown_to_blocks("---\nnot front matter\n", None);
    assert_eq!(blocks[0].ty, "divider");
}

#[test]
fn test_mid_document_dashes_still_divider() {
    let blocks = parse_markdown_to_blocks("# Body\n\nsome text\n\n---\n\nmore text", None);
    let types: Vec<&str> = blocks.iter().map(|b| b.ty.as_str()).collect();
    assert!(types.contains(&"divider"));
    // No front matter was stripped: the heading text survives first.
    assert_eq!(blocks[0].data["delta"], json!([{"insert": "Body"}]));
}

#[test]
fn test_lone_dashes_not_table() {
    // A separator-looking line with no preceding header row is just a divider.
    let blocks = parse_markdown_to_blocks("text\n\n---\n\nmore", None);
    let types: Vec<&str> = blocks.iter().map(|b| b.ty.as_str()).collect();
    assert!(!types.contains(&"simple_table"));
    assert!(types.contains(&"divider"));
}

#[test]
fn test_table_alignment_separators_and_padding() {
    let blocks = parse_markdown_to_blocks("| a | b | c |\n|:--|:-:|--:|\n| 1 | 2 |", None);
    let table = blocks[0].clone();
    assert_eq!(table.ty, "simple_table");
    assert_eq!(
        table.data["column_widths"],
        json!({"0": 150, "1": 150, "2": 150})
    );
    // The short row (2 cells) is padded with an empty third cell.
    let short_row = &table.children[1];
    assert_eq!(short_row.children.len(), 3);
    assert_eq!(
        short_row.children[2].children[0].data["delta"],
        json!([{"insert": ""}])
    );
}

// ---------------------------------------------------------------------------
// Inline tokenizer: nested formatting, backslash escapes, and round-trips.
// ---------------------------------------------------------------------------

#[test]
fn test_nested_inline_containers() {
    // Bold containing a link: `**[t](u)**` merges the container and inner
    // marks onto one op (old single-regex behavior parsed this as bold
    // literal text "[t](u)").
    assert_eq!(
        serde_json::to_value(parse_rich_text("**[t](https://x.example)**")).unwrap(),
        json!([{"insert": "t", "attributes": {"bold": true, "href": "https://x.example"}}])
    );
    // Link containing bold (the export-side wrapping order): same op.
    assert_eq!(
        serde_json::to_value(parse_rich_text("[**t**](https://x.example)")).unwrap(),
        json!([{"insert": "t", "attributes": {"bold": true, "href": "https://x.example"}}])
    );
    // Bold containing italic.
    assert_eq!(
        serde_json::to_value(parse_rich_text("**a *b* c**")).unwrap(),
        json!([
            {"insert": "a ", "attributes": {"bold": true}},
            {"insert": "b", "attributes": {"bold": true, "italic": true}},
            {"insert": " c", "attributes": {"bold": true}},
        ])
    );
    // Bold containing strikethrough.
    assert_eq!(
        serde_json::to_value(parse_rich_text("**~~s~~**")).unwrap(),
        json!([{"insert": "s", "attributes": {"bold": true, "strikethrough": true}}])
    );
    // A mismatched inner marker inside a container stays literal.
    assert_eq!(
        serde_json::to_value(parse_rich_text("**a *b**")).unwrap(),
        json!([{"insert": "a *b", "attributes": {"bold": true}}])
    );
}

#[test]
fn test_repeated_italic_markers_pair_nearest_first() {
    // Documented rule ("先开先闭"): same-kind markers pair with the nearest
    // following closer rather than nesting, so `*bold *inner* tail*` is two
    // italic runs with plain text between, matching CommonMark only in
    // spirit — see the tokenizer comment in src/markdown.rs.
    assert_eq!(
        serde_json::to_value(parse_rich_text("*bold *inner* tail*")).unwrap(),
        json!([
            {"insert": "bold ", "attributes": {"italic": true}},
            {"insert": "inner"},
            {"insert": " tail", "attributes": {"italic": true}},
        ])
    );
}

#[test]
fn test_backslash_escapes_parse_as_literal() {
    assert_eq!(
        serde_json::to_value(parse_rich_text("\\*not italic\\*")).unwrap(),
        json!([{"insert": "*not italic*"}])
    );
    assert_eq!(
        serde_json::to_value(parse_rich_text("\\[a\\] and \\\\ and \\`b\\`")).unwrap(),
        json!([{"insert": "[a] and \\ and `b`"}])
    );
    // Unrecognized escapes keep the backslash, and escapes are honored
    // inside container contents too.
    assert_eq!(
        serde_json::to_value(parse_rich_text("\\q **a \\* b**")).unwrap(),
        json!([
            {"insert": "\\q "},
            {"insert": "a * b", "attributes": {"bold": true}},
        ])
    );
}

#[test]
fn test_unclosed_markers_stay_literal() {
    for literal in ["**x", "*x", "~~x", "`x", "[x](", "[x]", "[x]()", "****"] {
        assert_eq!(
            serde_json::to_value(parse_rich_text(literal)).unwrap(),
            json!([{"insert": literal}]),
            "{literal:?} should stay literal"
        );
    }
    // Markers after a valid pair are literal too.
    assert_eq!(
        serde_json::to_value(parse_rich_text("**x** ok **y")).unwrap(),
        json!([
            {"insert": "x", "attributes": {"bold": true}},
            {"insert": " ok **y"},
        ])
    );
}

#[test]
fn test_code_span_content_is_verbatim() {
    // Nothing parses inside a code span: no marks, no escapes `.
    assert_eq!(
        serde_json::to_value(parse_rich_text("pre `**x** \\*y*` post")).unwrap(),
        json!([
            {"insert": "pre "},
            {"insert": "**x** \\*y*", "attributes": {"code": true}},
            {"insert": " post"},
        ])
    );
}

#[test]
fn test_roundtrip_plain_special_chars() {
    use appflowy_mcp::delta::DeltaOp;
    use appflowy_mcp::render::delta_to_markdown;
    let delta = vec![DeltaOp::plain("a*b [c] `d` \\e")];
    let md = delta_to_markdown(&delta);
    assert_eq!(md, "a\\*b \\[c\\] \\`d\\` \\\\e");
    assert_eq!(parse_rich_text(&md), delta);
}

#[test]
fn test_roundtrip_nested_attributes() {
    use appflowy_mcp::delta::DeltaOp;
    use appflowy_mcp::render::delta_to_markdown;
    let attrs = |pairs: &[(&str, Value)]| {
        serde_json::Map::from_iter(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())))
    };
    // bold + href
    let delta = vec![DeltaOp::with_attributes(
        "t",
        attrs(&[("bold", json!(true)), ("href", json!("https://u.example"))]),
    )];
    assert_eq!(parse_rich_text(&delta_to_markdown(&delta)), delta);
    // bold + strikethrough
    let delta = vec![DeltaOp::with_attributes(
        "x",
        attrs(&[("bold", json!(true)), ("strikethrough", json!(true))]),
    )];
    assert_eq!(parse_rich_text(&delta_to_markdown(&delta)), delta);
    // code + bold
    let delta = vec![DeltaOp::with_attributes(
        "a*b",
        attrs(&[("code", json!(true)), ("bold", json!(true))]),
    )];
    assert_eq!(parse_rich_text(&delta_to_markdown(&delta)), delta);
}

// ---------------------------------------------------------------------------
// Round-trip fidelity: block-marker escaping, link labels with ], dividers
// vs front matter, inline images in containers.
// ---------------------------------------------------------------------------

#[test]
fn test_roundtrip_leading_block_markers_stay_paragraph_text() {
    use appflowy_mcp::delta::DeltaOp;
    use appflowy_mcp::render::delta_to_markdown;
    // A paragraph whose text starts with a block marker must not re-import as
    // that block type; a bare ``` must not open a code fence.
    for text in [
        "# not a heading",
        "- not a list",
        "> not a quote",
        "1. not numbered",
        "---",
        "```",
        "```rust",
        "$$x$$",
    ] {
        let delta = vec![DeltaOp::plain(text)];
        let md = delta_to_markdown(&delta);
        let blocks = parse_markdown_to_blocks(&(md.clone() + "\n"), None);
        assert_eq!(
            blocks[0].ty, "paragraph",
            "{text:?} re-imported as {} (markdown: {md:?})",
            blocks[0].ty
        );
        assert_eq!(parse_rich_text(&md), delta, "{text:?} text changed");
    }
}

#[test]
fn test_roundtrip_link_label_ending_in_bracket() {
    use appflowy_mcp::delta::DeltaOp;
    use appflowy_mcp::render::delta_to_markdown;
    let attrs = serde_json::Map::from_iter(
        [("href", json!("https://u.example"))]
            .iter()
            .cloned()
            .map(|(k, v)| (k.to_string(), v)),
    );
    let delta = vec![DeltaOp::with_attributes("a]", attrs)];
    let md = delta_to_markdown(&delta);
    // The exported `[a\](url)` must re-import as the same link, and Pandoc's
    // identical escaping must work too.
    assert_eq!(md, "[a\\]](https://u.example)");
    assert_eq!(parse_rich_text(&md), delta);
    assert_eq!(parse_rich_text("[a\\]](https://u.example)"), delta);
}

#[test]
fn test_leading_divider_not_confused_with_front_matter() {
    // [divider, paragraph, divider, paragraph] — the exporter emits bare
    // `---` for dividers; only YAML-shaped bodies may be treated as front
    // matter, so "a" must survive.
    let md = "---\n\na\n\n---\n\nb\n";
    let blocks = parse_markdown_to_blocks(md, None);
    let types: Vec<&str> = blocks.iter().map(|b| b.ty.as_str()).collect();
    assert_eq!(types, ["divider", "paragraph", "divider", "paragraph"]);
    assert_eq!(blocks[1].data["delta"], json!([{"insert": "a"}]));
}

#[test]
fn test_inline_images_inside_list_and_heading() {
    // Images inside list items / headings break out as real image blocks
    // (previously flattened into `!` + link text).
    let blocks = parse_markdown_to_blocks("- ![logo](https://img.example/a.png)\n", None);
    assert_eq!(
        to_json(blocks),
        json!([{
            "type": "bulleted_list",
            "data": {"delta": [{"insert": ""}]},
            "children": [{
                "type": "image",
                "data": {"url": "https://img.example/a.png", "caption": "logo"},
            }],
        }])
    );

    let blocks = parse_markdown_to_blocks("# title ![a](https://img.example/b.png)\n", None);
    assert_eq!(
        to_json(blocks),
        json!([{
            "type": "heading",
            "data": {"level": 1, "delta": [{"insert": "title "}]},
            "children": [{
                "type": "image",
                "data": {"url": "https://img.example/b.png", "caption": "a"},
            }],
        }])
    );

    // Image inside a quote, with resolver applied. Text after the image
    // becomes a child paragraph of the quote (the delta can't span images).
    let mut resolve = |url: &str, _caption: &str| format!("cdn:{url}");
    let blocks = parse_markdown_to_blocks(
        "> see ![shot](https://img.example/c.png) here\n",
        Some(&mut resolve),
    );
    assert_eq!(
        to_json(blocks),
        json!([{
            "type": "quote",
            "data": {"delta": [{"insert": "see "}]},
            "children": [
                {
                    "type": "image",
                    "data": {"url": "cdn:https://img.example/c.png", "caption": "shot"},
                },
                {
                    "type": "paragraph",
                    "data": {"delta": [{"insert": " here"}]},
                },
            ],
        }])
    );
}

#[test]
fn test_indented_full_line_image_nests_under_list_item() {
    // The exporter renders an image child of a list item on its own indented
    // line (`- item` then `  ![alt](url)`); on import the image must stay
    // nested under that item instead of landing at the document top level
    // (BUGS.md #5).
    let blocks = parse_markdown_to_blocks("- item\n  ![alt](https://x.example/i.png)\n", None);
    assert_eq!(
        to_json(blocks),
        json!([{
            "type": "bulleted_list",
            "data": {"delta": [{"insert": "item"}]},
            "children": [{
                "type": "image",
                "data": {"url": "https://x.example/i.png", "caption": "alt"},
            }],
        }])
    );
}

#[test]
fn test_indented_image_nests_under_numbered_and_todo_items() {
    let blocks =
        parse_markdown_to_blocks("1. one\n  ![n](n.png)\n- [x] done\n  ![t](t.png)\n", None);
    assert_eq!(
        to_json(blocks),
        json!([
            {
                "type": "numbered_list",
                "data": {"delta": [{"insert": "one"}]},
                "children": [{
                    "type": "image",
                    "data": {"url": "n.png", "caption": "n"},
                }],
            },
            {
                "type": "todo_list",
                "data": {"checked": true, "delta": [{"insert": "done"}]},
                "children": [{
                    "type": "image",
                    "data": {"url": "t.png", "caption": "t"},
                }],
            },
        ])
    );
}

#[test]
fn test_list_items_after_a_nested_image_keep_nesting() {
    // The image must not push itself onto the list stack (images never
    // parent list items): items on either side of it still nest under their
    // real list parent.
    let blocks =
        parse_markdown_to_blocks("- outer\n  - inner\n  ![alt](i.png)\n  - inner2\n", None);
    assert_eq!(
        to_json(blocks),
        json!([{
            "type": "bulleted_list",
            "data": {"delta": [{"insert": "outer"}]},
            "children": [
                {
                    "type": "bulleted_list",
                    "data": {"delta": [{"insert": "inner"}]},
                },
                {
                    "type": "image",
                    "data": {"url": "i.png", "caption": "alt"},
                },
                {
                    "type": "bulleted_list",
                    "data": {"delta": [{"insert": "inner2"}]},
                },
            ],
        }])
    );
}

#[test]
fn test_unindented_image_closes_the_list() {
    // A full-line image at indent 0 is a document-level block, same as
    // before: the list ends here and the image does not nest.
    let blocks = parse_markdown_to_blocks("- item\n![alt](i.png)\n- next\n", None);
    let types: Vec<&str> = blocks.iter().map(|b| b.ty.as_str()).collect();
    assert_eq!(types, ["bulleted_list", "image", "bulleted_list"]);
}

#[test]
fn test_inline_image_syntax_in_code_span_is_verbatim() {
    // `` `![a](b)` `` inside a paragraph is code, not an image: the inline
    // tokenizer sees the code span first, so the image syntax stays text.
    let deltas = parse_rich_text("use `![a](b)` syntax");
    assert_eq!(
        serde_json::to_value(&deltas).unwrap(),
        json!([
            {"insert": "use "},
            {"insert": "![a](b)", "attributes": {"code": true}},
            {"insert": " syntax"},
        ])
    );
}

//! Offline tests for AppFlowy document -> Markdown export
//! (port of tests/test_export.py; the fixture `make_document` builds a
//! `Document` directly, and collab fixtures are built as real yjs docs via
//! yrs, encoded with `encode_state_as_update_v1`, then fed to
//! `decode_document`).

use std::collections::HashMap;

use serde_json::{json, Value};
use yrs::types::Attrs;
use yrs::{
    Any, ArrayPrelim, Doc, Map, MapPrelim, ReadTxn, StateVector, Text, TextPrelim, Transact,
};

use appflowy_mcp::collab::{decode_document, BlockEntry, Document};
use appflowy_mcp::delta::DeltaOp;
use appflowy_mcp::markdown::parse_markdown_to_blocks;
use appflowy_mcp::render::{delta_to_markdown, document_to_markdown, sanitize_filename};

// ---------------------------------------------------------------------------
// Plain-struct fixtures (port of make_document from test_export.py)
// ---------------------------------------------------------------------------

struct Spec {
    ty: &'static str,
    data: Value,
    delta: Option<Vec<DeltaOp>>,
    kids: Vec<Spec>,
}

fn spec(ty: &'static str, data: Value, delta: Option<Vec<DeltaOp>>, kids: Vec<Spec>) -> Spec {
    Spec {
        ty,
        data,
        delta,
        kids,
    }
}

/// Build a decoded `Document` from (ty, data, text_delta, children) specs.
fn make_document(specs: Vec<Spec>) -> Document {
    let mut blocks = HashMap::from([(
        "root".to_string(),
        BlockEntry {
            ty: "page".to_string(),
            data: json!({}),
            children: "c_root".to_string(),
            external_id: None,
        },
    )]);
    let mut children_map: HashMap<String, Vec<String>> = HashMap::from([("c_root".into(), vec![])]);
    let mut text_map = HashMap::new();

    fn add(
        spec: Spec,
        parent_children: &str,
        blocks: &mut HashMap<String, BlockEntry>,
        children_map: &mut HashMap<String, Vec<String>>,
        text_map: &mut HashMap<String, Vec<DeltaOp>>,
    ) {
        let bid = format!("b{}", blocks.len());
        blocks.insert(
            bid.clone(),
            BlockEntry {
                ty: spec.ty.to_string(),
                data: spec.data,
                children: format!("c_{bid}"),
                external_id: spec.delta.as_ref().map(|_| format!("t_{bid}")),
            },
        );
        children_map.insert(format!("c_{bid}"), vec![]);
        if let Some(delta) = spec.delta {
            text_map.insert(format!("t_{bid}"), delta);
        }
        children_map
            .get_mut(parent_children)
            .expect("parent children id")
            .push(bid.clone());
        for kid in spec.kids {
            add(kid, &format!("c_{bid}"), blocks, children_map, text_map);
        }
    }

    for spec in specs {
        add(
            spec,
            "c_root",
            &mut blocks,
            &mut children_map,
            &mut text_map,
        );
    }
    Document {
        page_id: "root".to_string(),
        blocks,
        children_map,
        text_map,
    }
}

fn bold_attrs() -> serde_json::Map<String, Value> {
    serde_json::Map::from_iter([("bold".to_string(), json!(true))])
}

// ---------------------------------------------------------------------------
// Rendering tests
// ---------------------------------------------------------------------------

#[test]
fn test_delta_inline_formatting() {
    let delta = vec![
        DeltaOp::plain("a "),
        DeltaOp::with_attributes("b", bold_attrs()),
        DeltaOp::plain(" "),
        DeltaOp::with_attributes(
            "c",
            serde_json::Map::from_iter([("italic".to_string(), json!(true))]),
        ),
        DeltaOp::plain(" "),
        DeltaOp::with_attributes(
            "d",
            serde_json::Map::from_iter([("code".to_string(), json!(true))]),
        ),
        DeltaOp::plain(" "),
        DeltaOp::with_attributes(
            "e",
            serde_json::Map::from_iter([("strikethrough".to_string(), json!(true))]),
        ),
        DeltaOp::plain(" "),
        DeltaOp::with_attributes(
            "f",
            serde_json::Map::from_iter([("href".to_string(), json!("https://x.example"))]),
        ),
    ];
    assert_eq!(
        delta_to_markdown(&delta),
        "a **b** *c* `d` ~~e~~ [f](https://x.example)"
    );
}

#[test]
fn test_delta_stacked_attributes_wrap_in_python_order() {
    // A single op can stack several marks; wrapping order is code, bold,
    // italic, strikethrough, link.
    let attrs = serde_json::Map::from_iter([
        ("bold".to_string(), json!(true)),
        ("href".to_string(), json!("https://x.example")),
    ]);
    let delta = vec![DeltaOp::with_attributes("x", attrs)];
    assert_eq!(delta_to_markdown(&delta), "[**x**](https://x.example)");
}

#[test]
fn test_document_to_markdown_block_types() {
    let doc = make_document(vec![
        spec(
            "heading",
            json!({"level": 2}),
            Some(vec![DeltaOp::plain("Head")]),
            vec![],
        ),
        spec(
            "paragraph",
            json!({}),
            Some(vec![DeltaOp::plain("para")]),
            vec![],
        ),
        spec(
            "bulleted_list",
            json!({}),
            Some(vec![DeltaOp::plain("item")]),
            vec![spec(
                "bulleted_list",
                json!({}),
                Some(vec![DeltaOp::plain("nested")]),
                vec![],
            )],
        ),
        spec(
            "numbered_list",
            json!({}),
            Some(vec![DeltaOp::plain("one")]),
            vec![],
        ),
        spec(
            "numbered_list",
            json!({}),
            Some(vec![DeltaOp::plain("two")]),
            vec![],
        ),
        spec(
            "todo_list",
            json!({"checked": true}),
            Some(vec![DeltaOp::plain("done")]),
            vec![],
        ),
        spec(
            "quote",
            json!({}),
            Some(vec![DeltaOp::plain("quoted")]),
            vec![],
        ),
        spec(
            "code",
            json!({"language": "python"}),
            Some(vec![DeltaOp::plain("print(1)")]),
            vec![],
        ),
        spec("divider", json!({}), None, vec![]),
        spec(
            "image",
            json!({"url": "https://x/i.png", "caption": "cap"}),
            None,
            vec![],
        ),
    ]);
    let md = document_to_markdown(&doc, Some("Title"));
    assert!(md.starts_with("# Title\n"));
    assert!(md.contains("## Head"));
    assert!(md.contains("- item\n  - nested"));
    assert!(md.contains("1. one\n2. two"));
    assert!(md.contains("- [x] done"));
    assert!(md.contains("> quoted"));
    assert!(md.contains("```python\nprint(1)\n```"));
    // Dividers export as `***` (not `---`): a divider as the first block must
    // not look like YAML front matter on re-import.
    assert!(md.contains("***"));
    assert!(md.contains("![cap](https://x/i.png)"));
}

#[test]
fn test_list_images_survive_export_import_round_trip() {
    // The exporter renders an image child of a list item on its own indented
    // line; on re-import it must stay nested under that item instead of
    // landing at the document top level after the list (BUGS.md #5).
    let doc = make_document(vec![
        spec(
            "bulleted_list",
            json!({}),
            Some(vec![DeltaOp::plain("bullet")]),
            vec![spec(
                "image",
                json!({"url": "https://x.example/b.png", "caption": "b"}),
                None,
                vec![],
            )],
        ),
        spec(
            "numbered_list",
            json!({}),
            Some(vec![DeltaOp::plain("one")]),
            vec![spec(
                "image",
                json!({"url": "https://x.example/n.png", "caption": "n"}),
                None,
                vec![],
            )],
        ),
        spec(
            "todo_list",
            json!({"checked": true}),
            Some(vec![DeltaOp::plain("done")]),
            vec![spec(
                "image",
                json!({"url": "https://x.example/t.png", "caption": "t"}),
                None,
                vec![],
            )],
        ),
        spec(
            "bulleted_list",
            json!({}),
            Some(vec![DeltaOp::plain("outer")]),
            vec![spec(
                "bulleted_list",
                json!({}),
                Some(vec![DeltaOp::plain("inner")]),
                vec![spec(
                    "image",
                    json!({"url": "https://x.example/i.png", "caption": "i"}),
                    None,
                    vec![],
                )],
            )],
        ),
    ]);
    let md = document_to_markdown(&doc, None);
    let blocks = parse_markdown_to_blocks(&md, None);
    assert_eq!(
        serde_json::to_value(&blocks).unwrap(),
        json!([
            {
                "type": "bulleted_list",
                "data": {"delta": [{"insert": "bullet"}]},
                "children": [{
                    "type": "image",
                    "data": {"url": "https://x.example/b.png", "caption": "b"},
                }],
            },
            {
                "type": "numbered_list",
                "data": {"delta": [{"insert": "one"}]},
                "children": [{
                    "type": "image",
                    "data": {"url": "https://x.example/n.png", "caption": "n"},
                }],
            },
            {
                "type": "todo_list",
                "data": {"checked": true, "delta": [{"insert": "done"}]},
                "children": [{
                    "type": "image",
                    "data": {"url": "https://x.example/t.png", "caption": "t"},
                }],
            },
            {
                "type": "bulleted_list",
                "data": {"delta": [{"insert": "outer"}]},
                "children": [{
                    "type": "bulleted_list",
                    "data": {"delta": [{"insert": "inner"}]},
                    "children": [{
                        "type": "image",
                        "data": {"url": "https://x.example/i.png", "caption": "i"},
                    }],
                }],
            },
        ])
    );
}

#[test]
fn test_unsupported_block_marked_on_export() {
    let doc = make_document(vec![spec("table", json!({}), None, vec![])]);
    let out = document_to_markdown(&doc, None);
    assert!(out.contains("unsupported block: table"));
}

#[test]
fn test_simple_table_rendered_as_gfm() {
    fn cell(t: &str) -> Spec {
        spec(
            "simple_table_cell",
            json!({}),
            None,
            vec![spec(
                "paragraph",
                json!({}),
                Some(vec![DeltaOp::plain(t)]),
                vec![],
            )],
        )
    }
    let table = spec(
        "simple_table",
        json!({}),
        None,
        vec![
            spec(
                "simple_table_row",
                json!({}),
                None,
                vec![cell("A"), cell("B")],
            ),
            spec(
                "simple_table_row",
                json!({}),
                None,
                vec![cell("1"), cell("2")],
            ),
        ],
    );
    let out = document_to_markdown(&make_document(vec![table]), None);
    assert!(out.contains("| A | B |"));
    assert!(out.contains("| --- | --- |"));
    assert!(out.contains("| 1 | 2 |"));
    assert!(!out.contains("unsupported block"));
}

#[test]
fn test_simple_table_pipes_and_short_rows() {
    fn cell(t: &str) -> Spec {
        spec(
            "simple_table_cell",
            json!({}),
            None,
            vec![spec(
                "paragraph",
                json!({}),
                Some(vec![DeltaOp::plain(t)]),
                vec![],
            )],
        )
    }
    let table = spec(
        "simple_table",
        json!({}),
        None,
        vec![
            spec(
                "simple_table_row",
                json!({}),
                None,
                vec![cell("a|b"), cell("x")],
            ),
            spec("simple_table_row", json!({}), None, vec![cell("1")]), // short row padded
        ],
    );
    let out = document_to_markdown(&make_document(vec![table]), None);
    assert!(out.contains("| a\\|b | x |"));
    assert!(out.contains("| 1 |  |"));
}

#[test]
fn test_math_equation_rendered() {
    let doc = make_document(vec![spec(
        "math_equation",
        json!({"formula": "E=mc^2"}),
        None,
        vec![],
    )]);
    let out = document_to_markdown(&doc, None);
    assert!(out.contains("$$") && out.contains("E=mc^2"));
}

#[test]
fn test_sanitize_filename() {
    assert_eq!(sanitize_filename("a/b:c*d?\"e\"", "fb"), "a-b-c-d--e-");
    assert_eq!(sanitize_filename("", "fb"), "fb");
    assert_eq!(sanitize_filename("...", "fb"), "fb");
}

#[test]
fn test_sanitize_filename_strips_control_chars() {
    assert_eq!(sanitize_filename("LLM\r-标准版", "fb"), "LLM-标准版");
    assert_eq!(sanitize_filename("a/b:c*d", "fb"), "a-b-c-d");
    assert_eq!(sanitize_filename("\r\n\t", "fb"), "fb");
    assert_eq!(sanitize_filename("", "fb"), "fb");
}

// ---------------------------------------------------------------------------
// Collab-decoding tests: build real yjs docs with yrs, encode, then decode.
// Python's decode_document fixtures are hand-assembled pycrdt docs with the
// same structure (root map "data" -> "document" {page_id, blocks, meta}).
// ---------------------------------------------------------------------------

fn yrs_attrs(pairs: &[(&str, bool)]) -> Attrs {
    pairs
        .iter()
        .map(|(k, v)| ((*k).into(), Any::Bool(*v)))
        .collect()
}

/// Insert a block into the blocks map; `extra` fills additional keys.
fn insert_block(
    blocks: &yrs::MapRef,
    txn: &mut yrs::TransactionMut,
    id: &str,
    ty: &str,
    data_json: &str,
    children: Option<&str>,
    external_id: Option<&str>,
) {
    let block = blocks.insert(txn, id, MapPrelim::default());
    block.insert(txn, "ty", ty);
    block.insert(txn, "data", data_json);
    if let Some(children) = children {
        block.insert(txn, "children", children);
    }
    if let Some(external_id) = external_id {
        block.insert(txn, "external_id", external_id);
    }
}

#[test]
fn test_decode_document_roundtrip() {
    let doc = Doc::new();
    let data = doc.get_or_insert_map("data");
    let mut txn = doc.transact_mut();

    let document = data.insert(&mut txn, "document", MapPrelim::default());
    document.insert(&mut txn, "page_id", "root");

    let blocks = document.insert(&mut txn, "blocks", MapPrelim::default());
    insert_block(
        &blocks,
        &mut txn,
        "root",
        "page",
        "{}",
        Some("c_root"),
        None,
    );
    insert_block(
        &blocks,
        &mut txn,
        "b1",
        "heading",
        r#"{"level": 2}"#,
        Some("c_b1"),
        Some("t_b1"),
    );
    insert_block(
        &blocks,
        &mut txn,
        "b2",
        "paragraph",
        "{}",
        Some("c_b2"),
        Some("t_b2"),
    );

    let meta = document.insert(&mut txn, "meta", MapPrelim::default());
    let children_map = meta.insert(&mut txn, "children_map", MapPrelim::default());
    let c_root: ArrayPrelim = ["b1", "b2"].into_iter().collect();
    children_map.insert(&mut txn, "c_root", c_root);
    children_map.insert(&mut txn, "c_b1", ArrayPrelim::default());
    children_map.insert(&mut txn, "c_b2", ArrayPrelim::default());

    let text_map = meta.insert(&mut txn, "text_map", MapPrelim::default());
    text_map.insert(&mut txn, "t_b1", TextPrelim::new("Head"));
    let body = text_map.insert(&mut txn, "t_b2", TextPrelim::new(""));
    body.insert_with_attributes(&mut txn, 0, "plain ", Attrs::default());
    body.insert_with_attributes(&mut txn, 6, "bold", yrs_attrs(&[("bold", true)]));
    body.insert_with_attributes(
        &mut txn,
        10,
        "link",
        [("href".into(), Any::String("https://x.example".into()))]
            .into_iter()
            .collect(),
    );

    drop(txn);
    let update = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());

    let decoded = decode_document(&update).expect("decode");
    assert_eq!(decoded.page_id, "root");

    let b1 = &decoded.blocks["b1"];
    assert_eq!(b1.ty, "heading");
    assert_eq!(b1.data, json!({"level": 2})); // data JSON string parsed
    assert_eq!(b1.children, "c_b1");
    assert_eq!(b1.external_id.as_deref(), Some("t_b1"));

    let root = &decoded.blocks["root"];
    assert_eq!(root.ty, "page");
    assert_eq!(root.external_id, None);

    assert_eq!(decoded.children_map["c_root"], vec!["b1", "b2"]);
    assert!(decoded.children_map["c_b1"].is_empty());

    // Text diffs preserve formatting.
    assert_eq!(decoded.text_map["t_b1"], vec![DeltaOp::plain("Head")]);
    assert_eq!(decoded.text_map["t_b2"].len(), 3);
    assert_eq!(decoded.text_map["t_b2"][0], DeltaOp::plain("plain "));
    assert_eq!(
        decoded.text_map["t_b2"][1],
        DeltaOp::with_attributes("bold", bold_attrs())
    );
    assert_eq!(decoded.text_map["t_b2"][2].insert, "link");

    // The decoded document renders with inline formatting preserved.
    let md = document_to_markdown(&decoded, Some("T"));
    assert!(md.starts_with("# T\n"));
    assert!(md.contains("## Head"));
    assert!(md.contains("plain **bold**[link](https://x.example)"));
}

#[test]
fn test_decode_block_defaults_and_bad_json() {
    let doc = Doc::new();
    let data = doc.get_or_insert_map("data");
    let mut txn = doc.transact_mut();

    let document = data.insert(&mut txn, "document", MapPrelim::default());
    document.insert(&mut txn, "page_id", "root");

    let blocks = document.insert(&mut txn, "blocks", MapPrelim::default());
    // No children/external_id keys -> "" and None.
    let bare = blocks.insert(&mut txn, "root", MapPrelim::default());
    bare.insert(&mut txn, "ty", "page");
    // Unparseable data JSON falls back to {}.
    bare.insert(&mut txn, "data", "not-json{{{");
    // A block with *no keys at all* -> ty "" etc.
    blocks.insert(&mut txn, "b1", MapPrelim::default());

    let meta = document.insert(&mut txn, "meta", MapPrelim::default());
    meta.insert(&mut txn, "children_map", MapPrelim::default());

    drop(txn);
    let update = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());

    let decoded = decode_document(&update).expect("decode");
    let root = &decoded.blocks["root"];
    assert_eq!(root.data, json!({})); // bad JSON -> {}
    assert_eq!(root.children, "");
    assert_eq!(root.external_id, None);

    let b1 = &decoded.blocks["b1"];
    assert_eq!(b1.ty, "");
    assert_eq!(b1.data, json!({}));
    assert_eq!(b1.children, "");
    assert_eq!(b1.external_id, None);
}

#[test]
fn test_decode_plain_string_text_map() {
    // The lossy collab/json fallback stores text as plain strings, not YText.
    let doc = Doc::new();
    let data = doc.get_or_insert_map("data");
    let mut txn = doc.transact_mut();

    let document = data.insert(&mut txn, "document", MapPrelim::default());
    document.insert(&mut txn, "page_id", "root");
    let blocks = document.insert(&mut txn, "blocks", MapPrelim::default());
    insert_block(
        &blocks,
        &mut txn,
        "root",
        "page",
        "{}",
        Some("c_root"),
        None,
    );
    insert_block(
        &blocks,
        &mut txn,
        "b1",
        "paragraph",
        "{}",
        None,
        Some("t_b1"),
    );

    let meta = document.insert(&mut txn, "meta", MapPrelim::default());
    let children_map = meta.insert(&mut txn, "children_map", MapPrelim::default());
    let c_root: ArrayPrelim = ["b1"].into_iter().collect();
    children_map.insert(&mut txn, "c_root", c_root);

    let text_map = meta.insert(&mut txn, "text_map", MapPrelim::default());
    text_map.insert(&mut txn, "t_b1", "flat text");
    text_map.insert(&mut txn, "t_empty", ""); // falsy -> []

    drop(txn);
    let update = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());

    let decoded = decode_document(&update).expect("decode");
    assert_eq!(decoded.text_map["t_b1"], vec![DeltaOp::plain("flat text")]);
    assert_eq!(decoded.text_map["t_empty"], Vec::<DeltaOp>::new());

    let md = document_to_markdown(&decoded, None);
    assert!(md.contains("flat text"));
}

#[test]
fn test_decode_invalid_update_errors() {
    // All-zero bytes decode as an empty update; decoding succeeds but the
    // doc has no data/document map, so decode_document must fail cleanly
    // (and, in particular, must not hang on the missing root map).
    assert!(decode_document(&[0u8, 0u8, 0u8]).is_err());
}

// Python's decode_document also accepted list[int]; in Rust the caller
// already hands us &[u8], so there is nothing to port there.

// ---------------------------------------------------------------------------
// Export -> reimport roundtrip (test_export_reimports_cleanly)
// ---------------------------------------------------------------------------

#[test]
fn test_export_reimports_cleanly() {
    use appflowy_mcp::markdown::parse_markdown_to_blocks;
    let doc = make_document(vec![
        spec(
            "heading",
            json!({"level": 1}),
            Some(vec![DeltaOp::plain("H")]),
            vec![],
        ),
        spec(
            "paragraph",
            json!({}),
            Some(vec![
                DeltaOp::plain("body "),
                DeltaOp::with_attributes("bold", bold_attrs()),
            ]),
            vec![],
        ),
        spec(
            "todo_list",
            json!({"checked": false}),
            Some(vec![DeltaOp::plain("task")]),
            vec![],
        ),
    ]);
    let md = document_to_markdown(&doc, None);
    let blocks = parse_markdown_to_blocks(&md, None);
    let types: Vec<&str> = blocks.iter().map(|b| b.ty.as_str()).collect();
    assert_eq!(types, ["heading", "paragraph", "todo_list"]);
    let delta = blocks[1].data["delta"].as_array().expect("paragraph delta");
    assert!(delta.contains(&json!({"insert": "bold", "attributes": {"bold": true}})));
}

// ---------------------------------------------------------------------------
// Server-side fetch tools against httpmock (port of the monkeypatched
// test_export.py cases): real yjs updates stand in for the patched
// decode_document/document_to_markdown.
// ---------------------------------------------------------------------------

use std::time::Duration;

use appflowy_mcp::client::AppFlowyClient;
use appflowy_mcp::tools::export::{
    export_views_to_directory, fetch_collab_markdown, fetch_page_markdown,
};
use httpmock::prelude::*;

/// Wrap a yjs update as the `encoded_collab` list-of-ints payload the HTTP
/// API returns.
fn collab_payload(update: &[u8]) -> Value {
    json!(update.iter().map(|b| *b as u64).collect::<Vec<u64>>())
}

/// Encode a document collab with one paragraph per string into a yjs update.
/// `&[]` yields a document with no content blocks (renders as just its title).
fn collab_update(texts: &[&str]) -> Vec<u8> {
    let doc = Doc::new();
    let data = doc.get_or_insert_map("data");
    let mut txn = doc.transact_mut();

    let document = data.insert(&mut txn, "document", MapPrelim::default());
    document.insert(&mut txn, "page_id", "root");
    let blocks = document.insert(&mut txn, "blocks", MapPrelim::default());
    insert_block(
        &blocks,
        &mut txn,
        "root",
        "page",
        "{}",
        Some("c_root"),
        None,
    );
    let ids: Vec<String> = (0..texts.len()).map(|i| format!("b{i}")).collect();
    for id in &ids {
        insert_block(
            &blocks,
            &mut txn,
            id,
            "paragraph",
            "{}",
            Some(&format!("c_{id}")),
            Some(&format!("t_{id}")),
        );
    }

    let meta = document.insert(&mut txn, "meta", MapPrelim::default());
    let children_map = meta.insert(&mut txn, "children_map", MapPrelim::default());
    let root_children: ArrayPrelim = ids.iter().cloned().collect();
    children_map.insert(&mut txn, "c_root", root_children);
    for id in &ids {
        children_map.insert(&mut txn, format!("c_{id}"), ArrayPrelim::default());
    }
    let text_map = meta.insert(&mut txn, "text_map", MapPrelim::default());
    for (id, text) in ids.iter().zip(texts) {
        text_map.insert(&mut txn, format!("t_{id}"), TextPrelim::new(*text));
    }

    drop(txn);
    let update = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    update
}

async fn httpmock_client(server: &MockServer) -> AppFlowyClient {
    let client = AppFlowyClient::new(Some("e".into()), Some("p".into()), server.base_url());
    client.set_access_token(Some("t".into())).await;
    client.set_refresh_token(Some("r".into())).await;
    client
}

/// Page-view response envelope: the client unwraps one `data` layer, and
/// `fetch_page_view_markdown` reads `data.encoded_collab` below that.
fn page_view_body(update: &[u8]) -> Value {
    json!({"data": {"data": {"encoded_collab": collab_payload(update)}}})
}

fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "appflowy-export-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Poll until `mock` has been hit `n` times (mock-swap pattern from
/// tests/client.rs: httpmock has no response sequences for one path).
async fn wait_hits(mock: &httpmock::Mock<'_>, n: usize) {
    for _ in 0..250 {
        if mock.calls_async().await >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("mock was not hit {n} time(s) within 5s");
}

/// Port of `test_fetch_page_markdown_retries_on_collab_not_found`: a transient
/// 'Collab not found' is retried, not dropped. Python's fake errored twice
/// then succeeded; with httpmock that is two mock swaps during the backoff.
async fn mock_collab_not_found(server: &MockServer) -> httpmock::Mock<'_> {
    server
        .mock_async(|when, then| {
            when.method(GET).path("/api/workspace/ws/page-view/vid");
            then.status(404).json_body(
                json!({"code": -2, "message": "Record not found:Collab not found (code -2)"}),
            );
        })
        .await
}

#[tokio::test]
async fn fetch_page_markdown_retries_on_collab_not_found() {
    let server = MockServer::start_async().await;
    let path = "/api/workspace/ws/page-view/vid";
    let boom1 = mock_collab_not_found(&server).await;
    let client = httpmock_client(&server).await;
    let pending = tokio::spawn({
        let client = client.clone();
        async move { fetch_page_markdown(&client, "ws", "vid", Some("T"), 2, 0.5).await }
    });

    wait_hits(&boom1, 1).await;
    boom1.delete_async().await;
    let boom2 = mock_collab_not_found(&server).await;
    wait_hits(&boom2, 1).await;
    boom2.delete_async().await;
    let ok = server
        .mock_async(move |when, then| {
            when.method(GET).path(path);
            then.status(200)
                .json_body(page_view_body(&collab_update(&["body"])));
        })
        .await;

    let md = pending.await.unwrap().expect("fetch succeeds on attempt 3");
    assert!(md.starts_with("# T"));
    assert!(md.contains("body"));
    ok.assert_calls_async(1).await;
}

/// Port of `test_fetch_page_markdown_retries_on_empty_body`: a page that
/// transiently decodes to only its title is retried. Python's fake returned
/// two empty bodies before the real one; here one empty + one swap (the retry
/// decision itself is what the test exercises).
#[tokio::test]
async fn fetch_page_markdown_retries_on_empty_body() {
    let server = MockServer::start_async().await;
    let empty = server
        .mock_async(|when, then| {
            when.method(GET).path("/api/workspace/ws/page-view/vid");
            then.status(200)
                .json_body(page_view_body(&collab_update(&[])));
        })
        .await;
    let client = httpmock_client(&server).await;
    let pending = tokio::spawn({
        let client = client.clone();
        async move { fetch_page_markdown(&client, "ws", "vid", Some("T"), 2, 0.5).await }
    });

    wait_hits(&empty, 1).await;
    empty.delete_async().await;
    let full = server
        .mock_async(|when, then| {
            when.method(GET).path("/api/workspace/ws/page-view/vid");
            then.status(200)
                .json_body(page_view_body(&collab_update(&["body"])));
        })
        .await;

    let md = pending.await.unwrap().expect("fetch retries empty body");
    assert!(md.contains("body"));
    full.assert_calls_async(1).await;
    // The empty-body mock was hit exactly once before the swap (wait_hits),
    // and only the swapped-in full-body mock served the successful attempt.
}

/// Port of `test_fetch_collab_markdown_tries_both_collab_types`: collab_type
/// is tried as 0, then "Document"; the second attempt's payload wins.
#[tokio::test]
async fn fetch_collab_markdown_tries_both_collab_types() {
    let server = MockServer::start_async().await;
    let invalid = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/api/workspace/v1/ws/collab/oid")
                .query_param("collab_type", "0");
            then.status(400)
                .json_body(json!({"code": 1001, "message": "invalid collab_type"}));
        })
        .await;
    let ok = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/api/workspace/v1/ws/collab/oid")
                .query_param("collab_type", "Document");
            then.status(200).json_body(
                json!({"data": {"encode_collab": {"doc_state": collab_payload(&collab_update(&["x"]))}}}),
            );
        })
        .await;
    let client = httpmock_client(&server).await;

    let (markdown, attempts) = fetch_collab_markdown(&client, "ws", "oid", Some("T")).await;
    let md = markdown.expect("Document collab_type succeeds");
    assert!(md.starts_with("# T"));
    assert!(md.contains("x"));
    // Only the collab_type=0 failure is logged, and it is logged *first* —
    // together with the successful return this pins the 0 -> "Document" order.
    assert_eq!(attempts.len(), 1);
    assert!(attempts[0].contains("collab_type=0"));
    invalid.assert_calls_async(1).await;
    ok.assert_calls_async(1).await;
}

/// Port of `test_page_markdown_falls_back_to_collab`: when page-view stays
/// 404 across retries, the raw collab endpoint recovers the content.
#[tokio::test]
async fn page_markdown_falls_back_to_raw_collab() {
    let server = MockServer::start_async().await;
    let page_view = server
        .mock_async(|when, then| {
            when.method(GET).path("/api/workspace/ws/page-view/vid");
            then.status(404).json_body(
                json!({"code": -2, "message": "Record not found:Collab not found (code -2)"}),
            );
        })
        .await;
    let collab = server
        .mock_async(|when, then| {
            when.method(GET).path("/api/workspace/v1/ws/collab/vid");
            then.status(200).json_body(json!({
                "data": {"encode_collab": {"doc_state": collab_payload(&collab_update(&["recovered"]))}}
            }));
        })
        .await;
    let client = httpmock_client(&server).await;

    let md = fetch_page_markdown(&client, "ws", "vid", Some("T"), 1, 0.0)
        .await
        .expect("raw collab fallback succeeds");
    assert!(md.starts_with("# T"));
    assert!(md.contains("recovered"));
    page_view.assert_calls_async(2).await; // retries=1 -> two page-view attempts
    collab.assert_calls_async(1).await; // first collab_type already works
}

/// Port of `test_export_prefetches_and_writes`: every document is fetched
/// exactly once (prefetch), the tree becomes space/Doc1.md,
/// space/Folder/README.md, space/Folder/Doc2.md, and no warnings occur.
#[tokio::test]
async fn export_prefetches_and_writes_directory() {
    let server = MockServer::start_async().await;
    let mut mocks = Vec::new();
    for (view_id, text) in [("d1", "body-d1"), ("d2", "body-d2"), ("f1", "body-f1")] {
        let update = collab_update(&[text]);
        mocks.push(
            server
                .mock_async(move |when, then| {
                    when.method(GET)
                        .path(format!("/api/workspace/ws/page-view/{view_id}"));
                    then.status(200).json_body(page_view_body(&update));
                })
                .await,
        );
    }
    let client = httpmock_client(&server).await;

    let views = json!([
        {"view_id": "s1", "name": "space", "is_space": true, "layout": 0, "children": [
            {"view_id": "d1", "name": "Doc1", "layout": 0, "children": []},
            {"view_id": "f1", "name": "Folder", "layout": 0, "children": [
                {"view_id": "d2", "name": "Doc2", "layout": 0, "children": []},
            ]},
        ]},
    ]);
    let dir = unique_temp_dir("tree");
    std::fs::create_dir_all(&dir).unwrap();
    let mut exported = Vec::new();
    let mut warnings = Vec::new();
    export_views_to_directory(
        &client,
        "ws",
        views.as_array().unwrap(),
        &dir,
        &mut exported,
        &mut warnings,
    )
    .await
    .unwrap();

    // Naming conventions: leaf -> <name>.md, page with children -> folder
    // whose own content is README.md.
    let doc1 = dir.join("space/Doc1.md");
    let readme = dir.join("space/Folder/README.md");
    let doc2 = dir.join("space/Folder/Doc2.md");
    for path in [&doc1, &readme, &doc2] {
        assert!(path.is_file(), "missing {}", path.display());
    }
    assert!(std::fs::read_to_string(&doc1).unwrap().contains("body-d1"));
    assert!(std::fs::read_to_string(&readme)
        .unwrap()
        .contains("body-f1"));
    assert!(std::fs::read_to_string(&doc2).unwrap().contains("body-d2"));
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert_eq!(exported.len(), 3);
    // Prefetch fetches each document exactly once (the walk reads the cache).
    for mock in &mocks {
        mock.assert_calls_async(1).await;
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Inline escaping + tokenizer round-trips (Markdown fidelity work).
// ---------------------------------------------------------------------------

fn attrs(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
    serde_json::Map::from_iter(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())))
}

#[test]
fn test_delta_escapes_plain_metacharacters() {
    use appflowy_mcp::markdown::parse_rich_text;
    // Plain text containing literal metacharacters is backslash-escaped so it
    // re-imports as the same characters instead of formatting.
    let delta = vec![DeltaOp::plain("a*b [c] `d` \\e")];
    let md = delta_to_markdown(&delta);
    assert_eq!(md, "a\\*b \\[c\\] \\`d\\` \\\\e");
    assert_eq!(parse_rich_text(&md), delta);
    // `|` is NOT escaped here — that is the table-cell rule's job.
    let delta = vec![DeltaOp::plain("a|b")];
    assert_eq!(delta_to_markdown(&delta), "a|b");
}

#[test]
fn test_delta_code_span_is_not_escaped_or_reparsed() {
    use appflowy_mcp::markdown::parse_rich_text;
    // Code span content is verbatim on both sides: marks and escapes inside
    // stay literal, and re-importing parses none of them.
    let delta = vec![DeltaOp::with_attributes(
        "a*b [c]",
        attrs(&[("code", json!(true))]),
    )];
    let md = delta_to_markdown(&delta);
    assert_eq!(md, "`a*b [c]`");
    assert_eq!(parse_rich_text(&md), delta);
    let delta = vec![DeltaOp::with_attributes(
        "a\\*`b",
        attrs(&[("code", json!(true))]),
    )];
    assert_eq!(delta_to_markdown(&delta), "`a\\*`b`");
}

#[test]
fn test_nested_bold_strike_roundtrip() {
    use appflowy_mcp::markdown::parse_rich_text;
    // Wrapping order code->bold->italic->strike->href puts strike outside
    // bold for a same-span combination; parsing must re-merge the nesting.
    let delta = vec![DeltaOp::with_attributes(
        "x",
        attrs(&[("bold", json!(true)), ("strikethrough", json!(true))]),
    )];
    let md = delta_to_markdown(&delta);
    assert_eq!(md, "~~**x**~~");
    assert_eq!(parse_rich_text(&md), delta);
    // The reverse nesting (`**~~x~~**`) imports to the same merged op.
    assert_eq!(parse_rich_text("**~~x~~**"), delta);
}

#[test]
fn test_bold_wrapping_link_roundtrip() {
    use appflowy_mcp::markdown::parse_rich_text;
    // A bold link is exported href-outside (`[**t**](u)`); both wrap orders
    // import to the same op.
    let delta = vec![DeltaOp::with_attributes(
        "t",
        attrs(&[("bold", json!(true)), ("href", json!("https://u.example"))]),
    )];
    let md = delta_to_markdown(&delta);
    assert_eq!(md, "[**t**](https://u.example)");
    assert_eq!(parse_rich_text(&md), delta);
    assert_eq!(parse_rich_text("**[t](https://u.example)**"), delta);
}

#[test]
fn test_table_cell_pipes_roundtrip() {
    use appflowy_mcp::markdown::parse_markdown_to_blocks;
    // Pipes inside a header cell are escaped on export and unescaped on
    // import (table rule, not the inline escape set).
    fn cell(t: &str) -> Spec {
        spec(
            "simple_table_cell",
            json!({}),
            None,
            vec![spec(
                "paragraph",
                json!({}),
                Some(vec![DeltaOp::plain(t)]),
                vec![],
            )],
        )
    }
    let table = spec(
        "simple_table",
        json!({}),
        None,
        vec![
            spec(
                "simple_table_row",
                json!({}),
                None,
                vec![cell("A|B"), cell("a*b")],
            ),
            spec(
                "simple_table_row",
                json!({}),
                None,
                vec![cell("1"), cell("2")],
            ),
        ],
    );
    let md = document_to_markdown(&make_document(vec![table]), None);
    assert!(md.contains("| A\\|B | a\\*b |"), "{md}");
    let blocks = parse_markdown_to_blocks(&md, None);
    assert_eq!(blocks[0].ty, "simple_table");
    let header = &blocks[0].children[0];
    let cell_text =
        |idx: usize| header.children[idx].children[0].data["delta"][0]["insert"].clone();
    assert_eq!(cell_text(0), json!("A|B"));
    assert_eq!(cell_text(1), json!("a*b"));
}

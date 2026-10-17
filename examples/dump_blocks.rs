//! Throwaway e2e helper: decode a yjs snapshot binary and print blocks.
fn main() {
    let path = std::env::args().nth(1).expect("path to snapshot binary");
    let bytes = std::fs::read(path).expect("read snapshot");
    let doc = appflowy_mcp::collab::decode_document(&bytes).expect("decode");
    let root = doc.children_map.get(
        doc.blocks.get(&doc.page_id).map(|b| b.children.as_str()).unwrap_or("")
    );
    let root_children: Vec<&str> = root.map(|v| v.iter().map(String::as_str).collect()).unwrap_or_default();
    for id in &root_children {
        if let Some(b) = doc.blocks.get(*id) {
            let text = b.external_id.as_deref().and_then(|ext| doc.text_map.get(ext)).map(|d| {
                d.iter().map(|op| op.insert.as_str()).collect::<String>()
            });
            println!("{}\t{}\t{:?}", id, b.ty, text);
        }
    }
}

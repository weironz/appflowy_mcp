//! `appflowy` binary entry point.
//!
//! Two personalities share this binary, selected by the executable's file
//! name (mirroring the Python package's two entry points): when argv[0]'s
//! file stem contains "appflowy-mcp" it runs the MCP server over stdio —
//! `appflowy-mcp` was the dedicated MCP entry point in Python, so copying
//! this binary under that name makes it the MCP server. Anything else runs
//! the `appflowy-cli` command-line client (which also offers `serve` for
//! the same MCP server).

#[tokio::main]
async fn main() {
    let argv0 = std::env::args().next().unwrap_or_default();
    let stem = std::path::Path::new(&argv0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_lowercase();

    let code = if stem.contains("appflowy-mcp") {
        match appflowy_rs::cli::serve().await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e:#}");
                1
            }
        }
    } else {
        appflowy_rs::cli::run().await
    };
    std::process::exit(code);
}

# Development

Standard cargo layout — the crate lives at the repository root (`src/`, `tests/`, `Cargo.toml`).

```bash
cargo test                 # full suite (unit + integration, mock servers, real yjs fixtures)
cargo clippy --all-targets # CI runs this with -D warnings
cargo build --release      # optimized single binary (LTO, stripped)
```

`Cargo.lock` is committed; CI builds with `--locked`.

### Module map

| Module | Role |
|---|---|
| `client.rs` | Async HTTP client: login/refresh, token store, race-safe 401 re-auth (refresh → login), 429/5xx retry with `Retry-After`, **one** central unwrap of the `{code, message, data}` envelope |
| `error.rs` | `Error::Api { message, status, code }` — status/code stay typed all the way to the MCP boundary; `Validation` maps to JSON-RPC `invalid_params` |
| `delta.rs` | `DeltaOp`, the shared rich-text delta shape |
| `markdown.rs` | Markdown → AppFlowy blocks (headings h1-h6, nested lists, tables, math, todo, quotes, code, front-matter, inline code/bold/italic/strike/link) |
| `collab.rs` | yjs collab decoding via [`yrs`](https://github.com/y-crdt/y-crdt) (root map `data` → blocks/meta/text_map) |
| `render.rs` | AppFlowy document → Markdown (incl. GFM tables), `sanitize_filename` |
| `importer.rs` | Directory/file import; per-page asset scanning, dedup and parallel upload (JoinSet + Semaphore), sequential page creation to keep sibling order deterministic |
| `server.rs` | `AppServer` (holds the client), startup best-effort login, merges all tool routers |
| `tools/*.rs` | One file per tool domain (auth, workspace, member, database, pages, content, publish, search, quicknotes, chat, upload, export); `tools/util.rs` has shared helpers incl. `map_tool_error` and `create_page_with_blocks` |
| `cli.rs` / `main.rs` | clap CLI + argv[0] dispatch (`appflowy-mcp` → serve) |

### Design notes (intentional differences from the Python version)

- **No per-tool auth or try/except.** The client re-authenticates on 401 itself (mutex + token-compare so a concurrent refresh is reused, not repeated). Tools return errors via `map_tool_error`, keeping HTTP status/code instead of flattening to strings — e.g. `list_published_views` checks `status() == Some(404)` rather than matching `"404"` in the message.
- **Validation before network.** Bad input is rejected with `invalid_params` before any request; it is never wrapped as an "API failure".
- **Envelope unwrapped once.** `client.request` returns `data` (or the whole body when there is no envelope) — nothing downstream touches `{code, message, data}` again.
- **Fetch failures keep traces.** `fetch_page_markdown` retries page-view, then falls back to the raw collab endpoint (both collab id types); a final failure includes every attempt (endpoint, attempt number, error) — previously these were silently swallowed.
- **Concurrency.** Export prefetch and per-page image upload are parallel (`JoinSet` + `Semaphore`, bounded at 8). `reorder_pages` builds an index map instead of repeated linear scans (`O(n²)` → `O(n)`).

### Adding a tool

1. Pick the domain file in `src/tools/` (or create one and register its router in `src/server.rs`).
2. Define the request struct: `derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)` with `#[serde(default)]`; field doc comments become the tool-input descriptions agents see.
3. Add the handler inside the domain file's `#[tool_router] impl AppServer` block:

```rust
#[tool(name = "appflowy_example", description = "What it does.")]
async fn appflowy_example(
    &self,
    Parameters(request): Parameters<ExampleRequest>,
) -> Result<Json<Value>, ErrorData> {
    // validate request fields first -> ErrorData::invalid_params
    let data = self
        .client
        .request(reqwest::Method::GET, "/api/...", None, None)
        .await
        .map_err(map_tool_error)?;
    Ok(Json(data))
}
```

4. `cargo test` and `cargo clippy --all-targets`; also verify with a stdio smoke (`tools/list`) when touching routing.

### Releasing

Bump `version` in `Cargo.toml`, commit, push `main`, then push a tag `vX.Y.Z`. `.github/workflows/release.yml` checks tag == crate version, runs tests + clippy, builds Windows/Linux/macOS (x64 & arm64) binaries, and attaches them to a new GitHub Release. There is no PyPI step anymore.


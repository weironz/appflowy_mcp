# AppFlowy MCP

An MCP server and CLI for [AppFlowy](https://github.com/appflowy-io/appflowy) with 114 tools covering workspaces, spaces, pages, page content, Markdown import and export, PDF/ZIP export, databases, rows, CSV/document import, search, members and invitations, page sharing, document comments, publishing, quick notes, notifications, file upload (incl. multipart), and AI chat & meeting transcription.

Since v0.8.0 the implementation is a single self-contained **Rust binary** — no Python or runtime dependencies. This repository *is* the Rust project (standard cargo layout); the original Python implementation was removed after the rewrite (see [Legacy Python](#legacy-python-implementation)).

## Installation

> **One-line installers**: Linux/macOS `curl -fsSL https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.sh | sh` · Windows PowerShell `irm https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.ps1 | iex` — details in [docs/installation.md](docs/installation.md).

### Prebuilt binaries

Download the binary for your platform from [GitHub Releases](https://github.com/weironz/appflowy_mcp/releases). One binary serves both entry points:

- `appflowy` — the CLI. Run `appflowy serve` to start the MCP server over stdio.
- Rename (or copy) the binary to `appflowy-mcp` (`.exe` on Windows) and it starts the MCP server immediately, mirroring the old Python `appflowy-mcp` entry point. The release ships both names for Windows.

### Build from source

Requires a Rust toolchain ([rustup](https://rustup.rs/)):

```bash
git clone https://github.com/weironz/appflowy_mcp.git
cd appflowy_mcp
cargo build --release
# binary: target/release/appflowy (appflowy.exe on Windows)
```

## MCP Client Setup

> 中文完整对接指南（Codex / Claude Code / Claude Desktop / Cursor，含 `--scope user` 的 Windows 坑与排错）: [docs/mcp-connect.md](docs/mcp-connect.md).

Point your client at the binary with the `serve` argument. Credentials come from the environment.

### Claude Code

```bash
claude mcp add appflowy \
  -e APPFLOWY_EMAIL=your-email \
  -e APPFLOWY_PASSWORD=your-password \
  -- /path/to/appflowy serve
```

### Codex

```bash
codex mcp add appflowy \
  --env APPFLOWY_EMAIL=your-email \
  --env APPFLOWY_PASSWORD=your-password \
  -- /path/to/appflowy serve
```

### Other MCP Clients

Any client that supports stdio MCP servers can use this JSON config (Claude Desktop, Cursor, Windsurf, etc.):

```json
{
  "mcpServers": {
    "appflowy": {
      "command": "/path/to/appflowy",
      "args": ["serve"],
      "env": {
        "APPFLOWY_EMAIL": "your-email",
        "APPFLOWY_PASSWORD": "your-password"
      }
    }
  }
}
```

If you renamed the binary to `appflowy-mcp`, omit the `args` entirely.

### Streamable HTTP

For remote or shared deployments the same server speaks Streamable HTTP:

```bash
appflowy serve --http 127.0.0.1:8000                # endpoint: http://127.0.0.1:8000/mcp
appflowy serve --http 0.0.0.0:8000 --allow-remote   # beyond loopback (read below)
```

Point HTTP-capable clients at `http://ADDR/mcp` (legacy session-mode clients and the newer stateless protocol are both negotiated automatically). Two guardrails are on by default: only loopback `Host` authorities are accepted (DNS-rebinding protection), and requests carrying a browser `Origin` are rejected. Binding a non-loopback address requires `--allow-remote`, which also disables the loopback host allowlist — do that only on a trusted network, because any reachable client can then drive all 114 tools with the server's AppFlowy credentials.

## Environment Variables

```text
APPFLOWY_EMAIL=your-email@example.com
APPFLOWY_PASSWORD=your-password
```

With these set, the server logs in once at startup and re-authenticates automatically when a token expires (thread-safe refresh). `appflowy_login` is still available to provide credentials at runtime.

For self-hosted AppFlowy, also set the base URL (defaults to `https://beta.appflowy.cloud`):

```text
APPFLOWY_BASE_URL=http://localhost:8000
```

All three variables can also live in a local `.env` file (loaded at startup).

## AppFlowy Structure

Most write operations need both a workspace and a parent view:

```text
workspace -> space -> page/database
```

To create a page inside a space, pass the workspace ID and use the space `view_id` as `parent_view_id`. Passing the workspace ID as a parent is rejected with an actionable error (a child of the workspace root would become a Space, not a document).

## Command-Line Interface

The same binary is a full CLI, aimed at scriptable workflows — backups via cron, bulk import, quick lookups. 中文完整命令参考见 [docs/cli.md](docs/cli.md).

```bash
appflowy login          # prompts for email/password; saves session tokens
appflowy logout
```

`login` saves the session tokens (never the password) to `~/.config/appflowy-cli/credentials.json` (mode 600 on Unix) and keeps them fresh across runs (GoTrue refresh-token rotation is persisted). The `APPFLOWY_EMAIL` / `APPFLOWY_PASSWORD` / `APPFLOWY_BASE_URL` environment variables take priority when set.

Addressing follows AppFlowy's hierarchy: pick a workspace once with `use` (or per command with `-w`), then address spaces and pages by PATH — `/`-separated names or UUIDs resolved level by level, case-insensitively:

```bash
appflowy use test                  # set the default workspace
appflowy workspaces                # list workspaces (* = default)
appflowy ls                        # spaces at the workspace root
appflowy ls demo                   # children of the demo space
appflowy tree demo                 # full subtree
appflowy search "query"

# export (Markdown, inline formatting preserved)
appflowy export demo/notes -o note.md   # leaf page -> one .md file
appflowy export demo -o ./demo          # space -> directory tree
appflowy export -o ./backup             # whole workspace

# import / create
appflowy import demo ./notes            # local dir -> page subtree
appflowy import demo note.md            # local file -> one page
echo "# Note" | appflowy save demo "Title"

appflowy -w Programming ls              # one-off workspace override
```

Every command accepts `--json` for machine-readable output. A nightly workspace backup is one cron line:

```cron
0 3 * * * APPFLOWY_EMAIL=... APPFLOWY_PASSWORD=... /path/to/appflowy -w <workspace> export -o ~/backups/appflowy-$(date +\%F)
```

## Tools

### Authentication

- `appflowy_login`
- `appflowy_refresh_token`

### Workspaces And Spaces

- `appflowy_list_workspaces`
- `appflowy_get_workspace_folder`
- `appflowy_list_spaces`
- `appflowy_create_space`
- `appflowy_update_space`

### Workspace Management

- `appflowy_create_workspace`
- `appflowy_update_workspace`
- `appflowy_delete_workspace`
- `appflowy_leave_workspace`
- `appflowy_get_workspace_settings`
- `appflowy_update_workspace_settings`
- `appflowy_get_workspace_usage`

Note: creating extra workspaces requires a paid AppFlowy Cloud plan; the free plan returns a workspace-limit error.

### Members And Invitations

- `appflowy_list_members`
- `appflowy_update_member`
- `appflowy_remove_members`
- `appflowy_invite_members`
- `appflowy_list_invitations`
- `appflowy_accept_invitation`

Member roles are `Owner`, `Member`, or `Guest`.

### Pages

- `appflowy_create_page`
- `appflowy_get_page`
- `appflowy_get_collab`
- `appflowy_update_page`
- `appflowy_move_page`
- `appflowy_reorder_pages`
- `appflowy_reorder_favorite`
- `appflowy_duplicate_page`
- `appflowy_move_page_to_trash`
- `appflowy_restore_page_from_trash`
- `appflowy_delete_page_from_trash`
- `appflowy_restore_all_from_trash`
- `appflowy_empty_trash`
- `appflowy_favorite_page`
- `appflowy_list_trash`
- `appflowy_list_favorites`
- `appflowy_list_recent`

AppFlowy has no bulk-sort endpoint; sibling order is controlled by the move endpoint's `prev_view_id`. `appflowy_reorder_pages` builds on it to reorder all direct children of a space or page in one call — pass an explicit `page_ids` order, or `sort_by` one of `name`, `created_at`, `last_edited_time` (with optional `descending`); pages already in place are skipped. `appflowy_reorder_favorite` reorders a page within the favorites list.

### Page Content

- `appflowy_save_page`
- `appflowy_append_page_content`
- `appflowy_append_text_to_page`
- `appflowy_append_blocks_to_page`
- `appflowy_create_markdown_page`
- `appflowy_append_markdown_to_page`
- `appflowy_import_markdown_file`
- `appflowy_import_markdown_directory`

Page-content support covers appending new document blocks, creating pages from Markdown, and appending Markdown to existing pages. AppFlowy exposes a high-level `append-block` endpoint, but not a matching high-level REST endpoint for deleting or editing arbitrary existing blocks. Page-level deletion through trash is supported.

When saving AI answers, notes, summaries, or generated content into AppFlowy, use `appflowy_save_page` by default. Its `content_format` field defaults to `markdown`, so agents do not need to ask users to say "save as Markdown" every time. Set `content_format` to `plain_text` only when the user explicitly wants the content preserved as literal plain text.

Use `appflowy_append_page_content` by default when adding AI-generated content to an existing page. It also treats content as Markdown unless `content_format` is set to `plain_text`.

Example paragraph block:

```json
{
  "type": "paragraph",
  "data": {
    "delta": [
      {
        "insert": "Hello from MCP"
      }
    ]
  }
}
```

Example Markdown page:

```json
{
  "parent_view_id": "space-or-page-view-id",
  "title": "Meeting Notes",
  "content": "# Meeting Notes\n\n- [ ] Follow up\n- **Important** decision\n\n```python\nprint(\"hello\")\n```",
  "content_format": "markdown"
}
```

Markdown conversion supports headings, paragraphs, dividers, bullet lists, numbered lists, todo lists, quotes, code blocks, GFM tables, math equations, image links, YAML front-matter stripping, and inline bold, italic, strikethrough, code, and links.

### Markdown Import

Use `appflowy_import_markdown_file` to import one local Markdown file as an AppFlowy page. Use `appflowy_import_markdown_directory` to recursively import a local folder tree.

Directory import maps local structure directly to AppFlowy:

```text
local folder -> AppFlowy page
local subfolder -> AppFlowy subpage
local .md/.markdown file -> AppFlowy page
```

`README.md` or `index.md` becomes the content of its folder page instead of a separate child page. Other Markdown files in the same folder become child pages. Hidden folders plus `.git`, `.hg`, `.svn`, `.idea`, `.vscode`, `node_modules`, and `__pycache__` are skipped.

Local image references are resolved relative to the Markdown file, uploaded to AppFlowy file storage in parallel (deduplicated per file), and replaced with AppFlowy file URLs:

```markdown
![Architecture](./images/architecture.png)
![Screenshot](../assets/screenshot.png)
```

Remote image URLs such as `https://...` are kept as-is. Missing local images are reported in the import result warnings, while other files continue importing.

Example directory import:

```json
{
  "parent_view_id": "space-or-page-view-id",
  "path": "D:\\notes",
  "upload_assets": true
}
```

Example single-file import:

```json
{
  "parent_view_id": "space-or-page-view-id",
  "path": "D:\\notes\\MCP\\config.md",
  "title": "MCP Config",
  "upload_assets": true
}
```

### Markdown Export

- `appflowy_export_page`
- `appflowy_export_space`
- `appflowy_export_workspace`

For Markdown there is no server-side export format, so these tools rebuild it from the page collab data. The raw yjs update returned by the page-view endpoint is decoded natively in Rust ([yrs](https://github.com/y-crdt/y-crdt), the reference implementation of Yjs), which preserves inline formatting (bold, italic, strikethrough, inline code, links) that the server's JSON conversion would flatten.

`appflowy_export_page` writes one page to a local `.md` file. `appflowy_export_space` exports a space (or any page subtree) as a directory tree that mirrors the import convention — a page with children becomes a folder whose own content is `README.md`, leaf pages become `.md` files. `appflowy_export_workspace` does the same for every space in the workspace. Page Markdown is prefetched concurrently (bounded at 8 workers) with per-page retry and fallback to the raw collab endpoint; pages that still fail are reported as warnings with full attempt traces instead of aborting the run. Non-document views (grids, boards, calendars) are skipped and reported in warnings, and the destination directory must be empty so existing files are never overwritten.

### Server-Side Export (PDF / ZIP)

- `appflowy_export_pdf` — newer commercial-cloud endpoint that renders a page (with nesting, databases and images) to PDF on the server and downloads the bytes to a local path; filename is recovered from `Content-Disposition` (RFC 6266). Higher fidelity than the Markdown rebuild for print/share use cases.
- `appflowy_export_workspace_zip` — owner-only full-workspace backup: returns a `task_id`; the server builds the ZIP asynchronously (with optional file attachments) and emails the owner a download link.

### Databases And Rows

- `appflowy_list_databases`
- `appflowy_get_database_fields`
- `appflowy_create_database_field`
- `appflowy_list_rows`
- `appflowy_get_row_details`
- `appflowy_create_row`
- `appflowy_upsert_row`
- `appflowy_get_updated_rows`

`appflowy_create_database_field` field types: 0=RichText, 1=Number, 2=DateTime, 3=SingleSelect, 4=MultiSelect, 5=Checkbox, 6=URL, 7=Checklist.

### Search

- `appflowy_search`
- `appflowy_search_summary`

Full-text search across a workspace. `appflowy_search_summary` follows up with the server's summary endpoint and returns cited summaries. Newly written content can take a short while to appear while the server indexes it.

### Publishing

- `appflowy_publish_page`
- `appflowy_unpublish_page`
- `appflowy_get_publish_namespace`
- `appflowy_set_publish_namespace`
- `appflowy_list_published_views`
- `appflowy_get_published_outline`
- `appflowy_get_published_comments`
- `appflowy_add_published_comment`
- `appflowy_get_published_reactions`
- `appflowy_add_published_reaction`
- `appflowy_duplicate_published_page`

`appflowy_publish_page` returns the publish info (namespace and publish name) for building the public URL. On older server deployments that lack the list endpoint, `appflowy_list_published_views` falls back to walking the workspace folder tree (detected via the structured 404 status, not error-string matching).

### Quick Notes

- `appflowy_list_quick_notes`
- `appflowy_create_quick_note`
- `appflowy_update_quick_note`
- `appflowy_delete_quick_note`

Quick note tools accept plain `text` (converted to paragraph blocks) or raw `data` JSON.

### AI Chat

- `appflowy_create_chat`
- `appflowy_delete_chat`
- `appflowy_get_chat_settings`
- `appflowy_update_chat_settings`
- `appflowy_list_chat_messages`
- `appflowy_chat_ask`

Create a chat with `appflowy_create_chat` (optionally passing `rag_ids`, page view ids the AI may use as context), then ask questions with `appflowy_chat_ask`, which posts the question and waits for the complete AI answer (non-streaming).

### AI Meeting Transcription

- `appflowy_meeting_transcribe`
- `appflowy_meeting_get_transcript`
- `appflowy_meeting_get_paragraphs`
- `appflowy_meeting_delete_transcript`
- `appflowy_meeting_usage`

`appflowy_meeting_transcribe` accepts a local audio file (uploaded via a presigned storage URL — the AppFlowy bearer token is never sent to storage) or an existing audio URL, then submits it for AI transcription returns a full transcript with empty text only when the audio is silent. Poll `appflowy_meeting_get_transcript` (status `queued` → `processing` → `completed`/`error`), then fetch speaker-labeled paragraphs with word-level timestamps.

### Document Comments

- `appflowy_list_inline_comments`
- `appflowy_add_inline_comment`
- `appflowy_resolve_inline_comment`
- `appflowy_delete_inline_comment`
- `appflowy_list_inline_comment_reactions`
- `appflowy_add_inline_comment_reaction`
- `appflowy_delete_inline_comment_reaction`
- `appflowy_notify_row_comment`

Inline annotations on a document page (distinct from published-page public comments) — the read/review loop for human-AI collaboration: an agent writes content, a reviewer leaves and resolves comments in the document. Deletion is a soft delete server-side (`is_deleted: true`); the v2 create endpoint falls back to the legacy one on older deployments (404/405).

### Page Sharing

- `appflowy_share_page`
- `appflowy_revoke_page_access`
- `appflowy_get_page_access_details`
- `appflowy_request_access`
- `appflowy_get_access_request`
- `appflowy_approve_access_request`
- `appflowy_list_shared_with_me`

Share a page with collaborators by email at an access level — `10` ReadOnly, `20` ReadAndComment, `30` ReadAndWrite, `50` FullAccess (verified against AppFlowy-Web's `AccessLevel` enum). `appflowy_get_page_access_details` uses the v2 endpoint with a structured-404 fallback to the legacy ancestor-walk variant. Distinct from publishing: sharing grants named accounts in-app access, publishing exposes a public link.

### CSV / Document Import

- `appflowy_import_csv`
- `appflowy_get_csv_import_status`
- `appflowy_cancel_csv_import`
- `appflowy_import_document`
- `appflowy_get_document_import_status`

`appflowy_import_csv` creates/appends/replaces a database (grid, board or calendar) from a local CSV with dialect options (header, delimiter, quote, encoding). `appflowy_import_document` converts a local `.doc`/`.docx`/`.html`/`.pdf` file into a Document page under `parent_view_id`. Both are asynchronous server-side tasks (presigned upload, then poll the status tools).

### Notifications

- `appflowy_list_notifications`
- `appflowy_get_unread_count`
- `appflowy_mark_notifications_read`
- `appflowy_mark_all_notifications_read`
- `appflowy_archive_notifications`
- `appflowy_archive_all_notifications`

The workspace notification inbox (comments, mentions, share invitations) — the way for an agent to watch collaboration events.

### User And Files

- `appflowy_get_user_profile`
- `appflowy_upload_file`

`appflowy_upload_file` uploads a local file to AppFlowy file storage and returns a URL that image or file blocks can reference. Use the page `view_id` as `parent_dir`. Files of 5 MB and above upload automatically as multipart chunks (concurrent parts with retries, aborted cleanly on failure); smaller files use the single-shot upload.

## Legacy Python Implementation

The original Python implementation (`appflowy_mcp/` and `appflowy-cli/`) was **deprecated in v0.8.0 and has since been removed from this repository** — check out tag `v0.8.0` if you need the Python sources. The Rust binary is the only implementation going forward.

- PyPI packages [`appflowy-mcp`](https://pypi.org/project/appflowy-mcp/) (final: **0.8.0**) and [`appflowy-cli`](https://pypi.org/project/appflowy-cli/) (final: **0.6.4**) remain installable but receive no further updates — the release workflow no longer publishes to PyPI.
- The Rust implementation is behavior-compatible: it shipped with the same 81 tool names and schemas as the Python version (verified via `tools/list` diff), and has since grown to 114 — same CLI commands and output, same credential file format and location.
- Beyond parity, the rewrite centralizes error handling (HTTP status/code preserved to the MCP boundary), unwraps the `{code,message,data}` envelope once in the client, retries/renews auth in one race-safe place, parallelizes export prefetch and asset upload, and keeps full attempt traces on fetch failures.

## Development

Standard cargo layout — the crate lives at the repository root (`src/`, `tests/`, `Cargo.toml`).

```bash
cargo test                 # 75 tests, self-contained (mock servers, real yjs fixtures)
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

## Notes

- Tokens are stored in memory by the MCP server process; the CLI persists them to its credentials file.
- `APPFLOWY_EMAIL`, `APPFLOWY_PASSWORD`, and `APPFLOWY_BASE_URL` can also be provided through a local `.env` file.
- Some page and space endpoints are implemented from AppFlowy source routes that are not present in the public OpenAPI document.

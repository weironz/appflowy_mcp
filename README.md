# AppFlowy MCP

An MCP server for AppFlowy with 68 tools covering workspaces, spaces, pages, page content, Markdown import, databases, rows, search, members and invitations, publishing, quick notes, file upload, and AI chat.

## Requirements

- Python 3.14
- uv
- AppFlowy account credentials

## Installation

The package is published on PyPI as [`appflowy-mcp`](https://pypi.org/project/appflowy-mcp/), so all clients can run it with `uvx appflowy-mcp`. Pick your client below.

### Claude Code

```bash
claude mcp add appflowy \
  -e APPFLOWY_EMAIL=your-email \
  -e APPFLOWY_PASSWORD=your-password \
  -- uvx appflowy-mcp
```

### Codex

```bash
codex mcp add appflowy \
  --env APPFLOWY_EMAIL=your-email \
  --env APPFLOWY_PASSWORD=your-password \
  -- uvx appflowy-mcp
```

### Other MCP Clients

Any client that supports stdio MCP servers can use this JSON config (Claude Desktop, Cursor, Windsurf, etc.):

```json
{
  "mcpServers": {
    "appflowy": {
      "command": "uvx",
      "args": ["appflowy-mcp"],
      "env": {
        "APPFLOWY_EMAIL": "your-email",
        "APPFLOWY_PASSWORD": "your-password"
      }
    }
  }
}
```

### Local Source (Development)

When developing from a cloned repository, replace `uvx appflowy-mcp` with `uv run` against your checkout. For example with Claude Code:

```bash
claude mcp add appflowy \
  -e APPFLOWY_EMAIL=your-email \
  -e APPFLOWY_PASSWORD=your-password \
  -- uv run --project /path/to/appflowy_mcp appflowy-mcp
```

## Environment Variables

Set these environment variables:

```text
APPFLOWY_EMAIL=your-email@example.com
APPFLOWY_PASSWORD=your-password
```

With these variables set, tools automatically log in on first use. `appflowy_login` is still available when you want to provide credentials explicitly.

For self-hosted AppFlowy, also set the base URL:

```text
APPFLOWY_BASE_URL=http://localhost:8000
```

If omitted, `APPFLOWY_BASE_URL` defaults to `https://beta.appflowy.cloud`.

## AppFlowy Structure

Most write operations need both a workspace and a parent view:

```text
workspace -> space -> page/database
```

To create a page inside a space, pass the workspace ID and use the space `view_id` as `parent_view_id`.

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
- `appflowy_update_page`
- `appflowy_move_page`
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

Markdown conversion supports headings, paragraphs, dividers, bullet lists, numbered lists, todo lists, quotes, code blocks, image links, and inline bold, italic, strikethrough, code, and links.

### Markdown Import

Use `appflowy_import_markdown_file` to import one local Markdown file as an AppFlowy page. Use `appflowy_import_markdown_directory` to recursively import a local folder tree.

Directory import maps local structure directly to AppFlowy:

```text
local folder -> AppFlowy page
local subfolder -> AppFlowy subpage
local .md/.markdown file -> AppFlowy page
```

`README.md` or `index.md` becomes the content of its folder page instead of a separate child page. Other Markdown files in the same folder become child pages. Hidden folders plus `.git`, `.hg`, `.svn`, `.idea`, `.vscode`, `node_modules`, and `__pycache__` are skipped.

Local image references are resolved relative to the Markdown file, uploaded to AppFlowy file storage, and replaced with AppFlowy file URLs:

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

Full-text search across a workspace. Newly written content can take a short while to appear while the server indexes it.

### Publishing

- `appflowy_publish_page`
- `appflowy_unpublish_page`
- `appflowy_get_publish_namespace`
- `appflowy_set_publish_namespace`
- `appflowy_list_published_views`

`appflowy_publish_page` returns the publish info (namespace and publish name) for building the public URL. On older server deployments that lack the list endpoint, `appflowy_list_published_views` falls back to walking the workspace folder tree.

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

### User And Files

- `appflowy_get_user_profile`
- `appflowy_upload_file`

`appflowy_upload_file` uploads a local file to AppFlowy file storage and returns a URL that image or file blocks can reference. Use the page `view_id` as `parent_dir`.

## Local Run

```bash
uv run appflowy-mcp
```

## Publish To PyPI

Build and check the package:

```bash
uv build
uv publish --dry-run --trusted-publishing never
```

Publish with a PyPI API token:

```powershell
$env:UV_PUBLISH_TOKEN="pypi-your-token"
uv publish --trusted-publishing never
```

Do not commit PyPI tokens or write them into project files.

## Notes

- Tokens are stored in memory by the MCP server process.
- `APPFLOWY_EMAIL`, `APPFLOWY_PASSWORD`, and `APPFLOWY_BASE_URL` can also be provided through a local `.env` file.
- Some page and space endpoints are implemented from AppFlowy source routes that are not present in the public OpenAPI document.

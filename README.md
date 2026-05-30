# AppFlowy MCP

An MCP server for AppFlowy Cloud. This fork adds workspace folder, space, page, trash, favorite, and basic page-content tools on top of the original workspace/database/row tools.

## Requirements

- Python 3.14
- uv
- AppFlowy Cloud account credentials

## Configure In Codex

### Option 1: PyPI / uvx

Use this after the package is published to PyPI. This is the simplest setup for daily use.

Use `uvx` as the command:

```text
uvx
```

Use these arguments:

```text
appflowy-mcp
```

### Option 2: Local Source / uv

Use this when developing the MCP locally or running directly from a cloned repository.

Use `uv` as the command:

```text
uv
```

Use these arguments:

```text
run
--project
D:\codes\MCP\Appflowy-MCP
D:\codes\MCP\Appflowy-MCP\main.py
```

### Environment Variables

Set these environment variables:

```text
APPFLOWY_EMAIL=your-email@example.com
APPFLOWY_PASSWORD=your-password
```

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

### Pages

- `appflowy_create_page`
- `appflowy_get_page`
- `appflowy_update_page`
- `appflowy_move_page_to_trash`
- `appflowy_restore_page_from_trash`
- `appflowy_delete_page_from_trash`
- `appflowy_favorite_page`
- `appflowy_list_trash`
- `appflowy_list_favorites`

### Page Content

- `appflowy_append_text_to_page`
- `appflowy_append_blocks_to_page`

Page-content support currently covers appending new document blocks. AppFlowy Cloud exposes a high-level `append-block` endpoint, but not a matching high-level REST endpoint for deleting or editing arbitrary existing blocks. Page-level deletion through trash is supported.

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

### Databases And Rows

- `appflowy_list_databases`
- `appflowy_get_database_fields`
- `appflowy_list_rows`
- `appflowy_get_row_details`
- `appflowy_create_row`
- `appflowy_upsert_row`
- `appflowy_get_updated_rows`

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
- `APPFLOWY_EMAIL` and `APPFLOWY_PASSWORD` can also be provided through a local `.env` file.
- Some page and space endpoints are implemented from AppFlowy Cloud source routes that are not present in the public OpenAPI document.

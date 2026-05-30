import os

from fastmcp import FastMCP

from .client import DEFAULT_BASE_URL, AppFlowyClient
from .importer import MarkdownImporter
from .markdown import parse_content_to_blocks, parse_markdown_to_blocks
from .models import (
    AppendPageContentRequest,
    AppendMarkdownRequest,
    LoginRequest,
    RefreshTokenRequest,
    RowCreateRequest,
    RowUpdateRequest,
    CreateSpaceRequest,
    UpdateSpaceRequest,
    CreatePageRequest,
    UpdatePageRequest,
    FavoritePageRequest,
    AppendBlocksRequest,
    AppendTextRequest,
    CreateMarkdownPageRequest,
    ImportMarkdownDirectoryRequest,
    ImportMarkdownFileRequest,
    SavePageRequest,
)
from dotenv import load_dotenv

load_dotenv()

mcp = FastMCP("appflowy")

# Global AppFlowy client
client = AppFlowyClient(
    email=os.getenv("APPFLOWY_EMAIL"),
    password=os.getenv("APPFLOWY_PASSWORD"),
    base_url=os.getenv("APPFLOWY_BASE_URL") or DEFAULT_BASE_URL,
)


def ensure_authenticated():
    if client.token_store.get_access_token():
        return
    if client.email and client.password:
        try:
            client.login()
            return
        except Exception as e:
            raise Exception(f"Auto-login failed: {str(e)}")
    raise Exception("Not authenticated. Please login first.")


def response_data(body):
    return body.get("data", body)


def ensure_parent_is_not_workspace(workspace_id: str, parent_view_id: str) -> None:
    """Guard against passing the workspace_id as a page parent.

    AppFlowy's hierarchy is workspace -> space -> page. The workspace root
    view_id equals the workspace_id, and any view created directly under it
    becomes a Space, not a document. This is a silent footgun, so reject it
    with an actionable message instead of creating an unexpected space.
    """
    if parent_view_id == workspace_id:
        raise Exception(
            "parent_view_id must be the view_id of a Space or an existing Page, "
            "not the workspace_id. AppFlowy's hierarchy is workspace -> space -> "
            "page; a view created directly under the workspace root becomes a "
            "Space rather than a document. Call appflowy_list_spaces to pick a "
            "space (or appflowy_create_space to make one) and pass its view_id."
        )


def walk_views(view):
    yield view
    for child in view.get("children") or []:
        yield from walk_views(child)


def create_page_with_blocks(
    workspace_id: str,
    parent_view_id: str,
    title: str,
    blocks: list[dict],
    layout: int = 0,
    view_id: str | None = None,
    collab_id: str | None = None,
):
    # AppFlowy stores the document collab under object_id == collab_id, but loads
    # a view's document by view_id. If only view_id is given, collab_id would
    # default to a different server-generated uuid, making the page unreadable
    # ("Collab not found"). Keep them in sync.
    if view_id is not None and collab_id is None:
        collab_id = view_id
    ensure_parent_is_not_workspace(workspace_id, parent_view_id)
    payload = {
        "parent_view_id": parent_view_id,
        "layout": layout,
        "name": title,
        "page_data": {
            "type": "page",
            "children": blocks,
        },
        "view_id": view_id,
        "collab_id": collab_id,
    }
    body = client._request(
        "POST",
        f"/api/workspace/{workspace_id}/page-view",
        json_body=payload,
    )
    return response_data(body)

# ==================== AUTHENTICATION TOOLS ====================

@mcp.tool(
    name="appflowy_login",
    description="Login to AppFlowy and get access token. Returns access token and refresh token.",
)
def appflowy_login(request: LoginRequest):
    """Login to AppFlowy. Can use provided credentials or fallback to APPFLOWY_EMAIL/APPFLOWY_PASSWORD env vars."""
    if request.email:
        client.email = request.email
    if request.password:
        client.password = request.password
        
    if not client.email or not client.password:
        raise Exception("Email and password must be provided either in the request or via APPFLOWY_EMAIL and APPFLOWY_PASSWORD env vars")
        
    try:
        result = client.login()
        return {"access_token": result.access_token, "refresh_token": result.refresh_token}
    except Exception as e:
        raise Exception(f"Login failed: {str(e)}")


@mcp.tool(
    name="appflowy_refresh_token",
    description="Refresh access token using refresh token.",
)
def appflowy_refresh_token(request: RefreshTokenRequest):
    """Refresh AppFlowy access token."""
    client.token_store.set_refresh_token(request.refresh_token)
    try:
        result = client.refresh_token()
        return {"access_token": result.access_token, "refresh_token": result.refresh_token}
    except Exception as e:
        raise Exception(f"Token refresh failed: {str(e)}")


# ==================== WORKSPACE TOOLS ====================

@mcp.tool(
    name="appflowy_list_workspaces",
    description="List all workspaces for the authenticated user.",
)
def appflowy_list_workspaces():
    """List all AppFlowy workspaces."""
    ensure_authenticated()

    try:
        body = client._request("GET", "/api/workspace")
        return body.get("data", [])
    except Exception as e:
        raise Exception(f"Failed to list workspaces: {str(e)}")


@mcp.tool(
    name="appflowy_get_workspace_folder",
    description="Get a workspace folder tree. Use root_view_id to expand a specific space/page and depth to control recursion.",
)
def appflowy_get_workspace_folder(
    workspace_id: str, depth: int | None = None, root_view_id: str | None = None
):
    """Get the AppFlowy folder tree for a workspace, space, or page."""
    ensure_authenticated()

    try:
        params = {}
        if depth is not None:
            params["depth"] = depth
        if root_view_id:
            params["root_view_id"] = root_view_id
        body = client._request(
            "GET", f"/api/workspace/{workspace_id}/folder", params=params
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to get workspace folder: {str(e)}")


@mcp.tool(
    name="appflowy_list_spaces",
    description="List all spaces in a workspace. Spaces are folder views where is_space is true.",
)
def appflowy_list_spaces(workspace_id: str, depth: int = 2):
    """List all spaces in a workspace."""
    ensure_authenticated()

    try:
        body = client._request(
            "GET", f"/api/workspace/{workspace_id}/folder", params={"depth": depth}
        )
        root = response_data(body)
        return [view for view in walk_views(root) if view.get("is_space")]
    except Exception as e:
        raise Exception(f"Failed to list spaces: {str(e)}")


@mcp.tool(name="appflowy_create_space", description="Create a space in a workspace.")
def appflowy_create_space(workspace_id: str, request: CreateSpaceRequest):
    """Create a new AppFlowy space."""
    ensure_authenticated()

    try:
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/space",
            json_body=request.model_dump(exclude_none=True),
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to create space: {str(e)}")


@mcp.tool(name="appflowy_update_space", description="Update a space name, icon, color, or permission.")
def appflowy_update_space(workspace_id: str, space_id: str, request: UpdateSpaceRequest):
    """Update an existing AppFlowy space."""
    ensure_authenticated()

    try:
        body = client._request(
            "PATCH",
            f"/api/workspace/{workspace_id}/space/{space_id}",
            json_body=request.model_dump(exclude_none=True),
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to update space: {str(e)}")


# ==================== DATABASE TOOLS ====================

@mcp.tool(
    name="appflowy_list_databases", description="List all databases in a workspace."
)
def appflowy_list_databases(workspace_id: str):
    """List all databases in a workspace."""
    ensure_authenticated()

    try:
        body = client._request("GET", f"/api/workspace/{workspace_id}/database")
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to list databases: {str(e)}")


@mcp.tool(
    name="appflowy_get_database_fields",
    description="Get fields of a specific database.",
)
def appflowy_get_database_fields(workspace_id: str, database_id: str):
    """Get fields of a specific database."""
    ensure_authenticated()

    try:
        fields = client.get_database_fields(workspace_id, database_id)
        return fields
    except Exception as e:
        raise Exception(f"Failed to get database fields: {str(e)}")


# ==================== ROW TOOLS ====================

@mcp.tool(name="appflowy_list_rows", description="List all row IDs in a database.")
def appflowy_list_rows(workspace_id: str, database_id: str):
    """List all row IDs in a database."""
    ensure_authenticated()

    try:
        rows = client.get_database_row_ids(workspace_id, database_id)
        return rows
    except Exception as e:
        raise Exception(f"Failed to list rows: {str(e)}")


@mcp.tool(
    name="appflowy_get_row_details", description="Get details of specific rows by IDs."
)
def appflowy_get_row_details(
    workspace_id: str, database_id: str, row_ids: str, with_doc: bool = False
):
    """Get details of specific rows. row_ids should be comma-separated UUIDs."""
    ensure_authenticated()

    try:
        ids_list = [id.strip() for id in row_ids.split(",") if id.strip()]
        if not ids_list:
            raise Exception("At least one row ID is required.")
            
        details = client.get_database_row_details(
            workspace_id, database_id, ids_list, with_doc=with_doc
        )
        return details
    except Exception as e:
        raise Exception(f"Failed to get row details: {str(e)}")


@mcp.tool(name="appflowy_create_row", description="Create a new row in a database.")
def appflowy_create_row(workspace_id: str, database_id: str, request: RowCreateRequest):
    """Create a new row in a database."""
    ensure_authenticated()

    try:
        row_id = client.create_database_row(
            workspace_id, database_id, cells=request.cells, document=request.document
        )
        return {"id": row_id}
    except Exception as e:
        raise Exception(f"Failed to create row: {str(e)}")


@mcp.tool(
    name="appflowy_upsert_row",
    description="Update existing row or create if it doesn't exist.",
)
def appflowy_upsert_row(workspace_id: str, database_id: str, request: RowUpdateRequest):
    """Update existing row or create if it doesn't exist."""
    ensure_authenticated()

    try:
        row_id = client.upsert_database_row(
            workspace_id, 
            database_id, 
            request.pre_hash or "", 
            cells=request.cells, 
            document=request.document
        )
        return {"id": row_id}
    except Exception as e:
        raise Exception(f"Failed to upsert row: {str(e)}")

@mcp.tool(
    name="appflowy_get_updated_rows", 
    description="Find updated rows in a database after a specific datetime."
)
def appflowy_get_updated_rows(workspace_id: str, database_id: str, after: str):
    """Find updated rows after a specific datetime (ISO 8601 string)."""
    ensure_authenticated()

    try:
        updated_rows = client.get_database_row_ids_updated(
            workspace_id, database_id, after=after
        )
        return updated_rows
    except Exception as e:
        raise Exception(f"Failed to get updated rows: {str(e)}")


# ==================== PAGE TOOLS ====================

@mcp.tool(name="appflowy_create_page", description="Create a page under a space or parent page.")
def appflowy_create_page(workspace_id: str, request: CreatePageRequest):
    """Create a document, grid, board, calendar, or chat page view."""
    ensure_authenticated()
    ensure_parent_is_not_workspace(workspace_id, request.parent_view_id)

    try:
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view",
            json_body=request.model_dump(exclude_none=True),
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to create page: {str(e)}")


@mcp.tool(name="appflowy_get_page", description="Get page view details and collab data.")
def appflowy_get_page(workspace_id: str, page_id: str):
    """Get an AppFlowy page view."""
    ensure_authenticated()

    try:
        body = client._request(
            "GET", f"/api/workspace/{workspace_id}/page-view/{page_id}"
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to get page: {str(e)}")


@mcp.tool(name="appflowy_update_page", description="Update a page name, icon, lock state, or extra metadata.")
def appflowy_update_page(
    workspace_id: str, page_id: str, request: UpdatePageRequest
):
    """Update an AppFlowy page view."""
    ensure_authenticated()

    try:
        body = client._request(
            "PATCH",
            f"/api/workspace/{workspace_id}/page-view/{page_id}",
            json_body=request.model_dump(exclude_none=True),
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to update page: {str(e)}")


@mcp.tool(name="appflowy_move_page_to_trash", description="Move a page to trash.")
def appflowy_move_page_to_trash(workspace_id: str, page_id: str):
    """Move an AppFlowy page view to trash."""
    ensure_authenticated()

    try:
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view/{page_id}/move-to-trash",
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to move page to trash: {str(e)}")


@mcp.tool(name="appflowy_restore_page_from_trash", description="Restore a page from trash.")
def appflowy_restore_page_from_trash(workspace_id: str, page_id: str):
    """Restore an AppFlowy page view from trash."""
    ensure_authenticated()

    try:
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view/{page_id}/restore-from-trash",
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to restore page from trash: {str(e)}")


@mcp.tool(name="appflowy_delete_page_from_trash", description="Permanently delete a trashed page.")
def appflowy_delete_page_from_trash(workspace_id: str, page_id: str):
    """Permanently delete an AppFlowy page view from trash."""
    ensure_authenticated()

    try:
        body = client._request(
            "DELETE", f"/api/workspace/{workspace_id}/trash/{page_id}"
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to delete page from trash: {str(e)}")


@mcp.tool(name="appflowy_favorite_page", description="Favorite, unfavorite, pin, or unpin a page.")
def appflowy_favorite_page(
    workspace_id: str, page_id: str, request: FavoritePageRequest
):
    """Favorite or unfavorite an AppFlowy page view."""
    ensure_authenticated()

    try:
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view/{page_id}/favorite",
            json_body=request.model_dump(exclude_none=True),
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to update favorite state: {str(e)}")


@mcp.tool(name="appflowy_list_trash", description="List trashed pages in a workspace.")
def appflowy_list_trash(workspace_id: str):
    """List trashed AppFlowy page views."""
    ensure_authenticated()

    try:
        body = client._request("GET", f"/api/workspace/{workspace_id}/trash")
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to list trash: {str(e)}")


@mcp.tool(name="appflowy_list_favorites", description="List favorite pages in a workspace.")
def appflowy_list_favorites(workspace_id: str):
    """List favorite AppFlowy page views."""
    ensure_authenticated()

    try:
        body = client._request("GET", f"/api/workspace/{workspace_id}/favorite")
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to list favorites: {str(e)}")


@mcp.tool(
    name="appflowy_append_blocks_to_page",
    description="Append raw AppFlowy document blocks to the end of a page.",
)
def appflowy_append_blocks_to_page(
    workspace_id: str, page_id: str, request: AppendBlocksRequest
):
    """Append raw blocks to an AppFlowy document page."""
    ensure_authenticated()

    try:
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view/{page_id}/append-block",
            json_body=request.model_dump(exclude_none=True),
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to append blocks to page: {str(e)}")


@mcp.tool(
    name="appflowy_append_text_to_page",
    description="Append one or more plain-text paragraph blocks to the end of a page.",
)
def appflowy_append_text_to_page(
    workspace_id: str, page_id: str, request: AppendTextRequest
):
    """Append plain text as paragraph-style blocks to an AppFlowy document page."""
    ensure_authenticated()

    try:
        blocks = [
            {
                "type": request.block_type,
                "data": {"delta": [{"insert": text}]},
            }
            for text in request.texts
        ]
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view/{page_id}/append-block",
            json_body={"blocks": blocks},
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to append text to page: {str(e)}")


@mcp.tool(
    name="appflowy_create_markdown_page",
    description="Create a document page from Markdown content. Prefer appflowy_save_page for general AI answer or note saving.",
)
def appflowy_create_markdown_page(
    workspace_id: str, request: CreateMarkdownPageRequest
):
    """Create an AppFlowy document page with Markdown converted to blocks."""
    ensure_authenticated()

    try:
        blocks = parse_markdown_to_blocks(request.content)
        data = create_page_with_blocks(
            workspace_id=workspace_id,
            parent_view_id=request.parent_view_id,
            title=request.title,
            blocks=blocks,
            layout=request.layout,
            view_id=request.view_id,
            collab_id=request.collab_id,
        )
        return {"page": data, "block_count": len(blocks)}
    except Exception as e:
        raise Exception(f"Failed to create markdown page: {str(e)}")


@mcp.tool(
    name="appflowy_append_markdown_to_page",
    description="Append Markdown content to an existing document page.",
)
def appflowy_append_markdown_to_page(
    workspace_id: str, page_id: str, request: AppendMarkdownRequest
):
    """Append Markdown converted to AppFlowy document blocks."""
    ensure_authenticated()

    try:
        blocks = parse_markdown_to_blocks(request.content)
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view/{page_id}/append-block",
            json_body={"blocks": blocks},
        )
        return {"result": response_data(body), "block_count": len(blocks)}
    except Exception as e:
        raise Exception(f"Failed to append markdown to page: {str(e)}")


@mcp.tool(
    name="appflowy_save_page",
    description=(
        "Default tool for saving AI answers, notes, summaries, or generated content "
        "as a new AppFlowy document page. The content_format defaults to markdown, "
        "so Markdown headings, lists, links, code blocks, and rich text are converted "
        "to AppFlowy blocks unless content_format is explicitly set to plain_text."
    ),
)
def appflowy_save_page(workspace_id: str, request: SavePageRequest):
    """Save generated content as a new AppFlowy page, using Markdown by default."""
    ensure_authenticated()

    try:
        blocks = parse_content_to_blocks(request.content, request.content_format)
        data = create_page_with_blocks(
            workspace_id=workspace_id,
            parent_view_id=request.parent_view_id,
            title=request.title,
            blocks=blocks,
            layout=request.layout,
            view_id=request.view_id,
            collab_id=request.collab_id,
        )
        return {
            "page": data,
            "block_count": len(blocks),
            "content_format": request.content_format,
        }
    except Exception as e:
        raise Exception(f"Failed to save page: {str(e)}")


@mcp.tool(
    name="appflowy_append_page_content",
    description=(
        "Default tool for appending AI answers, notes, summaries, or generated content "
        "to an existing AppFlowy document page. The content_format defaults to markdown."
    ),
)
def appflowy_append_page_content(
    workspace_id: str, page_id: str, request: AppendPageContentRequest
):
    """Append generated content to a page, using Markdown by default."""
    ensure_authenticated()

    try:
        blocks = parse_content_to_blocks(request.content, request.content_format)
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view/{page_id}/append-block",
            json_body={"blocks": blocks},
        )
        return {
            "result": response_data(body),
            "block_count": len(blocks),
            "content_format": request.content_format,
        }
    except Exception as e:
        raise Exception(f"Failed to append page content: {str(e)}")


@mcp.tool(
    name="appflowy_import_markdown_file",
    description=(
        "Import one local Markdown file as an AppFlowy page. Local image references "
        "in Markdown are resolved relative to the file, uploaded to AppFlowy, and "
        "replaced with AppFlowy file URLs when upload_assets is true."
    ),
)
def appflowy_import_markdown_file(
    workspace_id: str, request: ImportMarkdownFileRequest
):
    """Import one local Markdown file into AppFlowy."""
    ensure_authenticated()
    ensure_parent_is_not_workspace(workspace_id, request.parent_view_id)

    try:
        importer = MarkdownImporter(client, workspace_id)
        return importer.import_file(
            request.path,
            request.parent_view_id,
            title=request.title,
            upload_assets=request.upload_assets,
        )
    except Exception as e:
        raise Exception(f"Failed to import markdown file: {str(e)}")


@mcp.tool(
    name="appflowy_import_markdown_directory",
    description=(
        "Recursively import a local Markdown directory into AppFlowy. Every local "
        "folder becomes an AppFlowy page, Markdown files become child pages, and "
        "local image references are uploaded and inserted in place."
    ),
)
def appflowy_import_markdown_directory(
    workspace_id: str, request: ImportMarkdownDirectoryRequest
):
    """Import a local Markdown folder tree into AppFlowy."""
    ensure_authenticated()
    ensure_parent_is_not_workspace(workspace_id, request.parent_view_id)

    try:
        importer = MarkdownImporter(client, workspace_id)
        return importer.import_directory(
            request.path,
            request.parent_view_id,
            upload_assets=request.upload_assets,
        )
    except Exception as e:
        raise Exception(f"Failed to import markdown directory: {str(e)}")


def main():
    mcp.run()


if __name__ == "__main__":
    main()

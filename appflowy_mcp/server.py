from fastmcp import FastMCP
import os
import logging
from .models import (
    Task,
    LoginRequest,
    RefreshTokenRequest,
    AuthResponse,
    Workspace,
    Database,
    RowDetail,
    RowCreateRequest,
    RowUpdateRequest,
    CreateSpaceRequest,
    UpdateSpaceRequest,
    CreatePageRequest,
    UpdatePageRequest,
    FavoritePageRequest,
    AppendBlocksRequest,
    AppendTextRequest,
)
from dotenv import load_dotenv

from appflowysdk import AppFlowy
from appflowysdk.exceptions import AppFlowyError, LoginError, RefreshTokenError, APIError, ValidationError, NetworkError

load_dotenv()
logging.getLogger("appflowysdk").disabled = True

mcp = FastMCP("appflowy-cloud")

# Global AppFlowy client
client = AppFlowy(
    email=os.getenv("APPFLOWY_EMAIL"),
    password=os.getenv("APPFLOWY_PASSWORD")
)


def ensure_authenticated():
    if not client.token_store.get_access_token():
        raise Exception("Not authenticated. Please login first.")


def response_data(body):
    return body.get("data", body)


def walk_views(view):
    yield view
    for child in view.get("children") or []:
        yield from walk_views(child)

# ==================== AUTHENTICATION TOOLS ====================

@mcp.tool(
    name="appflowy_login",
    description="Login to AppFlowy Cloud and get access token. Returns access token and refresh token.",
)
def appflowy_login(request: LoginRequest):
    """Login to AppFlowy Cloud. Can use provided credentials or fallback to APPFLOWY_EMAIL/APPFLOWY_PASSWORD env vars."""
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
    """Refresh AppFlowy Cloud access token."""
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
            json_body=request.model_dump(),
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
            json_body=request.model_dump(),
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
        return [f.model_dump() for f in fields]
    except Exception as e:
        raise Exception(f"Failed to get database fields: {str(e)}")


# ==================== ROW TOOLS ====================

@mcp.tool(name="appflowy_list_rows", description="List all row IDs in a database.")
def appflowy_list_rows(workspace_id: str, database_id: str):
    """List all row IDs in a database."""
    ensure_authenticated()

    try:
        rows = client.get_database_row_ids(workspace_id, database_id)
        return [r.model_dump() for r in rows]
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
        return [d.model_dump() for d in details]
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
        return [r.model_dump() for r in updated_rows]
    except Exception as e:
        raise Exception(f"Failed to get updated rows: {str(e)}")


# ==================== PAGE TOOLS ====================

@mcp.tool(name="appflowy_create_page", description="Create a page under a space or parent page.")
def appflowy_create_page(workspace_id: str, request: CreatePageRequest):
    """Create a document, grid, board, calendar, or chat page view."""
    ensure_authenticated()

    try:
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view",
            json_body=request.model_dump(),
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
            json_body=request.model_dump(),
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
            json_body=request.model_dump(),
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
            json_body=request.model_dump(),
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


def main():
    mcp.run()


if __name__ == "__main__":
    main()

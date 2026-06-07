import mimetypes
import os
import uuid
from pathlib import Path

from fastmcp import FastMCP

from .client import DEFAULT_BASE_URL, AppFlowyClient
from .export import decode_document, document_to_markdown, sanitize_filename
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
    MovePageRequest,
    ReorderPagesRequest,
    AppendBlocksRequest,
    AppendTextRequest,
    CreateMarkdownPageRequest,
    ImportMarkdownDirectoryRequest,
    ImportMarkdownFileRequest,
    SavePageRequest,
    CreateWorkspaceRequest,
    UpdateWorkspaceRequest,
    UpdateWorkspaceSettingsRequest,
    UpdateMemberRequest,
    InviteMembersRequest,
    DuplicatePageRequest,
    PublishPageRequest,
    CreateQuickNoteRequest,
    UpdateQuickNoteRequest,
    CreateDatabaseFieldRequest,
    UploadFileRequest,
    CreateChatRequest,
    UpdateChatSettingsRequest,
    ExportPageRequest,
    ExportTreeRequest,
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
    description="Login to AppFlowy. Tokens are stored in the server; they are not returned to avoid leaking credentials into tool output.",
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
        client.login()
        return {"status": "authenticated"}
    except Exception as e:
        raise Exception(f"Login failed: {str(e)}")


@mcp.tool(
    name="appflowy_refresh_token",
    description="Refresh the access token. Uses the stored refresh token unless one is provided. Tokens are not returned.",
)
def appflowy_refresh_token(request: RefreshTokenRequest):
    """Refresh AppFlowy access token."""
    if request.refresh_token:
        client.token_store.set_refresh_token(request.refresh_token)
    try:
        client.refresh_token()
        return {"status": "refreshed"}
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


@mcp.tool(
    name="appflowy_move_page",
    description=(
        "Move a page to a new parent and/or reorder it among its siblings. "
        "Set new_parent_view_id to the destination parent (pass the page's "
        "current parent to reorder in place) and prev_view_id to the sibling to "
        "place it after (omit to put it first)."
    ),
)
def appflowy_move_page(workspace_id: str, page_id: str, request: MovePageRequest):
    """Move and/or reorder an AppFlowy page view."""
    ensure_authenticated()
    ensure_parent_is_not_workspace(workspace_id, request.new_parent_view_id)

    try:
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view/{page_id}/move",
            json_body=request.model_dump(exclude_none=True),
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to move page: {str(e)}")


@mcp.tool(
    name="appflowy_reorder_pages",
    description=(
        "Reorder the direct child pages of a space or page in one call. Give "
        "either an explicit page_ids order or sort_by name/created_at/"
        "last_edited_time. AppFlowy has no bulk-sort endpoint, so this chains "
        "the per-page move endpoint, skipping pages already in place."
    ),
)
def appflowy_reorder_pages(workspace_id: str, request: ReorderPagesRequest):
    """Reorder the children of an AppFlowy space or page."""
    ensure_authenticated()
    ensure_parent_is_not_workspace(workspace_id, request.parent_view_id)

    if (request.page_ids is None) == (request.sort_by is None):
        raise Exception("Provide exactly one of page_ids or sort_by.")

    try:
        body = client._request(
            "GET",
            f"/api/workspace/{workspace_id}/folder",
            params={"depth": 1, "root_view_id": request.parent_view_id},
        )
        children = response_data(body).get("children") or []
        current = [child["view_id"] for child in children]
        names = {child["view_id"]: child.get("name") for child in children}

        if request.page_ids is not None:
            unknown = [pid for pid in request.page_ids if pid not in current]
            if unknown:
                raise Exception(
                    f"Not direct children of {request.parent_view_id}: {unknown}"
                )
            listed = set(request.page_ids)
            target = request.page_ids + [
                pid for pid in current if pid not in listed
            ]
        else:
            def sort_key(child):
                value = child.get(request.sort_by)
                if request.sort_by == "name":
                    return (value or "").casefold()
                return value or ""

            target = [
                child["view_id"]
                for child in sorted(
                    children, key=sort_key, reverse=request.descending
                )
            ]

        moves = 0
        for index, page_id in enumerate(target):
            if current[index] == page_id:
                continue
            client._request(
                "POST",
                f"/api/workspace/{workspace_id}/page-view/{page_id}/move",
                json_body={
                    "new_parent_view_id": request.parent_view_id,
                    "prev_view_id": target[index - 1] if index else None,
                },
            )
            current.remove(page_id)
            current.insert(index, page_id)
            moves += 1

        return {
            "parent_view_id": request.parent_view_id,
            "order": [
                {"view_id": pid, "name": names.get(pid)} for pid in target
            ],
            "moves_performed": moves,
        }
    except Exception as e:
        raise Exception(f"Failed to reorder pages: {str(e)}")


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


@mcp.tool(
    name="appflowy_reorder_favorite",
    description=(
        "Reorder a page within the favorites list. prev_view_id is the "
        "favorite to place it after (omit to put it first)."
    ),
)
def appflowy_reorder_favorite(
    workspace_id: str, page_id: str, prev_view_id: str | None = None
):
    """Reorder an AppFlowy page in the favorites list."""
    ensure_authenticated()

    try:
        client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view/{page_id}/reorder-favorite",
            json_body={"prev_view_id": prev_view_id},
        )
        return {"page_id": page_id, "prev_view_id": prev_view_id}
    except Exception as e:
        raise Exception(f"Failed to reorder favorite: {str(e)}")


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


# ==================== SEARCH TOOLS ====================

@mcp.tool(
    name="appflowy_search",
    description=(
        "Search documents in a workspace by keyword or natural-language query "
        "(full-text plus semantic search when the server has indexing enabled). "
        "Returns matching documents with object_id (the page view_id), preview, and score."
    ),
)
def appflowy_search(
    workspace_id: str, query: str, limit: int = 10, preview_size: int = 180
):
    """Search workspace documents."""
    ensure_authenticated()

    try:
        body = client._request(
            "GET",
            f"/api/search/{workspace_id}",
            params={"query": query, "limit": limit, "preview_size": preview_size},
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to search workspace: {str(e)}")


# ==================== WORKSPACE MANAGEMENT TOOLS ====================

@mcp.tool(name="appflowy_create_workspace", description="Create a new workspace.")
def appflowy_create_workspace(request: CreateWorkspaceRequest):
    """Create a new AppFlowy workspace."""
    ensure_authenticated()

    try:
        body = client._request(
            "POST", "/api/workspace", json_body=request.model_dump(exclude_none=True)
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to create workspace: {str(e)}")


@mcp.tool(
    name="appflowy_update_workspace",
    description="Update a workspace name and/or icon.",
)
def appflowy_update_workspace(workspace_id: str, request: UpdateWorkspaceRequest):
    """Update an AppFlowy workspace."""
    ensure_authenticated()

    try:
        payload = {"workspace_id": workspace_id, **request.model_dump(exclude_none=True)}
        client._request("PATCH", "/api/workspace", json_body=payload)
        return {"status": "updated"}
    except Exception as e:
        raise Exception(f"Failed to update workspace: {str(e)}")


@mcp.tool(
    name="appflowy_delete_workspace",
    description="Permanently delete a workspace and everything in it. Irreversible.",
)
def appflowy_delete_workspace(workspace_id: str):
    """Delete an AppFlowy workspace."""
    ensure_authenticated()

    try:
        client._request("DELETE", f"/api/workspace/{workspace_id}")
        return {"status": "deleted"}
    except Exception as e:
        raise Exception(f"Failed to delete workspace: {str(e)}")


@mcp.tool(
    name="appflowy_leave_workspace",
    description="Leave a workspace you are a member of.",
)
def appflowy_leave_workspace(workspace_id: str):
    """Leave an AppFlowy workspace."""
    ensure_authenticated()

    try:
        # This endpoint expects a literal JSON null body.
        client._request_content_json(
            "POST",
            f"/api/workspace/{workspace_id}/leave",
            content=b"null",
            content_type="application/json",
        )
        return {"status": "left"}
    except Exception as e:
        raise Exception(f"Failed to leave workspace: {str(e)}")


@mcp.tool(
    name="appflowy_get_workspace_settings",
    description="Get workspace settings (search indexing, AI model).",
)
def appflowy_get_workspace_settings(workspace_id: str):
    """Get AppFlowy workspace settings."""
    ensure_authenticated()

    try:
        body = client._request("GET", f"/api/workspace/{workspace_id}/settings")
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to get workspace settings: {str(e)}")


@mcp.tool(
    name="appflowy_update_workspace_settings",
    description="Update workspace settings: disable_search_indexing and/or ai_model.",
)
def appflowy_update_workspace_settings(
    workspace_id: str, request: UpdateWorkspaceSettingsRequest
):
    """Update AppFlowy workspace settings."""
    ensure_authenticated()

    try:
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/settings",
            json_body=request.model_dump(exclude_none=True),
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to update workspace settings: {str(e)}")


@mcp.tool(
    name="appflowy_get_workspace_usage",
    description="Get workspace storage usage (consumed capacity in bytes).",
)
def appflowy_get_workspace_usage(workspace_id: str):
    """Get AppFlowy workspace storage usage."""
    ensure_authenticated()

    try:
        body = client._request("GET", f"/api/file_storage/{workspace_id}/usage")
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to get workspace usage: {str(e)}")


# ==================== MEMBER & INVITATION TOOLS ====================

@mcp.tool(
    name="appflowy_list_members",
    description="List members of a workspace with their roles.",
)
def appflowy_list_members(workspace_id: str):
    """List AppFlowy workspace members."""
    ensure_authenticated()

    try:
        body = client._request("GET", f"/api/workspace/{workspace_id}/member")
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to list members: {str(e)}")


@mcp.tool(
    name="appflowy_update_member",
    description="Update a workspace member's role (Owner, Member, Guest) or name.",
)
def appflowy_update_member(workspace_id: str, request: UpdateMemberRequest):
    """Update an AppFlowy workspace member."""
    ensure_authenticated()

    try:
        client._request(
            "PUT",
            f"/api/workspace/{workspace_id}/member",
            json_body=request.model_dump(exclude_none=True),
        )
        return {"status": "updated"}
    except Exception as e:
        raise Exception(f"Failed to update member: {str(e)}")


@mcp.tool(
    name="appflowy_remove_members",
    description="Remove members from a workspace by email.",
)
def appflowy_remove_members(workspace_id: str, emails: list[str]):
    """Remove AppFlowy workspace members."""
    ensure_authenticated()

    if not emails:
        raise Exception("At least one email is required.")
    try:
        client._request(
            "DELETE", f"/api/workspace/{workspace_id}/member", json_body=emails
        )
        return {"status": "removed", "emails": emails}
    except Exception as e:
        raise Exception(f"Failed to remove members: {str(e)}")


@mcp.tool(
    name="appflowy_invite_members",
    description="Invite people to a workspace by email with a role (Owner, Member, Guest).",
)
def appflowy_invite_members(workspace_id: str, request: InviteMembersRequest):
    """Invite members to an AppFlowy workspace."""
    ensure_authenticated()

    try:
        invitations = [
            {
                "email": email,
                "role": request.role,
                "skip_email_send": request.skip_email_send,
            }
            for email in request.emails
        ]
        client._request(
            "POST", f"/api/workspace/{workspace_id}/invite", json_body=invitations
        )
        return {"status": "invited", "emails": request.emails, "role": request.role}
    except Exception as e:
        raise Exception(f"Failed to invite members: {str(e)}")


@mcp.tool(
    name="appflowy_list_invitations",
    description="List workspace invitations sent to the authenticated user, optionally filtered by status (Pending, Accepted, Rejected).",
)
def appflowy_list_invitations(status: str | None = None):
    """List the authenticated user's AppFlowy workspace invitations."""
    ensure_authenticated()

    try:
        params = {"status": status} if status else None
        body = client._request("GET", "/api/workspace/invite", params=params)
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to list invitations: {str(e)}")


@mcp.tool(
    name="appflowy_accept_invitation",
    description="Accept a workspace invitation by invite_id.",
)
def appflowy_accept_invitation(invite_id: str):
    """Accept an AppFlowy workspace invitation."""
    ensure_authenticated()

    try:
        # This endpoint expects a literal JSON null body.
        client._request_content_json(
            "POST",
            f"/api/workspace/accept-invite/{invite_id}",
            content=b"null",
            content_type="application/json",
        )
        return {"status": "accepted"}
    except Exception as e:
        raise Exception(f"Failed to accept invitation: {str(e)}")


# ==================== MORE PAGE TOOLS ====================

@mcp.tool(
    name="appflowy_duplicate_page",
    description="Duplicate a page and all of its child pages.",
)
def appflowy_duplicate_page(
    workspace_id: str, page_id: str, request: DuplicatePageRequest
):
    """Duplicate an AppFlowy page view and its children."""
    ensure_authenticated()

    try:
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view/{page_id}/duplicate",
            json_body=request.model_dump(exclude_none=True),
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to duplicate page: {str(e)}")


@mcp.tool(
    name="appflowy_list_recent",
    description="List recently viewed pages in a workspace.",
)
def appflowy_list_recent(workspace_id: str):
    """List recently viewed AppFlowy pages."""
    ensure_authenticated()

    try:
        body = client._request("GET", f"/api/workspace/{workspace_id}/recent")
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to list recent pages: {str(e)}")


@mcp.tool(
    name="appflowy_restore_all_from_trash",
    description="Restore all trashed pages in a workspace.",
)
def appflowy_restore_all_from_trash(workspace_id: str):
    """Restore all AppFlowy pages from trash."""
    ensure_authenticated()

    try:
        client._request(
            "POST",
            f"/api/workspace/{workspace_id}/restore-all-pages-from-trash",
            json_body={},
        )
        return {"status": "restored_all"}
    except Exception as e:
        raise Exception(f"Failed to restore all pages from trash: {str(e)}")


@mcp.tool(
    name="appflowy_empty_trash",
    description="Permanently delete ALL trashed pages in a workspace. Irreversible.",
)
def appflowy_empty_trash(workspace_id: str):
    """Permanently delete all AppFlowy pages in trash."""
    ensure_authenticated()

    try:
        client._request(
            "POST",
            f"/api/workspace/{workspace_id}/delete-all-pages-from-trash",
            json_body={},
        )
        return {"status": "emptied"}
    except Exception as e:
        raise Exception(f"Failed to empty trash: {str(e)}")


# ==================== PUBLISH TOOLS ====================

@mcp.tool(
    name="appflowy_publish_page",
    description="Publish a page to the web so anyone with the link can view it.",
)
def appflowy_publish_page(workspace_id: str, page_id: str, request: PublishPageRequest):
    """Publish an AppFlowy page to the web."""
    ensure_authenticated()

    try:
        client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view/{page_id}/publish",
            json_body=request.model_dump(exclude_none=True),
        )
        info = client._request(
            "GET", f"/api/workspace/v1/published-info/{page_id}"
        )
        return response_data(info)
    except Exception as e:
        raise Exception(f"Failed to publish page: {str(e)}")


@mcp.tool(name="appflowy_unpublish_page", description="Unpublish a page from the web.")
def appflowy_unpublish_page(workspace_id: str, page_id: str):
    """Unpublish an AppFlowy page."""
    ensure_authenticated()

    try:
        client._request(
            "POST",
            f"/api/workspace/{workspace_id}/page-view/{page_id}/unpublish",
            json_body={},
        )
        return {"status": "unpublished"}
    except Exception as e:
        raise Exception(f"Failed to unpublish page: {str(e)}")


@mcp.tool(
    name="appflowy_get_publish_namespace",
    description="Get the workspace publish namespace (the subdomain published pages live under).",
)
def appflowy_get_publish_namespace(workspace_id: str):
    """Get the AppFlowy workspace publish namespace."""
    ensure_authenticated()

    try:
        body = client._request(
            "GET", f"/api/workspace/{workspace_id}/publish-namespace"
        )
        return {"namespace": response_data(body)}
    except Exception as e:
        raise Exception(f"Failed to get publish namespace: {str(e)}")


@mcp.tool(
    name="appflowy_set_publish_namespace",
    description="Rename the workspace publish namespace.",
)
def appflowy_set_publish_namespace(workspace_id: str, new_namespace: str):
    """Set the AppFlowy workspace publish namespace."""
    ensure_authenticated()

    try:
        current = client._request(
            "GET", f"/api/workspace/{workspace_id}/publish-namespace"
        )
        old_namespace = response_data(current)
        client._request(
            "PUT",
            f"/api/workspace/{workspace_id}/publish-namespace",
            json_body={
                "old_namespace": old_namespace,
                "new_namespace": new_namespace,
            },
        )
        return {"namespace": new_namespace}
    except Exception as e:
        raise Exception(f"Failed to set publish namespace: {str(e)}")


@mcp.tool(
    name="appflowy_list_published_views",
    description="List all published pages in a workspace.",
)
def appflowy_list_published_views(workspace_id: str):
    """List published AppFlowy pages."""
    ensure_authenticated()

    try:
        body = client._request("GET", f"/api/workspace/{workspace_id}/published-info")
        return response_data(body)
    except Exception as e:
        # Older deployments (e.g. beta.appflowy.cloud) lack this endpoint;
        # fall back to walking the folder tree for is_published views.
        if "404" not in str(e):
            raise Exception(f"Failed to list published views: {str(e)}")

    try:
        body = client._request(
            "GET", f"/api/workspace/{workspace_id}/folder", params={"depth": 10}
        )
        root = response_data(body)
        return [
            {
                "view_id": view.get("view_id"),
                "name": view.get("name"),
                "layout": view.get("layout"),
                "publish_name": (view.get("extra") or {}).get("publish_name")
                if isinstance(view.get("extra"), dict)
                else None,
            }
            for view in walk_views(root)
            if view.get("is_published")
        ]
    except Exception as e:
        raise Exception(f"Failed to list published views: {str(e)}")


# ==================== QUICK NOTE TOOLS ====================

def quick_note_data(text: str | None, data):
    if data is not None:
        return data
    if text is not None:
        return [
            {"type": "paragraph", "delta": [{"insert": line}]}
            for line in text.splitlines() or [""]
        ]
    return None


@mcp.tool(
    name="appflowy_list_quick_notes",
    description="List quick notes in a workspace, with optional search and pagination.",
)
def appflowy_list_quick_notes(
    workspace_id: str,
    search_term: str | None = None,
    offset: int | None = None,
    limit: int | None = None,
):
    """List AppFlowy quick notes."""
    ensure_authenticated()

    try:
        params = {
            k: v
            for k, v in {
                "search_term": search_term,
                "offset": offset,
                "limit": limit,
            }.items()
            if v is not None
        }
        body = client._request(
            "GET", f"/api/workspace/{workspace_id}/quick-note", params=params or None
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to list quick notes: {str(e)}")


@mcp.tool(
    name="appflowy_create_quick_note",
    description="Create a quick note from plain text (or raw block data).",
)
def appflowy_create_quick_note(workspace_id: str, request: CreateQuickNoteRequest):
    """Create an AppFlowy quick note."""
    ensure_authenticated()

    try:
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/quick-note",
            json_body={"data": quick_note_data(request.text, request.data)},
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to create quick note: {str(e)}")


@mcp.tool(
    name="appflowy_update_quick_note",
    description="Replace a quick note's content with plain text (or raw block data).",
)
def appflowy_update_quick_note(
    workspace_id: str, quick_note_id: str, request: UpdateQuickNoteRequest
):
    """Update an AppFlowy quick note."""
    ensure_authenticated()

    data = quick_note_data(request.text, request.data)
    if data is None:
        raise Exception("Either text or data is required.")
    try:
        client._request(
            "PUT",
            f"/api/workspace/{workspace_id}/quick-note/{quick_note_id}",
            json_body={"data": data},
        )
        return {"status": "updated"}
    except Exception as e:
        raise Exception(f"Failed to update quick note: {str(e)}")


@mcp.tool(name="appflowy_delete_quick_note", description="Delete a quick note.")
def appflowy_delete_quick_note(workspace_id: str, quick_note_id: str):
    """Delete an AppFlowy quick note."""
    ensure_authenticated()

    try:
        client._request(
            "DELETE", f"/api/workspace/{workspace_id}/quick-note/{quick_note_id}"
        )
        return {"status": "deleted"}
    except Exception as e:
        raise Exception(f"Failed to delete quick note: {str(e)}")


# ==================== MORE DATABASE TOOLS ====================

@mcp.tool(
    name="appflowy_create_database_field",
    description="Add a new field (column) to a database.",
)
def appflowy_create_database_field(
    workspace_id: str, database_id: str, request: CreateDatabaseFieldRequest
):
    """Add a field to an AppFlowy database."""
    ensure_authenticated()

    try:
        body = client._request(
            "POST",
            f"/api/workspace/{workspace_id}/database/{database_id}/fields",
            json_body=request.model_dump(exclude_none=True),
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to create database field: {str(e)}")


# ==================== USER & FILE TOOLS ====================

@mcp.tool(
    name="appflowy_get_user_profile",
    description="Get the authenticated user's profile (uid, email, name, latest workspace).",
)
def appflowy_get_user_profile():
    """Get the AppFlowy user profile."""
    ensure_authenticated()

    try:
        body = client._request("GET", "/api/user/profile")
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to get user profile: {str(e)}")


@mcp.tool(
    name="appflowy_upload_file",
    description=(
        "Upload a local file to AppFlowy file storage and return its URL, which can "
        "be referenced from image or file blocks in pages."
    ),
)
def appflowy_upload_file(workspace_id: str, request: UploadFileRequest):
    """Upload a local file to AppFlowy file storage."""
    ensure_authenticated()

    try:
        path = Path(request.path).expanduser()
        content = path.read_bytes()
        content_type = (
            request.content_type
            or mimetypes.guess_type(path.name)[0]
            or "application/octet-stream"
        )
        return client.upload_file_blob_v1(
            workspace_id,
            request.parent_dir,
            content=content,
            content_type=content_type,
        )
    except Exception as e:
        raise Exception(f"Failed to upload file: {str(e)}")


# ==================== EXPORT TOOLS ====================
#
# AppFlowy-Cloud has no export REST endpoint (the desktop app exports
# client-side), so these tools rebuild Markdown from the page collab:
# the page-view endpoint's encoded_collab is a raw yjs update, decoded
# with pycrdt to keep inline formatting that collab/json flattens.

def fetch_page_markdown(workspace_id: str, view_id: str, title: str | None) -> str:
    body = client._request(
        "GET", f"/api/workspace/{workspace_id}/page-view/{view_id}"
    )
    encoded = response_data(body).get("data", {}).get("encoded_collab")
    if not encoded:
        raise Exception("Page response did not include encoded_collab.")
    return document_to_markdown(decode_document(encoded), title=title)


def unique_path(path: Path) -> Path:
    if not path.exists():
        return path
    stem, suffix = path.stem, path.suffix
    counter = 2
    while True:
        candidate = path.with_name(f"{stem} ({counter}){suffix}")
        if not candidate.exists():
            return candidate
        counter += 1


def export_views_to_directory(
    workspace_id: str,
    views: list[dict],
    directory: Path,
    exported: list[str],
    warnings: list[str],
) -> None:
    for view in views:
        name = view.get("name") or ""
        children = view.get("children") or []
        fallback = str(view.get("view_id", ""))[:8]
        filename = sanitize_filename(name, fallback)

        if view.get("layout", 0) != 0:
            warnings.append(
                f"skipped non-document view '{name}' (layout {view.get('layout')})"
            )
            continue

        markdown = None
        if not view.get("is_space"):
            try:
                markdown = fetch_page_markdown(
                    workspace_id, view["view_id"], title=name
                )
            except Exception as e:
                warnings.append(f"failed to export '{name}': {e}")

        if children:
            subdir = unique_path(directory / filename)
            subdir.mkdir(parents=True)
            if markdown is not None:
                (subdir / "README.md").write_text(markdown, encoding="utf-8")
                exported.append(str(subdir / "README.md"))
            export_views_to_directory(
                workspace_id, children, subdir, exported, warnings
            )
        elif markdown is not None:
            target = unique_path(directory / f"{filename}.md")
            target.write_text(markdown, encoding="utf-8")
            exported.append(str(target))


@mcp.tool(
    name="appflowy_export_page",
    description=(
        "Export a single page as a local Markdown file, preserving inline "
        "formatting (bold, italic, links, code). Child pages are not included; "
        "use appflowy_export_space for a whole subtree."
    ),
)
def appflowy_export_page(workspace_id: str, page_id: str, request: ExportPageRequest):
    """Export an AppFlowy page to a local Markdown file."""
    ensure_authenticated()

    try:
        body = client._request(
            "GET", f"/api/workspace/{workspace_id}/page-view/{page_id}"
        )
        page = response_data(body)
        name = page.get("view", {}).get("name") or page_id
        encoded = page.get("data", {}).get("encoded_collab")
        if not encoded:
            raise Exception("Page response did not include encoded_collab.")
        markdown = document_to_markdown(decode_document(encoded), title=name)

        path = Path(request.path).expanduser()
        if path.suffix.lower() not in {".md", ".markdown"}:
            path = path.with_name(path.name + ".md")
        path.parent.mkdir(parents=True, exist_ok=True)
        path = unique_path(path)
        path.write_text(markdown, encoding="utf-8")
        return {"page_id": page_id, "name": name, "path": str(path)}
    except Exception as e:
        raise Exception(f"Failed to export page: {str(e)}")


@mcp.tool(
    name="appflowy_export_space",
    description=(
        "Export a space (or any page subtree) to a local directory of Markdown "
        "files, mirroring appflowy_import_markdown_directory: a page with "
        "children becomes a folder whose own content is README.md, leaf pages "
        "become .md files. Non-document views (grids, boards) are skipped with "
        "warnings."
    ),
)
def appflowy_export_space(
    workspace_id: str, space_view_id: str, request: ExportTreeRequest
):
    """Export an AppFlowy space subtree to a local directory."""
    ensure_authenticated()

    try:
        body = client._request(
            "GET",
            f"/api/workspace/{workspace_id}/folder",
            params={"depth": 10, "root_view_id": space_view_id},
        )
        root = response_data(body)

        directory = Path(request.path).expanduser()
        if directory.exists() and any(directory.iterdir()):
            raise Exception(f"Destination directory is not empty: {directory}")
        directory.mkdir(parents=True, exist_ok=True)

        exported: list[str] = []
        warnings: list[str] = []
        export_views_to_directory(
            workspace_id, [root], directory, exported, warnings
        )
        return {
            "root_view_id": space_view_id,
            "path": str(directory),
            "exported_files": exported,
            "warnings": warnings,
        }
    except Exception as e:
        raise Exception(f"Failed to export space: {str(e)}")


@mcp.tool(
    name="appflowy_export_workspace",
    description=(
        "Export every space in a workspace to a local directory tree of "
        "Markdown files. Each space becomes a top-level folder; pages follow "
        "the appflowy_export_space layout."
    ),
)
def appflowy_export_workspace(workspace_id: str, request: ExportTreeRequest):
    """Export an entire AppFlowy workspace to a local directory."""
    ensure_authenticated()

    try:
        body = client._request(
            "GET", f"/api/workspace/{workspace_id}/folder", params={"depth": 10}
        )
        root = response_data(body)

        directory = Path(request.path).expanduser()
        if directory.exists() and any(directory.iterdir()):
            raise Exception(f"Destination directory is not empty: {directory}")
        directory.mkdir(parents=True, exist_ok=True)

        exported: list[str] = []
        warnings: list[str] = []
        export_views_to_directory(
            workspace_id, root.get("children") or [], directory, exported, warnings
        )
        return {
            "workspace_id": workspace_id,
            "path": str(directory),
            "exported_files": exported,
            "warnings": warnings,
        }
    except Exception as e:
        raise Exception(f"Failed to export workspace: {str(e)}")


# ==================== AI CHAT TOOLS ====================

@mcp.tool(
    name="appflowy_create_chat",
    description=(
        "Create an AI chat in a workspace. Optionally pass rag_ids (page view ids) "
        "the AI should use as context. Returns the chat_id for follow-up calls."
    ),
)
def appflowy_create_chat(workspace_id: str, request: CreateChatRequest):
    """Create an AppFlowy AI chat."""
    ensure_authenticated()

    try:
        chat_id = request.chat_id or str(uuid.uuid4())
        client._request(
            "POST",
            f"/api/chat/{workspace_id}",
            json_body={
                "chat_id": chat_id,
                "name": request.name,
                "rag_ids": request.rag_ids,
            },
        )
        return {"chat_id": chat_id, "name": request.name, "rag_ids": request.rag_ids}
    except Exception as e:
        raise Exception(f"Failed to create chat: {str(e)}")


@mcp.tool(name="appflowy_delete_chat", description="Delete an AI chat.")
def appflowy_delete_chat(workspace_id: str, chat_id: str):
    """Delete an AppFlowy AI chat."""
    ensure_authenticated()

    try:
        client._request("DELETE", f"/api/chat/{workspace_id}/{chat_id}")
        return {"deleted": True, "chat_id": chat_id}
    except Exception as e:
        raise Exception(f"Failed to delete chat: {str(e)}")


@mcp.tool(
    name="appflowy_get_chat_settings",
    description="Get an AI chat's settings (name, rag_ids, metadata).",
)
def appflowy_get_chat_settings(workspace_id: str, chat_id: str):
    """Get AppFlowy AI chat settings."""
    ensure_authenticated()

    try:
        body = client._request("GET", f"/api/chat/{workspace_id}/{chat_id}/settings")
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to get chat settings: {str(e)}")


@mcp.tool(
    name="appflowy_update_chat_settings",
    description="Update an AI chat's name, rag_ids, or metadata.",
)
def appflowy_update_chat_settings(
    workspace_id: str, chat_id: str, request: UpdateChatSettingsRequest
):
    """Update AppFlowy AI chat settings."""
    ensure_authenticated()

    try:
        client._request(
            "POST",
            f"/api/chat/{workspace_id}/{chat_id}/settings",
            json_body=request.model_dump(exclude_none=True),
        )
        body = client._request("GET", f"/api/chat/{workspace_id}/{chat_id}/settings")
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to update chat settings: {str(e)}")


@mcp.tool(
    name="appflowy_list_chat_messages",
    description=(
        "List messages in an AI chat, newest first. Use limit to bound the page "
        "size and before_message_id to page backwards through history."
    ),
)
def appflowy_list_chat_messages(
    workspace_id: str,
    chat_id: str,
    limit: int = 20,
    before_message_id: int | None = None,
):
    """List AppFlowy AI chat messages."""
    ensure_authenticated()

    try:
        params: dict = {"limit": limit}
        if before_message_id is not None:
            params["before"] = before_message_id
        body = client._request(
            "GET", f"/api/chat/{workspace_id}/{chat_id}/message", params=params
        )
        return response_data(body)
    except Exception as e:
        raise Exception(f"Failed to list chat messages: {str(e)}")


@mcp.tool(
    name="appflowy_chat_ask",
    description=(
        "Ask the workspace AI a question in an existing chat and wait for the "
        "answer (non-streaming). Returns both the question message id and the "
        "AI answer. Create a chat first with appflowy_create_chat."
    ),
)
def appflowy_chat_ask(workspace_id: str, chat_id: str, question: str):
    """Ask the AppFlowy AI a question and return its answer."""
    ensure_authenticated()

    try:
        body = client._request(
            "POST",
            f"/api/chat/{workspace_id}/{chat_id}/message/question",
            # message_type is a u8 enum: 0=System, 1=User.
            json_body={"content": question, "message_type": 1},
        )
        question_message = response_data(body)
        question_id = question_message.get("message_id")
        if question_id is None:
            raise Exception("Question response did not include message_id.")

        body = client._request(
            "GET", f"/api/chat/{workspace_id}/{chat_id}/{question_id}/answer"
        )
        answer = response_data(body)
        return {
            "question_message_id": question_id,
            "answer_message_id": answer.get("message_id"),
            "answer": answer.get("content"),
        }
    except Exception as e:
        raise Exception(f"Failed to ask chat question: {str(e)}")


def main():
    mcp.run()


if __name__ == "__main__":
    main()

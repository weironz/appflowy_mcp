from typing import Any, Literal

from pydantic import BaseModel, Field


PARENT_VIEW_ID_DESC = (
    "view_id of the parent Space or existing Page to create under. Must NOT be "
    "the workspace_id: AppFlowy's hierarchy is workspace -> space -> page, and a "
    "view created directly under the workspace root becomes a Space, not a "
    "document. Call appflowy_list_spaces to pick a space (or appflowy_create_space "
    "to make one) and pass its view_id here."
)


class LoginRequest(BaseModel):
    email: str | None = None
    password: str | None = None


class RefreshTokenRequest(BaseModel):
    refresh_token: str | None = None


class RowCreateRequest(BaseModel):
    cells: dict[str, Any]
    document: str | None = None


class RowUpdateRequest(BaseModel):
    pre_hash: str | None = None
    cells: dict[str, Any]
    document: str | None = None


class CreateSpaceRequest(BaseModel):
    name: str
    space_permission: int = Field(0, ge=0, le=1)
    space_icon: str = "interface_essential/home-3"
    space_icon_color: str = "0xFFA34AFD"
    view_id: str | None = None


class UpdateSpaceRequest(BaseModel):
    name: str
    space_permission: int = Field(0, ge=0, le=1)
    space_icon: str = "interface_essential/home-3"
    space_icon_color: str = "0xFFA34AFD"


class CreatePageRequest(BaseModel):
    parent_view_id: str = Field(..., description=PARENT_VIEW_ID_DESC)
    name: str | None = None
    layout: int = Field(0, ge=0, le=4)
    page_data: dict[str, Any] | None = None
    view_id: str | None = None
    collab_id: str | None = None


class UpdatePageRequest(BaseModel):
    name: str
    icon: dict[str, Any] | None = None
    is_locked: bool | None = None
    extra: dict[str, Any] | None = None


class FavoritePageRequest(BaseModel):
    is_favorite: bool = True
    is_pinned: bool = False


class MovePageRequest(BaseModel):
    new_parent_view_id: str = Field(
        ...,
        description=(
            "view_id of the destination parent Space or Page. To REORDER a page "
            "in place, pass its current parent_view_id here. Must NOT be the "
            "workspace_id (a child of the workspace root becomes a Space)."
        ),
    )
    prev_view_id: str | None = Field(
        None,
        description=(
            "Ordering: view_id of the sibling to place this page immediately "
            "AFTER. Omit / null to make it the first child. This is how pages are "
            "reordered among their siblings."
        ),
    )


class AppendBlocksRequest(BaseModel):
    blocks: list[dict[str, Any]] = Field(..., min_length=1)


class AppendTextRequest(BaseModel):
    texts: list[str] = Field(..., min_length=1)
    block_type: str = "paragraph"


class CreateMarkdownPageRequest(BaseModel):
    parent_view_id: str = Field(..., description=PARENT_VIEW_ID_DESC)
    title: str = Field(..., min_length=1)
    content: str = Field(..., min_length=1)
    layout: int = Field(0, ge=0, le=4)
    view_id: str | None = None
    collab_id: str | None = None


class AppendMarkdownRequest(BaseModel):
    content: str = Field(..., min_length=1)


class SavePageRequest(BaseModel):
    parent_view_id: str = Field(..., description=PARENT_VIEW_ID_DESC)
    title: str = Field(..., min_length=1)
    content: str = Field(..., min_length=1)
    content_format: str = "markdown"
    layout: int = Field(0, ge=0, le=4)
    view_id: str | None = None
    collab_id: str | None = None


class AppendPageContentRequest(BaseModel):
    content: str = Field(..., min_length=1)
    content_format: str = "markdown"


class ImportMarkdownFileRequest(BaseModel):
    parent_view_id: str = Field(..., description=PARENT_VIEW_ID_DESC)
    path: str = Field(..., min_length=1)
    title: str | None = None
    upload_assets: bool = True


class ImportMarkdownDirectoryRequest(BaseModel):
    parent_view_id: str = Field(..., description=PARENT_VIEW_ID_DESC)
    path: str = Field(..., min_length=1)
    upload_assets: bool = True


WorkspaceRole = Literal["Owner", "Member", "Guest"]


class CreateWorkspaceRequest(BaseModel):
    workspace_name: str = Field(..., min_length=1)
    workspace_icon: str | None = None


class UpdateWorkspaceRequest(BaseModel):
    workspace_name: str | None = None
    workspace_icon: str | None = None


class UpdateWorkspaceSettingsRequest(BaseModel):
    disable_search_indexing: bool | None = None
    ai_model: str | None = None


class UpdateMemberRequest(BaseModel):
    email: str = Field(..., min_length=1)
    role: WorkspaceRole | None = None
    name: str | None = None


class InviteMembersRequest(BaseModel):
    emails: list[str] = Field(..., min_length=1)
    role: WorkspaceRole = "Member"
    skip_email_send: bool = False


class DuplicatePageRequest(BaseModel):
    suffix: str | None = Field(
        None, description='Name suffix for the copy, e.g. " (Copy)".'
    )


class PublishPageRequest(BaseModel):
    publish_name: str | None = Field(
        None, description="URL slug for the published page. Server picks one if omitted."
    )
    visible_database_view_ids: list[str] | None = None
    comments_enabled: bool = True
    duplicate_enabled: bool = True


class CreateQuickNoteRequest(BaseModel):
    text: str | None = Field(
        None, description="Plain text convenience; converted to paragraph blocks."
    )
    data: Any | None = Field(
        None, description="Raw quick-note JSON data. Takes precedence over text."
    )


class UpdateQuickNoteRequest(BaseModel):
    text: str | None = Field(
        None, description="Plain text convenience; converted to paragraph blocks."
    )
    data: Any | None = Field(
        None, description="Raw quick-note JSON data. Takes precedence over text."
    )


class CreateDatabaseFieldRequest(BaseModel):
    name: str = Field(..., min_length=1)
    field_type: int = Field(
        ...,
        description=(
            "AppFlowy field type id: 0=RichText, 1=Number, 2=DateTime, "
            "3=SingleSelect, 4=MultiSelect, 5=Checkbox, 6=URL, 7=Checklist."
        ),
    )
    type_option_data: dict[str, Any] | None = None


class UploadFileRequest(BaseModel):
    path: str = Field(..., min_length=1, description="Local file path to upload.")
    parent_dir: str = Field(
        ...,
        description=(
            "Directory key the blob is stored under; AppFlowy convention is the "
            "view_id of the page the file belongs to."
        ),
    )
    content_type: str | None = Field(
        None, description="MIME type. Guessed from the file name if omitted."
    )


class CreateChatRequest(BaseModel):
    name: str = Field(..., min_length=1)
    rag_ids: list[str] = Field(
        default_factory=list,
        description=(
            "View ids of pages the AI may use as RAG context. Empty list means "
            "no page context."
        ),
    )
    chat_id: str | None = Field(
        None, description="Chat UUID. Generated automatically if omitted."
    )


class UpdateChatSettingsRequest(BaseModel):
    name: str | None = None
    rag_ids: list[str] | None = Field(
        None, description="Replace the set of RAG context view ids."
    )
    metadata: dict[str, Any] | None = None


class ReorderPagesRequest(BaseModel):
    parent_view_id: str = Field(
        ...,
        description=(
            "view_id of the Space or Page whose direct children should be "
            "reordered. Must NOT be the workspace_id."
        ),
    )
    page_ids: list[str] | None = Field(
        None,
        description=(
            "Explicit desired order of child view_ids. Listed pages are placed "
            "first in this order; unlisted children keep their current relative "
            "order after them. Mutually exclusive with sort_by."
        ),
    )
    sort_by: Literal["name", "created_at", "last_edited_time"] | None = Field(
        None,
        description=(
            "Sort the children by this field instead of giving an explicit "
            "page_ids order. Mutually exclusive with page_ids."
        ),
    )
    descending: bool = Field(
        False, description="Reverse the sort_by order. Ignored with page_ids."
    )


class ExportPageRequest(BaseModel):
    path: str = Field(
        ...,
        description=(
            "Local destination file path for the Markdown export. '.md' is "
            "appended if missing. Parent directories are created."
        ),
    )


class ExportTreeRequest(BaseModel):
    path: str = Field(
        ...,
        description=(
            "Local destination directory. Created if missing; must be empty "
            "or not yet exist so existing files are never overwritten."
        ),
    )

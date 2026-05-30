from typing import Any

from pydantic import BaseModel, Field


class LoginRequest(BaseModel):
    email: str | None = None
    password: str | None = None


class RefreshTokenRequest(BaseModel):
    refresh_token: str


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
    parent_view_id: str
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


class AppendBlocksRequest(BaseModel):
    blocks: list[dict[str, Any]] = Field(..., min_length=1)


class AppendTextRequest(BaseModel):
    texts: list[str] = Field(..., min_length=1)
    block_type: str = "paragraph"


class CreateMarkdownPageRequest(BaseModel):
    parent_view_id: str
    title: str = Field(..., min_length=1)
    content: str = Field(..., min_length=1)
    layout: int = Field(0, ge=0, le=4)
    view_id: str | None = None
    collab_id: str | None = None


class AppendMarkdownRequest(BaseModel):
    content: str = Field(..., min_length=1)


class SavePageRequest(BaseModel):
    parent_view_id: str
    title: str = Field(..., min_length=1)
    content: str = Field(..., min_length=1)
    content_format: str = "markdown"
    layout: int = Field(0, ge=0, le=4)
    view_id: str | None = None
    collab_id: str | None = None


class AppendPageContentRequest(BaseModel):
    content: str = Field(..., min_length=1)
    content_format: str = "markdown"

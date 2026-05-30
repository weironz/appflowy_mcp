from pydantic import BaseModel
from typing import Optional, Dict, Any, List

# AppFlowy Cloud Models


class LoginRequest(BaseModel):
    email: Optional[str] = None
    password: Optional[str] = None


class RefreshTokenRequest(BaseModel):
    refresh_token: str


class AuthResponse(BaseModel):
    access_token: str
    token_type: str
    expires_in: int
    refresh_token: str


class Workspace(BaseModel):
    id: str
    name: str
    database_id: Optional[str] = None


class Database(BaseModel):
    id: str
    name: str
    workspace_id: str


class RowDetail(BaseModel):
    id: str
    cells: Dict[str, Any]
    document: Optional[str] = None


class RowCreateRequest(BaseModel):
    cells: Dict[str, Any]
    document: Optional[str] = None


class RowUpdateRequest(BaseModel):
    pre_hash: Optional[str] = None
    cells: Dict[str, Any]
    document: Optional[str] = None


class CreateSpaceRequest(BaseModel):
    name: str
    space_permission: int = 0
    space_icon: str = "interface_essential/home-3"
    space_icon_color: str = "0xFFA34AFD"
    view_id: Optional[str] = None


class UpdateSpaceRequest(BaseModel):
    name: str
    space_permission: int = 0
    space_icon: str = "interface_essential/home-3"
    space_icon_color: str = "0xFFA34AFD"


class CreatePageRequest(BaseModel):
    parent_view_id: str
    name: Optional[str] = None
    layout: int = 0
    page_data: Optional[Dict[str, Any]] = None
    view_id: Optional[str] = None
    collab_id: Optional[str] = None


class UpdatePageRequest(BaseModel):
    name: str
    icon: Optional[Dict[str, Any]] = None
    is_locked: Optional[bool] = None
    extra: Optional[Dict[str, Any]] = None


class FavoritePageRequest(BaseModel):
    is_favorite: bool = True
    is_pinned: bool = False


class AppendBlocksRequest(BaseModel):
    blocks: List[Dict[str, Any]]


class AppendTextRequest(BaseModel):
    texts: List[str]
    block_type: str = "paragraph"


# Todoist Models (existing)
class Task(BaseModel):
    id: str | None = None
    content: str
    description: str
    project_id: str | None = None
    priority: int

import mimetypes
import uuid
from pathlib import Path
from typing import Any
from urllib.parse import unquote, urlsplit

from .client import AppFlowyClient
from .markdown import parse_markdown_to_blocks


MARKDOWN_SUFFIXES = {".md", ".markdown"}
INDEX_FILENAMES = {"readme.md", "index.md"}
IGNORED_DIRECTORIES = {".git", ".hg", ".svn", ".idea", ".vscode", "node_modules", "__pycache__"}


class MarkdownImporter:
    def __init__(self, client: AppFlowyClient, workspace_id: str) -> None:
        self.client = client
        self.workspace_id = workspace_id
        self.created_pages: list[dict[str, Any]] = []
        self.uploaded_assets: list[dict[str, Any]] = []
        self.warnings: list[str] = []
        self.errors: list[dict[str, str]] = []

    def import_file(
        self,
        file_path: str,
        parent_view_id: str,
        *,
        title: str | None = None,
        upload_assets: bool = True,
    ) -> dict[str, Any]:
        path = Path(file_path).expanduser().resolve()
        if not path.is_file():
            raise ValueError(f"Markdown file does not exist: {path}")
        if path.suffix.lower() not in MARKDOWN_SUFFIXES:
            raise ValueError(f"File is not a Markdown file: {path}")

        page = self._create_markdown_page(
            markdown_path=path,
            parent_view_id=parent_view_id,
            title=title or path.stem,
            upload_assets=upload_assets,
            page_kind="markdown_file",
        )
        return self._summary(root_page=page)

    def import_directory(
        self,
        directory_path: str,
        parent_view_id: str,
        *,
        upload_assets: bool = True,
    ) -> dict[str, Any]:
        path = Path(directory_path).expanduser().resolve()
        if not path.is_dir():
            raise ValueError(f"Markdown directory does not exist: {path}")

        root_page = self._import_directory(
            directory_path=path,
            parent_view_id=parent_view_id,
            upload_assets=upload_assets,
        )
        return self._summary(root_page=root_page)

    def _import_directory(
        self,
        directory_path: Path,
        parent_view_id: str,
        *,
        upload_assets: bool,
    ) -> dict[str, Any] | None:
        index_file = self._index_file(directory_path)
        view_id = str(uuid.uuid4())
        collab_id = str(uuid.uuid4())
        blocks: list[dict[str, Any]] = []
        asset_count = 0

        if index_file:
            content = self._read_markdown(index_file)
            before = len(self.uploaded_assets)
            blocks = parse_markdown_to_blocks(
                content,
                image_url_resolver=self._image_resolver(
                    index_file.parent,
                    page_view_id=view_id,
                    upload_assets=upload_assets,
                ),
            )
            asset_count = len(self.uploaded_assets) - before

        page = self._create_page(
            parent_view_id=parent_view_id,
            title=directory_path.name,
            blocks=blocks,
            view_id=view_id,
            collab_id=collab_id,
            source_path=directory_path,
            page_kind="folder",
            asset_count=asset_count,
        )
        if not page:
            return None

        for markdown_path in self._markdown_files(directory_path):
            if index_file and markdown_path.resolve() == index_file.resolve():
                continue
            try:
                self._create_markdown_page(
                    markdown_path=markdown_path,
                    parent_view_id=view_id,
                    title=markdown_path.stem,
                    upload_assets=upload_assets,
                    page_kind="markdown_file",
                )
            except Exception as e:
                self.errors.append({"path": str(markdown_path), "error": str(e)})

        for child_dir in self._child_directories(directory_path):
            try:
                self._import_directory(
                    directory_path=child_dir,
                    parent_view_id=view_id,
                    upload_assets=upload_assets,
                )
            except Exception as e:
                self.errors.append({"path": str(child_dir), "error": str(e)})

        return page

    def _create_markdown_page(
        self,
        markdown_path: Path,
        parent_view_id: str,
        title: str,
        *,
        upload_assets: bool,
        page_kind: str,
    ) -> dict[str, Any] | None:
        view_id = str(uuid.uuid4())
        collab_id = str(uuid.uuid4())
        content = self._read_markdown(markdown_path)
        before = len(self.uploaded_assets)
        blocks = parse_markdown_to_blocks(
            content,
            image_url_resolver=self._image_resolver(
                markdown_path.parent,
                page_view_id=view_id,
                upload_assets=upload_assets,
            ),
        )
        return self._create_page(
            parent_view_id=parent_view_id,
            title=title,
            blocks=blocks,
            view_id=view_id,
            collab_id=collab_id,
            source_path=markdown_path,
            page_kind=page_kind,
            asset_count=len(self.uploaded_assets) - before,
        )

    def _create_page(
        self,
        *,
        parent_view_id: str,
        title: str,
        blocks: list[dict[str, Any]],
        view_id: str,
        collab_id: str,
        source_path: Path,
        page_kind: str,
        asset_count: int,
    ) -> dict[str, Any] | None:
        payload = {
            "parent_view_id": parent_view_id,
            "layout": 0,
            "name": title,
            "page_data": {
                "type": "page",
                "children": blocks,
            },
            "view_id": view_id,
            "collab_id": collab_id,
        }
        try:
            body = self.client._request(
                "POST",
                f"/api/workspace/{self.workspace_id}/page-view",
                json_body=payload,
            )
            page = body.get("data", body)
            entry = {
                "title": title,
                "type": page_kind,
                "source_path": str(source_path),
                "view_id": view_id,
                "collab_id": collab_id,
                "parent_view_id": parent_view_id,
                "block_count": len(blocks),
                "asset_count": asset_count,
                "page": page,
            }
            self.created_pages.append(entry)
            return entry
        except Exception as e:
            self.errors.append({"path": str(source_path), "error": str(e)})
            return None

    def _image_resolver(
        self,
        markdown_dir: Path,
        *,
        page_view_id: str,
        upload_assets: bool,
    ):
        def resolve(image_url: str, caption: str) -> str:
            if not upload_assets or self._is_remote_url(image_url):
                return image_url

            image_path = self._resolve_local_asset(markdown_dir, image_url)
            if not image_path:
                self.warnings.append(f"Image not found: {image_url} in {markdown_dir}")
                return image_url

            try:
                content = image_path.read_bytes()
                content_type = (
                    mimetypes.guess_type(image_path.name)[0] or "application/octet-stream"
                )
                upload = self.client.upload_file_blob_v1(
                    self.workspace_id,
                    page_view_id,
                    content=content,
                    content_type=content_type,
                )
                self.uploaded_assets.append(
                    {
                        "source_path": str(image_path),
                        "markdown_url": image_url,
                        "caption": caption,
                        "page_view_id": page_view_id,
                        "file_id": upload.get("file_id"),
                        "url": upload.get("url"),
                        "content_type": content_type,
                        "content_length": len(content),
                    }
                )
                return str(upload["url"])
            except Exception as e:
                self.errors.append({"path": str(image_path), "error": str(e)})
                return image_url

        return resolve

    def _resolve_local_asset(self, markdown_dir: Path, image_url: str) -> Path | None:
        split = urlsplit(image_url)
        raw_path = unquote(split.path).replace("\\", "/")
        if not raw_path:
            return None
        candidate = Path(raw_path)
        if not candidate.is_absolute():
            candidate = markdown_dir / candidate
        try:
            resolved = candidate.resolve()
        except OSError:
            return None
        return resolved if resolved.is_file() else None

    def _read_markdown(self, path: Path) -> str:
        try:
            return path.read_text(encoding="utf-8-sig")
        except UnicodeDecodeError:
            self.warnings.append(f"File is not valid UTF-8, decoded with replacement: {path}")
            return path.read_text(encoding="utf-8", errors="replace")

    def _index_file(self, directory_path: Path) -> Path | None:
        for item in sorted(directory_path.iterdir(), key=lambda p: p.name.lower()):
            if item.is_file() and item.name.lower() in INDEX_FILENAMES:
                return item
        return None

    def _markdown_files(self, directory_path: Path) -> list[Path]:
        return [
            item
            for item in sorted(directory_path.iterdir(), key=lambda p: p.name.lower())
            if item.is_file() and item.suffix.lower() in MARKDOWN_SUFFIXES
        ]

    def _child_directories(self, directory_path: Path) -> list[Path]:
        return [
            item
            for item in sorted(directory_path.iterdir(), key=lambda p: p.name.lower())
            if item.is_dir()
            and item.name not in IGNORED_DIRECTORIES
            and not item.name.startswith(".")
        ]

    def _summary(self, *, root_page: dict[str, Any] | None) -> dict[str, Any]:
        return {
            "root_page": root_page,
            "created_page_count": len(self.created_pages),
            "uploaded_asset_count": len(self.uploaded_assets),
            "error_count": len(self.errors),
            "warning_count": len(self.warnings),
            "created_pages": self.created_pages,
            "uploaded_assets": self.uploaded_assets,
            "warnings": self.warnings,
            "errors": self.errors,
        }

    def _is_remote_url(self, value: str) -> bool:
        scheme = urlsplit(value).scheme.lower()
        return scheme in {"http", "https", "data", "mailto"}

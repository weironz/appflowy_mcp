"""Command-line interface for AppFlowy.

A thin layer over the same client/converters the MCP server uses, aimed
at scriptable workflows (backups via cron, bulk import, quick lookups).

Authentication, in priority order:
1. APPFLOWY_EMAIL / APPFLOWY_PASSWORD environment variables (or .env)
2. tokens saved by `appflowy-cli login` in ~/.config/appflowy-cli/
"""
import argparse
import getpass
import json
import os
import re
import sys
from importlib.metadata import version
from pathlib import Path

from .importer import MarkdownImporter
from .markdown import parse_content_to_blocks
from .server import (
    client,
    create_page_with_blocks,
    ensure_authenticated,
    export_views_to_directory,
    fetch_page_markdown,
    response_data,
    unique_path,
    walk_views,
)


def credentials_path() -> Path:
    base = os.environ.get("XDG_CONFIG_HOME", "~/.config")
    return Path(base).expanduser() / "appflowy-cli" / "credentials.json"


def save_credentials() -> None:
    """Persist the session tokens (never the password) with mode 600."""
    path = credentials_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            {
                "email": client.email,
                "base_url": client.base_url,
                "access_token": client.token_store.get_access_token(),
                "refresh_token": client.token_store.get_refresh_token(),
            },
            indent=2,
        ),
        encoding="utf-8",
    )
    path.chmod(0o600)


def load_credentials() -> bool:
    """Restore a saved session into the client. Returns True if loaded."""
    path = credentials_path()
    if not path.is_file():
        return False
    try:
        stored = json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return False
    if not stored.get("refresh_token"):
        return False
    client.email = client.email or stored.get("email")
    if stored.get("base_url") and not os.getenv("APPFLOWY_BASE_URL"):
        client.base_url = stored["base_url"]
    client.token_store.set_access_token(stored.get("access_token"))
    client.token_store.set_refresh_token(stored["refresh_token"])
    return True


def cmd_login(args):
    email = args.email or input("Email: ")
    password = args.password or getpass.getpass("Password: ")
    if args.base_url:
        client.base_url = args.base_url.rstrip("/")
    client.email, client.password = email, password
    client.login()
    save_credentials()
    print(f"Logged in as {email}. Tokens saved to {credentials_path()}")


def cmd_logout(args):
    path = credentials_path()
    if path.is_file():
        path.unlink()
        print(f"Removed {path}")
    else:
        print("No saved credentials.")


def emit(args, data, human):
    if args.json:
        print(json.dumps(data, indent=2, ensure_ascii=False))
    else:
        human(data)


UUID_RE = re.compile(
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
    r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
)


def resolve_workspace(token: str) -> str:
    """Accept a workspace id or name; resolve names to ids."""
    if UUID_RE.match(token):
        return token
    workspaces = response_data(client._request("GET", "/api/workspace"))
    matches = [
        ws for ws in workspaces
        if (ws.get("workspace_name") or "").casefold() == token.casefold()
    ]
    if len(matches) == 1:
        return matches[0]["workspace_id"]
    names = ", ".join(sorted(ws.get("workspace_name", "?") for ws in workspaces))
    kind = "Ambiguous" if matches else "Unknown"
    raise Exception(f"{kind} workspace '{token}'. Available: {names}")


def resolve_view(workspace_id: str, token: str) -> str:
    """Accept a view id or name; resolve names by walking the folder tree."""
    if UUID_RE.match(token):
        return token
    body = client._request(
        "GET", f"/api/workspace/{workspace_id}/folder", params={"depth": 10}
    )
    views = list(walk_views(response_data(body)))
    matches = [
        v for v in views if (v.get("name") or "").casefold() == token.casefold()
    ]
    if len(matches) == 1:
        return matches[0]["view_id"]
    if matches:
        listing = ", ".join(f"{v.get('name')} ({v.get('view_id')})" for v in matches)
        raise Exception(
            f"Ambiguous view name '{token}' — pass a view_id instead: {listing}"
        )
    raise Exception(
        f"No space or page named '{token}' in this workspace. "
        "Run `appflowy-cli folder <workspace>` to list views."
    )


def resolve_args(args) -> None:
    if getattr(args, "workspace", None):
        args.workspace = resolve_workspace(args.workspace)
        for attr in ("space", "page", "parent", "root"):
            value = getattr(args, attr, None)
            if value:
                setattr(args, attr, resolve_view(args.workspace, value))


def cmd_workspaces(args):
    body = client._request("GET", "/api/workspace")
    workspaces = response_data(body)

    def human(data):
        for ws in data:
            print(f"{ws.get('workspace_id')}  {ws.get('workspace_name')}")

    emit(args, workspaces, human)


def cmd_spaces(args):
    body = client._request(
        "GET", f"/api/workspace/{args.workspace}/folder", params={"depth": 2}
    )
    spaces = [v for v in walk_views(response_data(body)) if v.get("is_space")]

    def human(data):
        for space in data:
            print(f"{space.get('view_id')}  {space.get('name')}")

    emit(args, spaces, human)


def cmd_folder(args):
    params = {"depth": args.depth}
    if args.root:
        params["root_view_id"] = args.root
    body = client._request(
        "GET", f"/api/workspace/{args.workspace}/folder", params=params
    )
    root = response_data(body)

    def human(data):
        def tree(view, indent):
            marker = "[space] " if view.get("is_space") else ""
            print(f"{'  ' * indent}{marker}{view.get('name')}  ({view.get('view_id')})")
            for child in view.get("children") or []:
                tree(child, indent + 1)

        tree(data, 0)

    emit(args, root, human)


def cmd_search(args):
    body = client._request(
        "GET",
        f"/api/search/{args.workspace}",
        params={"query": args.query, "limit": args.limit},
    )
    hits = response_data(body)

    def human(data):
        if not data:
            print("(no results)")
        for hit in data:
            print(f"{hit.get('object_id')}  {hit.get('preview', '')!r}")

    emit(args, hits, human)


def cmd_export_page(args):
    body = client._request(
        "GET", f"/api/workspace/{args.workspace}/page-view/{args.page}"
    )
    page = response_data(body)
    name = page.get("view", {}).get("name") or args.page
    markdown = fetch_page_markdown(args.workspace, args.page, title=name)

    path = Path(args.output).expanduser()
    if path.suffix.lower() not in {".md", ".markdown"}:
        path = path.with_name(path.name + ".md")
    path.parent.mkdir(parents=True, exist_ok=True)
    path = unique_path(path)
    path.write_text(markdown, encoding="utf-8")
    emit(args, {"page_id": args.page, "name": name, "path": str(path)},
         lambda d: print(d["path"]))


def export_tree(args, root_views):
    directory = Path(args.output).expanduser()
    if directory.exists() and any(directory.iterdir()):
        raise Exception(f"Destination directory is not empty: {directory}")
    directory.mkdir(parents=True, exist_ok=True)

    exported: list[str] = []
    warnings: list[str] = []
    export_views_to_directory(args.workspace, root_views, directory, exported, warnings)
    result = {"path": str(directory), "exported_files": exported, "warnings": warnings}

    def human(data):
        for file in data["exported_files"]:
            print(file)
        for warning in data["warnings"]:
            print(f"warning: {warning}", file=sys.stderr)
        print(f"{len(data['exported_files'])} files -> {data['path']}")

    emit(args, result, human)


def cmd_export_space(args):
    body = client._request(
        "GET",
        f"/api/workspace/{args.workspace}/folder",
        params={"depth": 10, "root_view_id": args.space},
    )
    export_tree(args, [response_data(body)])


def cmd_export_workspace(args):
    body = client._request(
        "GET", f"/api/workspace/{args.workspace}/folder", params={"depth": 10}
    )
    export_tree(args, response_data(body).get("children") or [])


def import_summary(args, summary):
    def human(data):
        for page in data.get("created_pages", []):
            print(f"{page.get('view_id')}  {page.get('title')}")
        for warning in data.get("warnings", []):
            print(f"warning: {warning}", file=sys.stderr)
        print(f"{len(data.get('created_pages', []))} pages imported")

    emit(args, summary, human)


def cmd_import_file(args):
    importer = MarkdownImporter(client, args.workspace)
    summary = importer.import_file(
        args.file, args.parent, title=args.title, upload_assets=not args.no_assets
    )
    import_summary(args, summary)


def cmd_import_dir(args):
    importer = MarkdownImporter(client, args.workspace)
    summary = importer.import_directory(
        args.directory, args.parent, upload_assets=not args.no_assets
    )
    import_summary(args, summary)


def cmd_save(args):
    if args.file:
        content = Path(args.file).expanduser().read_text(encoding="utf-8")
    else:
        content = args.content if args.content is not None else sys.stdin.read()
    blocks = parse_content_to_blocks(content, "markdown")
    page = create_page_with_blocks(args.workspace, args.parent, args.title, blocks)
    emit(args, page, lambda d: print(d.get("view_id", d)))


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="appflowy", description="AppFlowy command-line client."
    )
    parser.add_argument(
        "--version", action="version", version=version("appflowy-mcp")
    )
    parser.add_argument(
        "--json", action="store_true", help="Print raw JSON instead of summaries."
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser(
        "login",
        help="Log in and save session tokens for later commands "
        "(the password itself is never stored).",
    )
    p.add_argument("--email", help="prompted for interactively if omitted")
    p.add_argument("--password", help="prompted for interactively if omitted")
    p.add_argument("--base-url", help="self-hosted AppFlowy Cloud URL")
    p.set_defaults(func=cmd_login, auth=False)

    p = sub.add_parser("logout", help="Delete saved session tokens.")
    p.set_defaults(func=cmd_logout, auth=False)

    p = sub.add_parser("workspaces", help="List workspaces.")
    p.set_defaults(func=cmd_workspaces)

    p = sub.add_parser("spaces", help="List spaces in a workspace.")
    p.add_argument("workspace", help="workspace id or name")
    p.set_defaults(func=cmd_spaces)

    p = sub.add_parser("folder", help="Print the folder tree of a workspace.")
    p.add_argument("workspace", help="workspace id or name")
    p.add_argument("--depth", type=int, default=10)
    p.add_argument("--root", help="root view_id to expand (default: workspace root)")
    p.set_defaults(func=cmd_folder)

    p = sub.add_parser("search", help="Full-text search a workspace.")
    p.add_argument("workspace", help="workspace id or name")
    p.add_argument("query")
    p.add_argument("--limit", type=int, default=10)
    p.set_defaults(func=cmd_search)

    p = sub.add_parser("export-page", help="Export one page to a Markdown file.")
    p.add_argument("workspace", help="workspace id or name")
    p.add_argument("page", help="page view_id or name")
    p.add_argument("-o", "--output", required=True, help="destination .md file")
    p.set_defaults(func=cmd_export_page)

    p = sub.add_parser(
        "export-space", help="Export a space/page subtree to a directory."
    )
    p.add_argument("workspace", help="workspace id or name")
    p.add_argument("space", help="space view_id or name")
    p.add_argument("-o", "--output", required=True, help="destination directory")
    p.set_defaults(func=cmd_export_space)

    p = sub.add_parser(
        "export-workspace", help="Export every space in a workspace to a directory."
    )
    p.add_argument("workspace", help="workspace id or name")
    p.add_argument("-o", "--output", required=True, help="destination directory")
    p.set_defaults(func=cmd_export_workspace)

    p = sub.add_parser("import-file", help="Import a Markdown file as a page.")
    p.add_argument("workspace", help="workspace id or name")
    p.add_argument("parent", help="parent space or page (view_id or name)")
    p.add_argument("file")
    p.add_argument("--title")
    p.add_argument("--no-assets", action="store_true",
                   help="keep local image paths instead of uploading them")
    p.set_defaults(func=cmd_import_file)

    p = sub.add_parser(
        "import-dir", help="Recursively import a directory of Markdown files."
    )
    p.add_argument("workspace", help="workspace id or name")
    p.add_argument("parent", help="parent space or page (view_id or name)")
    p.add_argument("directory")
    p.add_argument("--no-assets", action="store_true",
                   help="keep local image paths instead of uploading them")
    p.set_defaults(func=cmd_import_dir)

    p = sub.add_parser(
        "save", help="Create a page from Markdown (a file, --content, or stdin)."
    )
    p.add_argument("workspace", help="workspace id or name")
    p.add_argument("parent", help="parent space or page (view_id or name)")
    p.add_argument("title")
    p.add_argument("--file", help="Markdown file to use as content")
    p.add_argument("--content", help="inline Markdown content")
    p.set_defaults(func=cmd_save)

    return parser


def main() -> None:
    args = build_parser().parse_args()
    try:
        if not getattr(args, "auth", True):
            args.func(args)
            return

        stored = False
        if client.email and client.password:
            ensure_authenticated()
        elif load_credentials():
            stored = True
        else:
            raise Exception(
                "Not authenticated. Run `appflowy-cli login`, or set "
                "APPFLOWY_EMAIL and APPFLOWY_PASSWORD."
            )

        refresh_before = client.token_store.get_refresh_token()
        try:
            resolve_args(args)
            args.func(args)
        finally:
            # GoTrue rotates refresh tokens; persist the new one or the
            # next run's refresh would fail.
            if stored and client.token_store.get_refresh_token() != refresh_before:
                save_credentials()
    except Exception as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()

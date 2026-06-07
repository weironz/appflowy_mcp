"""Command-line interface for AppFlowy.

A thin layer over the same client/converters the MCP server uses, aimed
at scriptable workflows (backups via cron, bulk import, quick lookups).

Authentication, in priority order:
1. APPFLOWY_EMAIL / APPFLOWY_PASSWORD environment variables (or .env)
2. tokens saved by `appflowy-cli login` in ~/.config/appflowy-cli/

Addressing follows AppFlowy's hierarchy. The workspace comes from
`appflowy-cli use <workspace>` (saved as the default) or the global
`-w/--workspace` flag; everything below it is a PATH of space/page
segments — names or UUIDs — resolved level by level, e.g. `demo/notes`.
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
)


# ==================== config & credentials ====================

def config_dir() -> Path:
    base = os.environ.get("XDG_CONFIG_HOME", "~/.config")
    return Path(base).expanduser() / "appflowy-cli"


def credentials_path() -> Path:
    return config_dir() / "credentials.json"


def config_path() -> Path:
    return config_dir() / "config.json"


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


def load_config() -> dict:
    try:
        return json.loads(config_path().read_text(encoding="utf-8"))
    except Exception:
        return {}


def save_config(config: dict) -> None:
    config_path().parent.mkdir(parents=True, exist_ok=True)
    config_path().write_text(json.dumps(config, indent=2), encoding="utf-8")


# ==================== addressing ====================

UUID_RE = re.compile(
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
    r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
)


def resolve_workspace(token: str) -> dict:
    """Accept a workspace id or name; return {workspace_id, workspace_name}."""
    workspaces = response_data(client._request("GET", "/api/workspace"))
    if UUID_RE.match(token):
        for ws in workspaces:
            if ws.get("workspace_id") == token:
                return ws
        return {"workspace_id": token, "workspace_name": token}
    matches = [
        ws for ws in workspaces
        if (ws.get("workspace_name") or "").casefold() == token.casefold()
    ]
    if len(matches) == 1:
        return matches[0]
    names = ", ".join(sorted(ws.get("workspace_name", "?") for ws in workspaces))
    kind = "Ambiguous" if matches else "Unknown"
    raise Exception(f"{kind} workspace '{token}'. Available: {names}")


def current_workspace(args) -> str:
    """Workspace id from -w/--workspace, falling back to `use` default."""
    token = args.workspace or load_config().get("workspace")
    if not token:
        raise Exception(
            "No workspace selected. Run `appflowy-cli use <workspace>` "
            "or pass -w <workspace>."
        )
    return resolve_workspace(token)["workspace_id"]


def resolve_path(workspace_id: str, path: str | None) -> dict:
    """Walk PATH segment by segment from the workspace root.

    Each segment is a child name or view_id; returns the resolved view
    node (the workspace root view when PATH is empty).
    """
    body = client._request(
        "GET", f"/api/workspace/{workspace_id}/folder", params={"depth": 10}
    )
    node = response_data(body)
    for segment in [s for s in (path or "").split("/") if s]:
        children = node.get("children") or []
        matches = [c for c in children if c.get("view_id") == segment] or [
            c for c in children
            if (c.get("name") or "").casefold() == segment.casefold()
        ]
        if len(matches) == 1:
            node = matches[0]
            continue
        where = node.get("name") or "workspace root"
        if matches:
            listing = ", ".join(f"{c['name']} ({c['view_id']})" for c in matches)
            raise Exception(
                f"Ambiguous segment '{segment}' under {where} — use a "
                f"view_id: {listing}"
            )
        available = ", ".join(c.get("name", "?") for c in children) or "(none)"
        raise Exception(
            f"No '{segment}' under {where}. Children: {available}"
        )
    return node


# ==================== commands ====================

def emit(args, data, human):
    if args.json:
        print(json.dumps(data, indent=2, ensure_ascii=False))
    else:
        human(data)


def view_marker(view: dict) -> str:
    if view.get("is_space"):
        return "space"
    return {0: "page", 1: "grid", 2: "board", 3: "calendar", 4: "chat"}.get(
        view.get("layout", 0), f"layout{view.get('layout')}"
    )


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


def cmd_use(args):
    config = load_config()
    if not args.target:
        current = config.get("workspace")
        print(current or "(no default workspace; run `appflowy-cli use <workspace>`)")
        return
    workspace = resolve_workspace(args.target)
    config["workspace"] = workspace["workspace_name"]
    save_config(config)
    print(
        f"Default workspace: {workspace['workspace_name']} "
        f"({workspace['workspace_id']})"
    )


def cmd_workspaces(args):
    workspaces = response_data(client._request("GET", "/api/workspace"))
    default = load_config().get("workspace")

    def human(data):
        for ws in data:
            star = "*" if ws.get("workspace_name") == default else " "
            print(f"{star} {ws.get('workspace_id')}  {ws.get('workspace_name')}")

    emit(args, workspaces, human)


def cmd_ls(args):
    workspace_id = current_workspace(args)
    node = resolve_path(workspace_id, args.path)
    children = node.get("children") or []

    def human(data):
        if not data:
            print("(empty)")
        for child in data:
            print(f"{view_marker(child):8} {child.get('view_id')}  {child.get('name')}")

    emit(args, children, human)


def cmd_tree(args):
    workspace_id = current_workspace(args)
    node = resolve_path(workspace_id, args.path)

    def human(data):
        def walk(view, indent):
            print(f"{'  ' * indent}[{view_marker(view)}] {view.get('name')}  "
                  f"({view.get('view_id')})")
            for child in view.get("children") or []:
                walk(child, indent + 1)

        walk(data, 0)

    emit(args, node, human)


def cmd_search(args):
    workspace_id = current_workspace(args)
    body = client._request(
        "GET",
        f"/api/search/{workspace_id}",
        params={"query": args.query, "limit": args.limit},
    )
    hits = response_data(body)

    def human(data):
        if not data:
            print("(no results)")
        for hit in data:
            print(f"{hit.get('object_id')}  {hit.get('preview', '')!r}")

    emit(args, hits, human)


def cmd_export(args):
    workspace_id = current_workspace(args)
    node = resolve_path(workspace_id, args.path)
    is_root = not (args.path or "").strip("/")
    is_tree = is_root or node.get("is_space") or (node.get("children") or [])

    if is_tree:
        directory = Path(args.output).expanduser()
        if directory.exists() and any(directory.iterdir()):
            raise Exception(f"Destination directory is not empty: {directory}")
        directory.mkdir(parents=True, exist_ok=True)

        exported: list[str] = []
        warnings: list[str] = []
        roots = node.get("children") or [] if is_root else [node]
        export_views_to_directory(workspace_id, roots, directory, exported, warnings)
        result = {
            "path": str(directory),
            "exported_files": exported,
            "warnings": warnings,
        }

        def human(data):
            for file in data["exported_files"]:
                print(file)
            for warning in data["warnings"]:
                print(f"warning: {warning}", file=sys.stderr)
            print(f"{len(data['exported_files'])} files -> {data['path']}")

        emit(args, result, human)
        return

    # Leaf page -> a single Markdown file.
    name = node.get("name") or node["view_id"]
    markdown = fetch_page_markdown(workspace_id, node["view_id"], title=name)
    path = Path(args.output).expanduser()
    if path.is_dir():
        path = path / f"{name}.md"
    elif path.suffix.lower() not in {".md", ".markdown"}:
        path = path.with_name(path.name + ".md")
    path.parent.mkdir(parents=True, exist_ok=True)
    path = unique_path(path)
    path.write_text(markdown, encoding="utf-8")
    emit(args, {"page_id": node["view_id"], "name": name, "path": str(path)},
         lambda d: print(d["path"]))


def cmd_import(args):
    workspace_id = current_workspace(args)
    node = resolve_path(workspace_id, args.path)
    if not (args.path or "").strip("/"):
        raise Exception(
            "Import needs a space or page as PATH; importing directly under "
            "the workspace root would create spaces. Run `appflowy-cli ls` "
            "and pick a space."
        )
    local = Path(args.local).expanduser()
    importer = MarkdownImporter(client, workspace_id)
    if local.is_dir():
        summary = importer.import_directory(
            str(local), node["view_id"], upload_assets=not args.no_assets
        )
    else:
        summary = importer.import_file(
            str(local), node["view_id"],
            title=args.title, upload_assets=not args.no_assets,
        )

    def human(data):
        for page in data.get("created_pages", []):
            print(f"{page.get('view_id')}  {page.get('title')}")
        for warning in data.get("warnings", []):
            print(f"warning: {warning}", file=sys.stderr)
        print(f"{len(data.get('created_pages', []))} pages imported")

    emit(args, summary, human)


def cmd_save(args):
    workspace_id = current_workspace(args)
    node = resolve_path(workspace_id, args.path)
    if not (args.path or "").strip("/"):
        raise Exception(
            "Save needs a space or page as PATH; a page created directly "
            "under the workspace root would become a space."
        )
    if args.file:
        content = Path(args.file).expanduser().read_text(encoding="utf-8")
    else:
        content = args.content if args.content is not None else sys.stdin.read()
    blocks = parse_content_to_blocks(content, "markdown")
    page = create_page_with_blocks(workspace_id, node["view_id"], args.title, blocks)
    emit(args, page, lambda d: print(d.get("view_id", d)))


# ==================== entry point ====================

def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="appflowy-cli",
        description=(
            "AppFlowy command-line client. Select a workspace once with "
            "`use`, then address spaces and pages by PATH, e.g. `demo/notes`."
        ),
    )
    parser.add_argument(
        "--version", action="version", version=version("appflowy-mcp")
    )
    parser.add_argument(
        "--json", action="store_true", help="Print raw JSON instead of summaries."
    )
    parser.add_argument(
        "-w", "--workspace",
        help="workspace id or name (overrides the `use` default)",
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

    p = sub.add_parser("use", help="Set (or show) the default workspace.")
    p.add_argument("target", nargs="?", help="workspace id or name")
    p.set_defaults(func=cmd_use)

    p = sub.add_parser("workspaces", help="List workspaces (* = default).")
    p.set_defaults(func=cmd_workspaces)

    p = sub.add_parser("ls", help="List children of a space/page PATH.")
    p.add_argument("path", nargs="?", help="e.g. demo or demo/notes (default: root)")
    p.set_defaults(func=cmd_ls)

    p = sub.add_parser("tree", help="Print the view tree under PATH.")
    p.add_argument("path", nargs="?", help="space/page path (default: root)")
    p.set_defaults(func=cmd_tree)

    p = sub.add_parser("search", help="Full-text search the workspace.")
    p.add_argument("query")
    p.add_argument("--limit", type=int, default=10)
    p.set_defaults(func=cmd_search)

    p = sub.add_parser(
        "export",
        help="Export PATH as Markdown: a leaf page becomes one .md file, "
        "a space/subtree becomes a directory, no PATH exports the whole "
        "workspace.",
    )
    p.add_argument("path", nargs="?", help="space/page path (default: whole workspace)")
    p.add_argument("-o", "--output", required=True,
                   help="destination file or directory")
    p.set_defaults(func=cmd_export)

    p = sub.add_parser(
        "import",
        help="Import local Markdown under PATH: a local file becomes one "
        "page, a local directory becomes a page subtree.",
    )
    p.add_argument("path", help="destination space/page path")
    p.add_argument("local", help="local .md file or directory")
    p.add_argument("--title", help="page title for single-file import")
    p.add_argument("--no-assets", action="store_true",
                   help="keep local image paths instead of uploading them")
    p.set_defaults(func=cmd_import)

    p = sub.add_parser(
        "save", help="Create a page under PATH from Markdown "
        "(a file, --content, or stdin)."
    )
    p.add_argument("path", help="parent space/page path")
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

# AppFlowy MCP

An MCP server and CLI for [AppFlowy](https://github.com/appflowy-io/appflowy): **128 tools** covering workspaces, spaces, pages, document content (incl. collab-level editing with snapshot/restore), Markdown import & export, PDF/ZIP export, databases & rows, search, members and invitations, sharing, comments, publishing, notifications, file upload, and AI chat & meeting transcription. Distributed as a **single static Rust binary** — the same `appflowy` binary is the MCP server (`appflowy serve`) and a full CLI.

[![release](https://img.shields.io/github/v/release/weironz/appflowy_mcp)](https://github.com/weironz/appflowy_mcp/releases)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

## Quick start

Install (details and more platforms: [docs/installation.md](docs/installation.md)):

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.sh | sh

# Windows (PowerShell)
irm https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.ps1 | iex
```

Wire up an MCP client ([docs/mcp-connect.md](docs/mcp-connect.md) for Codex / Claude Desktop / Cursor, HTTP mode, and troubleshooting):

```bash
claude mcp add --scope user appflowy \
  -e APPFLOWY_EMAIL=you@example.com \
  -e APPFLOWY_PASSWORD=your-password \
  -- appflowy serve
```

Or drive it from a shell:

```bash
appflowy login
appflowy use my-workspace
appflowy export -o ./backup        # whole workspace → Markdown tree (incremental on reruns)
appflowy search "quarterly report"
```

## Highlights

- **Two personalities, one binary**: MCP server (stdio by default, Streamable HTTP with `--http`) and a scriptable CLI with workspace/PATH addressing.
- **Collab-faithful export**: whole-workspace Markdown export with inline formatting decoded natively from yjs collabs (tables, math, nested lists), incremental on reruns; server-side PDF export and owner-only workspace ZIP backup.
- **Full-fidelity import**: Markdown files & directories (images uploaded in parallel, sibling order preserved), CSV → database, doc/docx/html/pdf → pages, Notion `.zip` archives, all as resumable server-side tasks.
- **Writes where it matters**: pages, blocks, append (order-safe), collab-level **document editing** — replace a block's text, detach a block, snapshot and restore (CRDT-correct rebuild semantics) — plus database-row documents and row update/delete/restore.
- **Agent-grade errors**: typed errors keep HTTP status/code end-to-end; input validation returns `isError: true` tool results (MCP SEP-1303) so the model can self-correct.

## Documentation

| Where | What |
|---|---|
| [docs/installation.md](docs/installation.md) | Installers, manual/source install, uninstall, asset matrix |
| [docs/mcp-connect.md](docs/mcp-connect.md) | Client setup & troubleshooting (Codex, Claude, Cursor, HTTP) |
| [docs/cli.md](docs/cli.md) | CLI reference: auth chain, addressing, every command, cron/CI |
| [docs/tools.md](docs/tools.md) | The full 128-tool catalog |
| [docs/agent-guide.md](docs/agent-guide.md) | How an AI client should use the tools (paste-ready block + recipes) |
| [docs/development.md](docs/development.md) | Module map, design notes, adding a tool, release process |
| [docs/ROADMAP.md](docs/ROADMAP.md) | What's shipped and what's deliberately deferred |
| [docs/README.md](docs/README.md) | Docs index |

## Requirements & credentials

The server authenticates with environment variables (`.env` also works):

```text
APPFLOWY_EMAIL=you@example.com
APPFLOWY_PASSWORD=your-password
APPFLOWY_BASE_URL=https://beta.appflowy.cloud   # only for self-hosted
```

The CLI can use those, or persistent tokens from `appflowy login` (`~/.config/appflowy-cli/credentials.json`). Most write operations need a workspace plus a parent view id (`workspace → space → page`).

## Notes

- The original Python implementation was removed in v0.8.0 (see tag `v0.8.0` for those sources); PyPI packages are frozen at `appflowy-mcp` 0.8.0 / `appflowy-cli` 0.6.4.
- Some endpoints are implemented from AppFlowy source routes that are not in the public OpenAPI document; newer commercial-cloud capabilities degrade with a clear 404 or a documented fallback on older self-hosted images.
- Logs always go to stderr (stdout stays the JSON-RPC channel).

## License

[Apache License 2.0](LICENSE) — Copyright 2026 weironz.

# Connecting appflowy to Codex / Claude / Cursor (MCP)

`appflowy serve` exposes AppFlowy Cloud as **114 MCP tools** to any MCP client:
workspace/space/page CRUD, Markdown import & export, PDF export, databases and CSV import, row operations, publishing, document comments, page sharing, notifications, file upload, AI chat and meeting transcription. A typical workflow: **let an agent write conversations and notes straight into your AppFlowy workspace**, or export the whole workspace to Markdown for backup.

Install the binary first (one-line scripts: [installation.md](installation.md)), then follow the section for your client.

## Credentials: read this first (applies to every client)

The MCP server authenticates **only via environment variables** (or a `.env` file in its working directory):

```text
APPFLOWY_EMAIL=you@example.com
APPFLOWY_PASSWORD=your-password
APPFLOWY_BASE_URL=https://beta.appflowy.cloud   # only for self-hosted servers
```

Two pitfalls worth knowing up front:

- **The `~/.config/appflowy-cli/credentials.json` written by `appflowy login` is for the CLI only; the MCP server does not read it.** A server process launched by a client has no CLI login state — you must supply the env vars in the client configuration. Where env is genuinely impossible, calling the `appflowy_login` tool over MCP works too (but that puts credentials into chat history; think twice).
- **Never put env credentials into project-level config that git tracks** (e.g. a project's `.mcp.json` or `.cursor/mcp.json`) — once the repo goes public, your account credentials live in the commit history forever. Project-level config is only for credential-free servers; anything holding credentials belongs in **user-level** config (see each section below).

## Codex

One line (user scope, available in all projects):

```bash
codex mcp add appflowy \
  --env APPFLOWY_EMAIL=you@example.com \
  --env APPFLOWY_PASSWORD=your-password \
  -- appflowy serve
```

Equivalent handwritten config in `~/.codex/config.toml`:

```toml
[mcp_servers.appflowy]
command = "appflowy"
args = ["serve"]
env = { APPFLOWY_EMAIL = "you@example.com", APPFLOWY_PASSWORD = "your-password" }
```

Verify: start a new session and ask Codex to list your workspaces (it will call `appflowy_list_workspaces`).

## Claude Code

```bash
claude mcp add --scope user appflowy \
  -e APPFLOWY_EMAIL=you@example.com \
  -e APPFLOWY_PASSWORD=your-password \
  -- appflowy serve
```

### ⚠️ Do not drop `--scope user` — especially on Windows

Without it the entry lands in the default `local` (project) scope. On Windows, `claude mcp add` writes the project key with **forward slashes** (`C:/Users/you/proj`) while the app itself creates it with **backslashes** (`C:\Users\you\proj`) — the two never match, and the config is never loaded.

The symptom is treacherous: `claude mcp list` reports `✓ Connected` (it reads the forward-slash key it wrote itself), yet **after a restart there are no appflowy tools in the session at all** — neither side is lying, they are just looking at different configs. Don't trust `✓ Connected`; trust "are `appflowy_*` tools present after a restart" (reproduced on a sibling project in 2026).

`--scope user` writes to the top-level `mcpServers` in `~/.claude.json`, bypassing the project key entirely.

To rotate credentials: re-run `claude mcp add` — the same-named entry is overwritten and other MCP servers are unaffected.

### Claude Desktop

In `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "appflowy": {
      "command": "appflowy",
      "args": ["serve"],
      "env": {
        "APPFLOWY_EMAIL": "you@example.com",
        "APPFLOWY_PASSWORD": "your-password"
      }
    }
  }
}
```

If the binary is not on your PATH, use an absolute path (`C:\Users\you\AppData\Local\Appflowy\bin\appflowy.exe`). If you installed the `appflowy-mcp` name as well, `args` can be omitted entirely (it launches straight into MCP mode).

## Cursor

User-level `~/.cursor/mcp.json` (visible in every project — recommended for credentials):

```json
{
  "mcpServers": {
    "appflowy": {
      "command": "appflowy",
      "args": ["serve"],
      "env": {
        "APPFLOWY_EMAIL": "you@example.com",
        "APPFLOWY_PASSWORD": "your-password"
      }
    }
  }
}
```

The project-level `.cursor/mcp.json` has the same shape, but **that file is git-tracked — never put account credentials in it** (see "Credentials" above). Check Settings → MCP to confirm it loaded; verify by asking Cursor to count the tools (should be 114).

## Need HTTP instead of stdio? (rare)

All four clients above **spawn a local process and talk stdio** — the standard for desktop agents. Streamable HTTP is only for "the client is on another machine / several people share one instance":

```bash
appflowy serve --http 127.0.0.1:8000                 # local dev tunnel
appflowy serve --http 0.0.0.0:8000 --allow-remote    # internal network (read below)
```

The endpoint is `http://ADDR/mcp`; HTTP-capable clients point straight at the URL. **Guardrails**: loopback Hosts only and browser Origins rejected by default; binding a non-loopback address requires `--allow-remote` — anyone who can reach that port can drive all 114 tools with your AppFlowy credentials. There is no OAuth yet, so put it behind a trusted network or a gateway you control.

## Troubleshooting

- **`✓ Connected` but no `appflowy_*` tools in the session**: the Claude Code project-key slash trap (see above) — re-add with `--scope user`. For Cursor/Desktop, check the JSON is valid (trailing commas) and that `command` resolves on PATH (use an absolute path otherwise).
- **Every tool call fails 401 / Not authenticated**: the env never reached the server process — wrong key names in the client config (`APPFLOWY_EMAIL`/`APPFLOWY_PASSWORD` exactly), or you were counting on `credentials.json` (the server does not read it, see "Credentials"). **Restart the client** after changing config.
- **404s or odd errors on self-hosted servers**: `APPFLOWY_BASE_URL` points at an old image; some endpoints (comment v2, share details, meeting transcription, …) only exist on the newer commercial cloud — the server degrades or reports a clear 404. Upgrade the self-hosted image, or use the official cloud.
- **Watch the protocol traffic**: all logs go to stderr (stdout is the JSON-RPC channel and stays clean); add `RUST_LOG=debug` to the client's env.
- **Verify the tool set**: ask the client to run `appflowy_list_workspaces` — it should return your workspaces. Then count the tools: it should be **114** (older binaries report 81; fewer than that means an old release is being launched).

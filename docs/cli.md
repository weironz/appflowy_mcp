# appflowy CLI reference

`appflowy` is the command-line client for AppFlowy Cloud, built for scripts and cron — whole-workspace backups, bulk import, quick lookups. It is **the same binary, same HTTP client, same import/export engine** as the MCP server (`appflowy serve`): MCP talks to AI agents, the CLI talks to shells and CI. Install it via [installation.md](installation.md).

## Authentication chain (exactly two sources, in order)

1. `APPFLOWY_EMAIL` + `APPFLOWY_PASSWORD` in the environment (optionally from a `.env` file; `APPFLOWY_BASE_URL` for self-hosted)
2. The credentials file saved by `appflowy login` (below)

If neither exists: `error: Not authenticated. Run 'appflowy-cli login', or set APPFLOWY_EMAIL and APPFLOWY_PASSWORD.` with exit code 1.

```bash
appflowy login                     # prompts for email/password; stores tokens, never the password
appflowy logout                    # deletes the credentials file
```

Credentials file: `~/.config/appflowy-cli/credentials.json` (mode 600 on Unix; holds email, base_url, access token, refresh token). **GoTrue rotates refresh tokens**: after each command, a changed refresh token is persisted back — so a long-idle install keeps working. `appflowy login --base-url <url>` targets a self-hosted server (precedence: env var > saved file).

> This file **only serves the CLI**; the MCP server (`appflowy serve`) does not read it. See "Credentials" in [mcp-connect.md](mcp-connect.md).

## Addressing model

The hierarchy is `workspace → space → page`, with two rules:

- Pick a workspace once with `appflowy use <name-or-uuid>`; every command also accepts `-w` for a one-off override. Names match **case-insensitively**, and ambiguity prints all candidates.
- Below the workspace, spaces and pages are addressed by **PATH**: `/`-separated segments, each a **name or view_id** (UUID), resolved level by level. Matching tries the UUID exactly first, then a case-insensitive name. Ambiguity errors include a `name (view_id)` disambiguation list.

```bash
appflowy use AI              # default workspace
appflowy ls                  # workspace root (spaces)
appflowy ls LLM-notes        # children of a space
appflowy tree LLM-notes/concepts
appflowy -w test ls          # one-off workspace override
```

## Global options

| Option | Effect |
| --- | --- |
| `--json` | Raw JSON output (for scripts/agents; default output is human summaries) |
| `-w, --workspace <name-or-id>` | Override the `use` default |
| `-h, --help` / `-V, --version` | |

## Commands

### `login` / `logout` — credentials

See above. `login --email/--password` supplies them non-interactively (mind your shell history). On Windows, password input is **echoed unmasked** (a known limitation, see ROADMAP).

### `use [name]` — default workspace

With no argument it prints the current default; with an argument it resolves and saves it (the name is stored, the id is printed). Note it passes the authentication chain (it needs the candidate list).

### `workspaces` — list workspaces

`*` marks the default. `--json` returns the full fields (role/member_count/created_at/…).

### `ls [PATH]` / `tree [PATH]` — structure

`ls` prints only the direct children of PATH (typed: space/page/grid/board/calendar/chat); `tree` prints the whole subtree. Without PATH, the workspace root.

### `search <query> [--limit N]` — full-text search

```bash
appflowy search "transformer" --limit 5
```

Hits list object_id + preview snippet. Freshly written content appears only after the server finishes indexing it.

### `export [PATH] -o <dest>` — Markdown export (portable backup)

| PATH is | Result |
| --- | --- |
| omitted | Entire workspace → directory tree |
| a space / a page with children | That subtree → directory tree (a page with children becomes a folder whose own text is `README.md`) |
| a leaf page | A single `.md` file |

```bash
appflowy export -o ./backup           # whole workspace → ./backup/
appflowy export LLM-notes -o ./llm    # one subtree
appflowy export LLM-notes/concepts/Transformer -o t.md
```

- Inline formatting (bold/italic/code/links) is preserved by decoding the yjs collab directly; tables, math, nested lists and todos are supported.
- Pages that fail to fetch (missing collab, errors) are reported as warnings **without aborting**; non-document views (grid/board/calendar) are skipped and reported.
- The destination of a tree export **must not exist or be empty** (anti-overwrite guard); fetching is bounded at 8 concurrent pages.
- This produces **Markdown**, not the official backup format. For the official ZIP backup see the MCP tool `appflowy_export_workspace_zip` (owner-only, delivered by email) — [mcp-connect.md](mcp-connect.md) / README.

### `import <PATH> <local> [--no-assets] [--title T]` — Markdown import

```bash
appflowy import demo ./notes          # local directory → page subtree
appflowy import demo note.md          # single file → one page
```

- Mapping: directory → page, subdirectory → subpage; `README.md`/`index.md` content merges into its directory page instead of becoming a separate child.
- Local images are resolved relative to the Markdown file, **deduplicated and uploaded in parallel** (up to 8), and their URLs replaced with AppFlowy file URLs; remote images are kept; missing images become warnings without aborting.
- `--no-assets` builds the structure without uploading images.
- Pages are created in document order (sibling order stays deterministic); tables, nested lists, math blocks and front-matter are supported.
- PATH must not be the workspace root (a direct child of the root would become a space) — the error message points you to `ls` to pick a space first.

### `save <PATH> <title> [--file F | --content C]` — quick page

```bash
echo "# Note" | appflowy save demo "Title"      # stdin
appflowy save demo "Title" --file note.md
appflowy save demo "Title" --content "# Inline"
```

The CLI version of "save this AI answer into AppFlowy"; content is parsed as Markdown by default.

### `serve [--http ADDR] [--allow-remote]` — MCP server

```bash
appflowy serve                               # stdio (what client configs use)
appflowy serve --http 127.0.0.1:8000         # Streamable HTTP @ /mcp
appflowy serve --http 0.0.0.0:8000 --allow-remote
```

Renaming/copying the binary to `appflowy-mcp` makes it start straight in serve mode (compatible with configs written for the old `appflowy-mcp` entry point). Guardrails and the risks of sharing are covered in [mcp-connect.md](mcp-connect.md).

## CI / cron example

```cron
0 3 * * * APPFLOWY_EMAIL=... APPFLOWY_PASSWORD=... /home/me/.local/bin/appflowy -w AI export -o ~/backups/appflowy-$(date +\%F)
```

Notes: with env credentials the run is fully stateless (nothing written to disk); `--json` for piping into jq; failures always exit non-zero with `error: ...` on stderr.

## Configuration files at a glance

| File | Contents |
| --- | --- |
| `~/.config/appflowy-cli/credentials.json` | CLI login tokens (mode 600) |
| `~/.config/appflowy-cli/config.json` | default workspace saved by `use` |
| `.env` in the startup directory | `APPFLOWY_EMAIL/PASSWORD/BASE_URL` |

## Troubleshooting

- `Not authenticated...`: neither credential source is present — see "Authentication chain".
- `Unknown/Ambiguous workspace 'x'. Available: ...`: name mistyped or duplicated; use the printed UUID.
- `No 'seg' under X. Children: ...` / `Ambiguous segment ... — use a view_id`: a PATH segment matched nothing or several things; take a view_id from the printed list.
- `Destination directory is not empty`: the export guard; choose an empty directory.
- Mojibake in Windows consoles: the CLI always emits UTF-8; switch the console to code page 65001 (`chcp 65001`) or use Windows Terminal. The CLI will not adopt console encodings.

## Related docs

- Install / uninstall / one-line scripts: [installation.md](installation.md)
- Connecting MCP clients (Codex/Claude/Cursor): [mcp-connect.md](mcp-connect.md)
- The full 114-tool catalog: [README.md](../README.md) at the repository root

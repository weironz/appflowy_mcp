# appflowy-cli

Command-line client for [AppFlowy](https://github.com/appflowy-io/appflowy): export, import, search, and manage workspaces from scripts or cron.

```bash
uvx appflowy-cli workspaces
uvx appflowy-cli export-workspace <workspace-id> -o ./backup
echo "# Note" | uvx appflowy-cli save <workspace-id> <parent-id> "Title"
```

Authenticate with `uvx appflowy-cli login` (saves session tokens, never the password, to `~/.config/appflowy-cli/`), or via `APPFLOWY_EMAIL` / `APPFLOWY_PASSWORD` / `APPFLOWY_BASE_URL` (or a `.env` file). All commands accept `--json`.

This is a thin wrapper; the implementation lives in [`appflowy-mcp`](https://pypi.org/project/appflowy-mcp/). Full documentation: https://github.com/weironz/appflowy_mcp

# appflowy-cli

Command-line client for [AppFlowy](https://github.com/appflowy-io/appflowy): export, import, search, and manage workspaces from scripts or cron.

```bash
uvx appflowy-cli login        # once; saves session tokens (never the password)
uvx appflowy-cli use test     # pick a default workspace
uvx appflowy-cli ls demo      # then address spaces/pages by path
uvx appflowy-cli export demo/notes -o note.md
uvx appflowy-cli export -o ./backup        # whole workspace
echo "# Note" | uvx appflowy-cli save demo "Title"
```

Commands: `login`, `logout`, `use`, `workspaces`, `ls`, `tree`, `search`, `export`, `import`, `save`. All accept `--json`; `-w <workspace>` overrides the default per command. Environment variables `APPFLOWY_EMAIL` / `APPFLOWY_PASSWORD` / `APPFLOWY_BASE_URL` are also supported and take priority.

This is a thin wrapper; the implementation lives in [`appflowy-mcp`](https://pypi.org/project/appflowy-mcp/). Full documentation: https://github.com/weironz/appflowy_mcp

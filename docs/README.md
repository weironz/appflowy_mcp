# Documentation

| Doc | What it covers |
|---|---|
| [installation.md](installation.md) | One-line installers (Linux/macOS/Windows), manual & source install, asset availability, uninstall, troubleshooting |
| [mcp-connect.md](mcp-connect.md) | Connecting MCP clients — Codex / Claude Code / Claude Desktop / Cursor (stdio), Streamable HTTP mode, credential rules, troubleshooting |
| [cli.md](cli.md) | Full CLI reference — auth chain, addressing model, every command, CI/cron examples |
| [tools.md](tools.md) | The complete 128-tool catalog, grouped by domain with input semantics |
| [development.md](development.md) | Build/test layout, module map, design notes (errors, envelope unwrap, concurrency, CRDT restore semantics), how to add a tool, release process |
| [ROADMAP.md](ROADMAP.md) | Shipped history + what is deliberately deferred and why |

Quick starts in a hurry:

```bash
curl -fsSL https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.sh | sh   # Linux/macOS
irm https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.ps1 | iex        # Windows PowerShell
appflowy login
claude mcp add --scope user appflowy \
  -e APPFLOWY_EMAIL=you@example.com -e APPFLOWY_PASSWORD=your-password -- appflowy serve
```

Then keep going with [mcp-connect.md](mcp-connect.md) for your specific client.

# appflowy-cli (deprecated)

**Deprecated as of v0.8.0.** The Python `appflowy-cli` package is superseded by the Rust rewrite shipped in this repository: a single `appflowy` binary implements the same CLI (`login`, `logout`, `use`, `workspaces`, `ls`, `tree`, `search`, `export`, `import`, `save`, same `--json`/`-w` flags, same credential file at `~/.config/appflowy-cli/`) plus the MCP server (`appflowy serve`).

- Install: download the binary from [GitHub Releases](https://github.com/weironz/appflowy_mcp/releases), or `cargo build --release` inside `appflowy-rs/`.
- The PyPI package [`appflowy-cli`](https://pypi.org/project/appflowy-cli/) (final: **0.6.4**) remains installable but receives no further updates.
- Migration: replace `uvx appflowy-cli` with `appflowy` in scripts/cron; command lines are unchanged otherwise.

Legacy quick reference (Python package, frozen):

```bash
uvx appflowy-cli login
uvx appflowy-cli use test
uvx appflowy-cli export -o ./backup
```

Full documentation: https://github.com/weironz/appflowy_mcp

# Installing appflowy (one-line script / manual / from source)

`appflowy` is a single static binary: the same file is both the CLI and the MCP server (`appflowy serve`; renamed or copied to `appflowy-mcp` it starts in MCP mode directly). This page covers all three installation paths; the first one is recommended for regular use.

## One-line install (recommended)

Linux / macOS (bash/sh):

```bash
curl -fsSL https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.ps1 | iex
```

> If the execution policy blocks the script, bypass it for the current process only:
> `Set-ExecutionPolicy Bypass -Scope Process -Force; irm ... | iex`

**Updating**: once installed, `appflowy update` self-updates the binary from the latest GitHub release (`--version X.Y.Z` pins, `--check` only reports) — no need to re-run the installer. Re-running the one-liner works too (it installs the latest release over the existing binary). Pinning a specific version:

```bash
APPFLOWY_VERSION=0.8.1 curl -fsSL https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.sh | sh
```
```powershell
$env:APPFLOWY_VERSION='0.8.1'; irm https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.ps1 | iex
```

Custom install directory (defaults are `~/.local/bin` and `%LOCALAPPDATA%\Appflowy\bin`): `APPFLOWY_BIN_DIR`.

### What the script actually does (no black box)

1. Maps the platform to a release asset: Linux x64 → `appflowy-linux-x64`, macOS arm64 → `appflowy-macos-aarch64`, Windows x64 → `appflowy-windows-x64.exe`.
2. Resolves the version: an explicit `APPFLOWY_VERSION` wins, otherwise the latest GitHub release is used.
3. Downloads to a temp file and **runs `--version` on the download before installing it** — a truncated archive or a wrong-architecture binary fails there, not after installation.
4. Installs two names: `appflowy` (the CLI) and `appflowy-mcp` (the same bytes; argv[0] dispatch means MCP clients can point at it directly, no `serve` argument needed).
5. On Windows it prepends the bin directory to the user PATH (open a **new** terminal afterwards); on Linux/macOS it prints a hint if the directory is not on your PATH.

### ⚠️ Prebuilt asset availability (important)

Release assets are built by CI **on tag push** and currently cover **Windows x64 / Linux x64 / macOS arm64 (Apple Silicon)**. v0.8.0 shipped the Windows asset only, and **Intel Macs (x86_64) have no prebuilt asset** — GitHub's retired-generation Intel runner pool starves that CI job for 40+ minutes, so it was dropped from the matrix. On an Intel Mac, install from source (`cargo build --release`, see below).

## Uninstall

Remove the two binaries (and optionally the config directory holding the CLI login tokens):

```bash
rm -f ~/.local/bin/appflowy ~/.local/bin/appflowy-mcp
rm -rf ~/.config/appflowy-cli        # optional: saved credentials
```
```powershell
rm "$env:LOCALAPPDATA\Appflowy\bin\appflowy*.exe"
rm -r "$env:USERPROFILE\.config\appflowy-cli"   # optional
```

## Manual install

Download the `appflowy-*` asset for your platform from [GitHub Releases](https://github.com/weironz/appflowy_mcp/releases), make it executable, and put it anywhere on your PATH. Copy it once more under the name `appflowy-mcp` if you want the argv[0] MCP entry point.

## From source

Requires a Rust toolchain ([rustup](https://rustup.rs/)):

```bash
git clone https://github.com/weironz/appflowy_mcp.git
cd appflowy_mcp
cargo build --release        # output: target/release/appflowy (appflowy.exe on Windows)
```

Pure rustls with bundled webpki root certificates — no OpenSSL dependency, friendly to cross-compiling.

## Troubleshooting

- **Download 404**: no asset for that platform yet (see "Prebuilt asset availability"), or a mistyped tag — check `APPFLOWY_VERSION`, or fall back to manual/source install.
- **Windows SmartScreen warning**: the binary is not code-signed; this is expected. Click "More info → Run anyway". Managed machines may need an IT exception.
- **`appflowy` not found after install**: on Windows, open a **new** terminal (PATH changes only apply to new processes). On Linux/macOS add `~/.local/bin` to your PATH as the script suggests.
- **Corporate proxy / slow GitHub**: the scripts do not handle proxies or mirrors; download manually, or `cargo build`.
- **Sanity check**: `appflowy --version` prints `appflowy-cli 0.x.y` — the `-cli` suffix in the version string is a leftover from the historical package name, not the wrong binary.
- **`appflowy update` fails with "not writable"**: the install dir belongs to another user/root — fix permissions, or re-run the install script with the right privileges. A download 404 means no asset for that platform in that release (see "Prebuilt asset availability").

## Next steps

- Connect MCP clients (Codex / Claude / Cursor): [mcp-connect.md](mcp-connect.md)
- Full CLI reference: [cli.md](cli.md)

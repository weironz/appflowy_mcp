# appflowy one-line installer (Windows PowerShell):
#   irm https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.ps1 | iex
#
# Optional env:
#   $env:APPFLOWY_VERSION='0.9.0'   pin a release (default: latest)
#   $env:APPFLOWY_BIN_DIR=...       install dir (default: %LOCALAPPDATA%\Appflowy\bin)
#
# Downloads the prebuilt x64 binary from GitHub Releases, verifies it runs
# `--version` before installing, installs TWO names (appflowy.exe the CLI,
# appflowy-mcp.exe the MCP entry point), and prepends the bin dir to the
# user PATH. Re-running updates to the latest/pinned release.
$ErrorActionPreference = 'Stop'

$Repo   = 'weironz/appflowy_mcp'
$BinDir = if ($env:APPFLOWY_BIN_DIR) { $env:APPFLOWY_BIN_DIR } else { "$env:LOCALAPPDATA\Appflowy\bin" }

function Fail($msg) { Write-Host "install: $msg" -ForegroundColor Red; exit 1 }

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
    Fail "prebuilt Windows binaries are x64 only; build from source instead (docs/installation.md)"
}

if ($env:APPFLOWY_VERSION) {
    $Tag = $env:APPFLOWY_VERSION
    if (-not $Tag.StartsWith('v')) { $Tag = "v$Tag" }
} else {
    try {
        $Tag = (Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest").tag_name
    } catch { Fail "could not resolve the latest release tag from GitHub: $_" }
    if (-not $Tag) { Fail "could not resolve the latest release tag from GitHub" }
}
Write-Host "install: appflowy-windows-x64 @ $Tag"

$Url = "https://github.com/$Repo/releases/download/$Tag/appflowy-windows-x64.exe"
$Tmp = Join-Path $env:TEMP ("appflowy-install-" + [guid]::NewGuid() + ".exe")
try {
    Invoke-WebRequest -Uri $Url -OutFile $Tmp -UseBasicParsing
} catch {
    Fail "download failed: $Url`ninstall: check the asset exists and GitHub is reachable; fallback: manual download / cargo build"
}

try {
    $Got = & $Tmp --version 2>$null
} catch {
    Fail "downloaded file failed to run --version (truncated or wrong-arch binary)"
}
Write-Host "install: verified ($Got)"

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
Copy-Item $Tmp (Join-Path $BinDir 'appflowy.exe') -Force
Copy-Item $Tmp (Join-Path $BinDir 'appflowy-mcp.exe') -Force
Remove-Item $Tmp -Force

$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($UserPath -split ';') -notcontains $BinDir) {
    [Environment]::SetEnvironmentVariable('Path', "$BinDir;$UserPath", 'User')
    Write-Host "install: added $BinDir to your user PATH (open a NEW terminal to pick it up)"
}

Write-Host "install: done -> $BinDir\appflowy.exe (+ appflowy-mcp.exe)"
Write-Host "next: appflowy login    # or set APPFLOWY_EMAIL / APPFLOWY_PASSWORD"
Write-Host "next: connect an MCP client -> https://github.com/$Repo/blob/main/docs/mcp-connect.md"

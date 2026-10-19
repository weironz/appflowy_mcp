# appflowy-mcp local dev + release flow (replaces CI when GitHub Actions
# quota is unavailable; the workflow stays as manual dispatch backup).

set shell := ["bash", "-uc"]

cargo_bin := env("HOME") + "/.cargo/bin/cargo"
zigbuild  := env("HOME") + "/.cargo/bin/cargo-zigbuild"

# -------- recipes --------

# default: list everything
default:
    @just --list

# -------- dev gates (identical to CI) --------

test:
    "{{cargo_bin}}" test

clippy:
    "{{cargo_bin}}" clippy --all-targets -- -D warnings

fmt:
    "{{cargo_bin}}" fmt --all

# both gates in order
gate: test clippy

# quick stdio ping of the MCP server
smoke:
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"just","version":"0"}}}' | "{{cargo_bin}}" run --quiet -- serve | head -c 200 && echo

# -------- builds --------

# host (Windows x64) artifacts -> dist/
build:
    "{{cargo_bin}}" build --release
    mkdir -p dist
    cp target/release/appflowy.exe dist/appflowy-windows-x64.exe
    cp dist/appflowy-windows-x64.exe dist/appflowy-mcp-windows-x64.exe
    ls -la dist

# Linux x64 via zigbuild (skip with a warning if not installed)
build-linux:
    if [ -f "{{zigbuild}}" ]; then \
      "{{cargo_bin}}" zigbuild --release --target x86_64-unknown-linux-gnu && \
      mkdir -p dist && cp target/x86_64-unknown-linux-gnu/release/appflowy dist/appflowy-linux-x64 && \
      ls -la dist/appflowy-linux-x64; \
    else echo "skip: cargo-zigbuild not found (install via `cargo install cargo-zigbuild`)"; fi

# macOS arm64 via zigbuild (best effort; needs a machO-capable zig toolchain)
build-macos:
    -if [ -f "{{zigbuild}}" ]; then "{{cargo_bin}}" zigbuild --release --target aarch64-apple-darwin && mkdir -p dist && cp target/aarch64-apple-darwin/release/appflowy dist/appflowy-macos-aarch64; fi || echo "skip: macOS cross-build failed (missing SDK/toolchain for aws-lc); work around: build on a mac"

build-artifacts: build build-linux build-macos

# -------- release --------

# derive plain X.Y.Z from the tag (must be vX.Y.Z)
_plain_tag TAG:
    @echo "{{TAG}}" | sed -n 's/^v\([0-9][0-9]*\(\.[0-9][0-9]*\)*\)$/\1/p' | head -1

# version bump in Cargo.toml + Cargo.lock, commit only the bump
bump TAG:
    plain=$(just _plain_tag "{{TAG}}"); [ -n "$plain" ] || { echo "bad tag: {{TAG}} (expect vX.Y.Z)"; exit 1; }; \
    sed -i -E '0,/^version = "[0-9]+\.[0-9]+\.[0-9]+"/s/^version = "[0-9]+\.[0-9]+\.[0-9]+"/version = "'"$plain"'"/' Cargo.toml && \
    "{{cargo_bin}}" check --quiet && \
    git add Cargo.toml Cargo.lock && \
    git commit -m "Bump version to $plain for {{TAG}}"

# pre-flight: version matches, tag/release absent, tree clean
preflight TAG:
    plain=$(just _plain_tag "{{TAG}}"); [ -n "$plain" ] || { echo "bad tag: {{TAG}}"; exit 1; }; \
    current=$(grep -m1 '^version' Cargo.toml | cut -d '"' -f2); \
    [ "$current" = "$plain" ] || { echo "Cargo.toml says $current, tag is {{TAG}} — run \`just bump {{TAG}}\` first"; exit 1; }; \
    if git rev-parse -q --verify "refs/tags/{{TAG}}" >/dev/null 2>&1 || gh release view "{{TAG}}" >/dev/null 2>&1; then \
      echo "{{TAG}} already exists"; exit 1; fi; \
    git diff --quiet && git diff --cached --quiet || { echo "working tree not clean"; exit 1; }

# full local release: preflight -> gate -> artifacts -> tag+push -> gh release
release TAG: (preflight TAG) gate build-artifacts
    git tag "{{TAG}}" && \
    git push origin main && \
    git push origin "{{TAG}}" && \
    gh release create "{{TAG}}" dist/appflowy-* --latest --generate-notes -t "{{TAG}}" && \
    gh release view "{{TAG}}" --json url,assets --jq '"\(.url)\nassets: \([.assets[].name] | join(", "))"'

# one command short-hand: bump + release
ship TAG: (bump TAG) (release TAG)

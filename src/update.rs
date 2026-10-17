//! `appflowy update`: self-update the binary from GitHub Releases.
//!
//! Mirrors install.sh / install.ps1: resolve a release tag (latest, or
//! pinned via --version / APPFLOWY_VERSION), download the platform asset,
//! run `--version` on the download as a smoke test, then swap it in at the
//! running executable's path. The installers drop two names of the same
//! bytes (`appflowy` + the `appflowy-mcp` MCP alter ego), so the sibling is
//! re-synced afterwards — both entry points land on the new version.
//!
//! Replacement mechanics: on Unix a rename over the running binary is atomic
//! (the process keeps the old inode). On Windows a running exe cannot be
//! overwritten but CAN be renamed, so the old image is moved to
//! `<file>.old`, the new one moved in, and the old one removed best-effort —
//! it stays locked until this process exits and is swept by `cleanup_stale`
//! on the next update run.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use serde_json::{json, Value};

use crate::error::{Error, Result};

/// The repo whose Releases feed the updater. The HTTP functions take their
/// base URLs as parameters (instead of using these constants directly) so
/// tests can point them at an `httpmock` server.
const REPO: &str = "weironz/appflowy_mcp";
const API_BASE: &str = "https://api.github.com";
const DOWNLOAD_BASE: &str = "https://github.com";

/// Version of the running binary (what `--version` prints after `appflowy-cli`).
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Args)]
pub struct UpdateArgs {
    /// Pin the release to install (default: the latest release; also read
    /// from APPFLOWY_VERSION, matching the install scripts).
    #[arg(long, value_name = "X.Y.Z")]
    pub version: Option<String>,
    /// Only check whether an update exists; download and replace nothing.
    #[arg(long)]
    pub check: bool,
}

/// Strict X.Y.Z comparison without pulling in a semver crate (release tags
/// are always `vX.Y.Z`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Parse `0.9.0` / `v0.9.0` / `0.9.0-rc.1`, rejecting anything without all
/// three numeric components.
fn parse_version(raw: &str) -> Option<Version> {
    let raw = raw.trim().trim_start_matches(['v', 'V']);
    let core = raw.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version(major, minor, patch))
}

/// Extract the trailing version token from `--version` output
/// (`appflowy-cli 0.9.0`).
fn version_from_version_output(output: &str) -> Option<Version> {
    parse_version(output.split_whitespace().last()?)
}

/// Map (os, arch) to the release asset name, mirroring the install scripts.
/// Intel Macs map to `appflowy-macos-x64`, which only exists in older
/// releases — newer ones 404 and the error message says so.
fn asset_name(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Some("appflowy-windows-x64.exe"),
        ("linux", "x86_64") => Some("appflowy-linux-x64"),
        ("macos", "x86_64") => Some("appflowy-macos-x64"),
        ("macos", "aarch64") => Some("appflowy-macos-aarch64"),
        _ => None,
    }
}

/// Name of the staging file placed next to the target. Windows can only exec
/// `*.exe`, so the download must carry the extension for the smoke test.
fn staging_path(dir: &Path, stem: &str, pid: u32) -> PathBuf {
    let name = if cfg!(windows) {
        format!("{stem}.new-{pid}.exe")
    } else {
        format!("{stem}.new-{pid}")
    };
    dir.join(name)
}

/// Remove leftovers from earlier update attempts next to the exe: the
/// Windows `.old` backup (locked until the previous process exited) and any
/// interrupted download's staging files.
fn cleanup_stale(dir: &Path, file_name: &str, stem: &str) {
    let backup = format!("{file_name}.old");
    let staging_prefix = format!("{stem}.new-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == backup || name.starts_with(&staging_prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The sibling entry point: `appflowy` <-> `appflowy-mcp`. A binary renamed
/// to anything else has no sibling to sync.
fn sibling_path(exe: &Path) -> Option<PathBuf> {
    let stem = exe.file_stem()?.to_str()?;
    let name = match stem {
        "appflowy" => {
            if cfg!(windows) {
                "appflowy-mcp.exe"
            } else {
                "appflowy-mcp"
            }
        }
        "appflowy-mcp" => {
            if cfg!(windows) {
                "appflowy.exe"
            } else {
                "appflowy"
            }
        }
        _ => return None,
    };
    Some(exe.with_file_name(name))
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("appflowy-mcp/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()?)
}

/// Latest release tag from the GitHub API (`{api_base}` is a parameter for
/// tests; production passes `API_BASE`).
#[doc(hidden)]
pub async fn resolve_latest_tag(client: &reqwest::Client, api_base: &str) -> Result<String> {
    let url = format!("{api_base}/repos/{REPO}/releases/latest");
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(160)
            .collect();
        return Err(Error::Other(format!(
            "could not resolve the latest release ({status} from {url}): {body} \
             — pass --version X.Y.Z to pin a release"
        )));
    }
    let body: Value = resp.json().await?;
    let tag = body.get("tag_name").and_then(Value::as_str).unwrap_or("");
    if tag.is_empty() {
        return Err(Error::Other(format!(
            "the latest-release response from {url} has no tag_name"
        )));
    }
    Ok(tag.to_string())
}

/// Download one release asset (`{download_base}` is a parameter for tests;
/// production passes `DOWNLOAD_BASE`).
#[doc(hidden)]
pub async fn download_asset(
    client: &reqwest::Client,
    download_base: &str,
    tag: &str,
    asset: &str,
) -> Result<Vec<u8>> {
    let url = format!("{download_base}/{REPO}/releases/download/{tag}/{asset}");
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Other(format!(
            "download failed: {url} returned {status} — the asset may not exist \
             for this platform in {tag} (e.g. Intel macOS); fallback: build from \
             source (docs/installation.md)"
        )));
    }
    Ok(resp.bytes().await?.to_vec())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Run `staged --version` before installing (mirrors the install scripts):
/// a truncated download or a wrong-architecture binary fails here, and a
/// mismatched version means the tagged asset isn't what it claims.
fn verify_staged_binary(staged: &Path, expected: Version) -> Result<()> {
    make_executable(staged)?;
    let output = std::process::Command::new(staged)
        .arg("--version")
        .output()
        .map_err(|e| {
            Error::Other(format!(
                "downloaded binary failed to run (truncated or wrong-arch?): {e}"
            ))
        })?;
    let got = version_from_version_output(&String::from_utf8_lossy(&output.stdout));
    if !output.status.success() || got != Some(expected) {
        return Err(Error::Other(format!(
            "downloaded binary failed verification: expected {expected}, \
             stdout {:?}, stderr {:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn replace_hint(target: &Path, e: std::io::Error) -> Error {
    Error::Other(format!(
        "cannot replace {}: {e} — the install directory is not writable; fix \
         permissions or update by re-running the install script",
        target.display()
    ))
}

#[cfg(windows)]
fn backup_path(target: &Path) -> PathBuf {
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("appflowy");
    target.with_file_name(format!("{file_name}.old"))
}

/// Swap `staged` in at `target` (the running exe).
fn replace_self(staged: &Path, target: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let backup = backup_path(target);
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(target, &backup).map_err(|e| replace_hint(target, e))?;
        if let Err(e) = std::fs::rename(staged, target) {
            let _ = std::fs::rename(&backup, target); // put the old binary back
            return Err(replace_hint(target, e));
        }
        // Fails while this process still runs the old image — expected; the
        // next update's cleanup_stale sweeps it.
        let _ = std::fs::remove_file(&backup);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(staged, target).map_err(|e| replace_hint(target, e))
    }
}

/// Re-sync the second installed name with the freshly replaced target:
/// hard-link on Unix like install.sh's `ln -f`, copy on Windows. A failing
/// sibling (e.g. `appflowy-mcp.exe` held open by a running MCP client) is a
/// warning, not a fatal error — the main binary is already updated.
fn sync_sibling(target: &Path, sibling: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        std::fs::copy(target, sibling)?;
    }
    #[cfg(not(windows))]
    {
        std::fs::remove_file(sibling)?;
        if std::fs::hard_link(target, sibling).is_err() {
            std::fs::copy(target, sibling)?;
        }
    }
    Ok(())
}

/// Verify, replace, re-sync. Returns the synced sibling path (if any) and a
/// non-fatal warning. On Err the staging file still exists for the caller to
/// remove.
fn install_staged(
    staged: &Path,
    exe: &Path,
    version: Version,
) -> Result<(Option<PathBuf>, Option<String>)> {
    verify_staged_binary(staged, version)?;
    let sibling = sibling_path(exe).filter(|s| s.exists());
    replace_self(staged, exe)?;
    let warning = match &sibling {
        Some(sibling) => sync_sibling(exe, sibling).err().map(|e| {
            format!(
                "{} was updated but {} could not be replaced (in use by an MCP \
                 client?): {e} — re-run `appflowy update` once it exits",
                exe.display(),
                sibling.display()
            )
        }),
        None => None,
    };
    Ok((sibling, warning))
}

pub async fn cmd_update(args: &UpdateArgs, json_out: bool) -> Result<()> {
    let current = parse_version(CURRENT_VERSION)
        .ok_or_else(|| Error::Other(format!("cannot parse own version {CURRENT_VERSION:?}")))?;
    let pin = args
        .version
        .clone()
        .or_else(|| std::env::var("APPFLOWY_VERSION").ok())
        .filter(|s| !s.trim().is_empty());

    let client = http_client()?;
    let (tag, target) = match pin.as_deref() {
        Some(pin) => {
            let version = parse_version(pin)
                .ok_or_else(|| Error::Validation(format!("version '{pin}' (want X.Y.Z)")))?;
            (format!("v{version}"), version)
        }
        None => {
            let tag = resolve_latest_tag(&client, API_BASE).await?;
            let version = parse_version(&tag).ok_or_else(|| {
                Error::Other(format!(
                    "latest release tag '{tag}' is not vX.Y.Z — pass --version to pin a release"
                ))
            })?;
            (tag, version)
        }
    };

    if args.check {
        let update_available = target != current;
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "installed": current.to_string(),
                    "target": target.to_string(),
                    "update_available": update_available,
                }))?
            );
        } else if update_available {
            println!("update available: {current} -> {target} (run `appflowy update`)");
        } else {
            println!("appflowy {current} is up to date");
        }
        return Ok(());
    }

    // An explicit pin always proceeds (it doubles as a force-reinstall);
    // unpinned runs stop when the latest release is not newer.
    if pin.is_none() && target <= current {
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "updated": false,
                    "version": current.to_string(),
                    "latest": target.to_string(),
                }))?
            );
        } else {
            println!("appflowy {current} is up to date (latest: {target})");
        }
        return Ok(());
    }

    let asset = asset_name(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
        Error::Other(format!(
            "no prebuilt asset for {}/{} — build from source (docs/installation.md)",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    })?;

    let exe = std::env::current_exe()?;
    // Resolve symlinks so we replace the real binary, not the link. Skipped
    // on Windows: canonicalize yields hard-to-read \\?\ verbatim paths and
    // current_exe is already fully resolved there.
    #[cfg(not(windows))]
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe
        .parent()
        .ok_or_else(|| {
            Error::Other(format!(
                "current exe {} has no parent directory",
                exe.display()
            ))
        })?
        .to_path_buf();
    let file_name = exe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let stem = exe
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();

    cleanup_stale(&dir, &file_name, &stem);

    let bytes = download_asset(&client, DOWNLOAD_BASE, &tag, asset).await?;

    let staged = staging_path(&dir, &stem, std::process::id());
    std::fs::write(&staged, &bytes).map_err(|e| {
        Error::Other(format!(
            "cannot write {}: {e} — the install directory is not writable; fix \
             permissions or update by re-running the install script",
            staged.display()
        ))
    })?;

    let installed = install_staged(&staged, &exe, target);
    if installed.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    let (sibling, warning) = installed?;
    if let Some(warning) = &warning {
        eprintln!("warning: {warning}");
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "updated": true,
                "previous": current.to_string(),
                "version": target.to_string(),
                "tag": tag,
                "asset": asset,
                "path": exe.display().to_string(),
                "sibling": sibling.map(|s| s.display().to_string()),
            }))?
        );
    } else {
        println!("updated {current} -> {target}:");
        println!("  {}", exe.display());
        if let Some(sibling) = &sibling {
            println!("  {}", sibling.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("0.8.1"), Some(Version(0, 8, 1)));
        assert_eq!(parse_version("v1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(parse_version(" 0.9.0 "), Some(Version(0, 9, 0)));
        assert_eq!(parse_version("10.20.30"), Some(Version(10, 20, 30)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("x.y.z"), None);
        assert_eq!(parse_version("v"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn orders_versions() {
        assert!(Version(0, 9, 0) > Version(0, 8, 1));
        assert!(Version(1, 0, 0) > Version(0, 99, 99));
        assert!(parse_version(env!("CARGO_PKG_VERSION")).is_some());
    }

    #[test]
    fn reads_version_from_version_output() {
        assert_eq!(
            version_from_version_output("appflowy-cli 0.9.0"),
            Some(Version(0, 9, 0))
        );
        assert_eq!(
            version_from_version_output("appflowy-cli 0.9.0\n"),
            Some(Version(0, 9, 0))
        );
        assert_eq!(version_from_version_output("not a version"), None);
    }

    #[test]
    fn maps_platforms_to_assets() {
        assert_eq!(
            asset_name("windows", "x86_64"),
            Some("appflowy-windows-x64.exe")
        );
        assert_eq!(asset_name("linux", "x86_64"), Some("appflowy-linux-x64"));
        assert_eq!(
            asset_name("macos", "aarch64"),
            Some("appflowy-macos-aarch64")
        );
        assert_eq!(asset_name("macos", "x86_64"), Some("appflowy-macos-x64"));
        assert_eq!(asset_name("linux", "aarch64"), None);
        assert_eq!(asset_name("windows", "aarch64"), None);
    }

    #[test]
    fn builds_staging_paths() {
        let path = staging_path(Path::new("/d"), "appflowy", 42);
        let name = path.file_name().unwrap().to_str().unwrap();
        if cfg!(windows) {
            assert_eq!(name, "appflowy.new-42.exe");
        } else {
            assert_eq!(name, "appflowy.new-42");
        }
    }

    #[test]
    fn finds_sibling_names() {
        let mcp = if cfg!(windows) {
            "appflowy-mcp.exe"
        } else {
            "appflowy-mcp"
        };
        let cli = if cfg!(windows) {
            "appflowy.exe"
        } else {
            "appflowy"
        };
        assert_eq!(
            sibling_path(Path::new("/bin").join(cli).as_path()),
            Some(Path::new("/bin").join(mcp))
        );
        assert_eq!(
            sibling_path(Path::new("/bin").join(mcp).as_path()),
            Some(Path::new("/bin").join(cli))
        );
        assert_eq!(sibling_path(Path::new("/bin/renamed-tool")), None);
    }
}

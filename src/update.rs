//! `appflowy update`: self-update the binary from GitHub Releases.
//!
//! Mirrors install.sh / install.ps1: resolve a release tag (latest, or
//! pinned via --version / APPFLOWY_VERSION), download the platform asset
//! (verifying its sha256 digest when the release records one), run
//! `--version` on the download as a smoke test, then swap it in at the
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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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
    /// from APPFLOWY_VERSION, matching the install scripts). Must be a
    /// stable `X.Y.Z`: prerelease/build suffixes are rejected rather than
    /// silently mapped to their stable core.
    #[arg(long, value_name = "X.Y.Z")]
    pub version: Option<String>,
    /// Only check whether an update exists; download and replace nothing.
    #[arg(long)]
    pub check: bool,
}

/// Strict X.Y.Z comparison without pulling in a semver crate (stable release
/// tags are always `vX.Y.Z`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Parse a stable `0.9.0` / `v0.9.0`, rejecting anything without all three
/// numeric components as well as prerelease / build-metadata suffixes.
///
/// Suffixes used to be truncated (`0.9.0-rc.1` became `0.9.0`), which silently
/// queried and installed the stable tag the user never asked for. Until the
/// updater compares full semver, a suffix is a validation error instead.
fn parse_version(raw: &str) -> Option<Version> {
    let raw = raw.trim().trim_start_matches(['v', 'V']);
    if raw.contains(['-', '+']) {
        return None;
    }
    let mut parts = raw.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version(major, minor, patch))
}

/// Resolve a pinned `--version` / `APPFLOWY_VERSION` to its release tag.
/// Prerelease and build suffixes are rejected here rather than silently
/// truncated to their stable core, so a request for `0.9.0-rc.1` can never
/// turn into a query or install of `v0.9.0`.
fn resolve_pinned_release(pin: &str) -> Result<(String, Version)> {
    let version = parse_version(pin).ok_or_else(|| {
        Error::Validation(format!(
            "version '{pin}' (want a stable X.Y.Z; prerelease/build suffixes \
             such as '-rc.1' or '+build' are not supported)"
        ))
    })?;
    Ok((format!("v{version}"), version))
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

/// Upper bound on how long a staging file may exist before it is considered a
/// crash leftover. Verification runs the downloaded binary once, so a live
/// staging file exists for seconds; an hour is far beyond any plausible
/// overlap with a concurrent update, while still letting the next run sweep
/// a file abandoned by a dropped download or an interrupted install.
const STAGING_STALE_AFTER: Duration = Duration::from_secs(60 * 60);

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
///
/// Staging files are named `{stem}.new-{pid}`, so the prefix alone says
/// nothing about whether the owning process is still running. A concurrent
/// update's file must not be swept: only files whose mtime proves they are
/// older than [`STAGING_STALE_AFTER`] are removed. `.old` needs no such
/// guard — on Windows it is locked while the owning process still runs, and
/// on Unix this updater never creates one.
fn cleanup_stale(dir: &Path, file_name: &str, stem: &str) {
    cleanup_stale_at(dir, file_name, stem, SystemTime::now());
}

/// [`cleanup_stale`] with an injectable clock, so the age threshold can be
/// tested without mutating file timestamps.
fn cleanup_stale_at(dir: &Path, file_name: &str, stem: &str, now: SystemTime) {
    let backup = format!("{file_name}.old");
    let staging_prefix = format!("{stem}.new-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == backup {
            let _ = std::fs::remove_file(entry.path());
        } else if name.starts_with(&staging_prefix) && staging_is_stale(&entry, now) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// True only when a staging file's age is known and exceeds the threshold.
/// `metadata()` / `modified()` failures (and future timestamps from clock
/// skew) mean "cannot prove it is stale", so the file is left for the owning
/// process or a later cleanup.
fn staging_is_stale(entry: &std::fs::DirEntry, now: SystemTime) -> bool {
    let Ok(metadata) = entry.metadata() else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    now.duration_since(modified)
        .is_ok_and(|age| age >= STAGING_STALE_AFTER)
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

/// One release as the GitHub API describes it, reduced to what the updater
/// needs: the tag plus each asset's `digest` field (`sha256:<hex>`). GitHub
/// only began populating digests in 2025, so older releases carry none —
/// downloads from them skip checksum verification.
#[doc(hidden)]
#[derive(Debug)]
pub struct ReleaseInfo {
    pub tag: String,
    /// asset file name -> raw digest; assets without one are omitted.
    digests: HashMap<String, String>,
}

impl ReleaseInfo {
    /// The hex sha256 the release recorded for `asset`, or None when it has
    /// no digest or uses an algorithm this updater can't check (only
    /// sha256 — the only one GitHub publishes today — is verified).
    pub fn sha256_digest(&self, asset: &str) -> Option<&str> {
        self.digests
            .get(asset)
            .and_then(|digest| digest.strip_prefix("sha256:"))
    }
}

/// Reduce a release JSON payload to its tag and asset digests; None means
/// no usable tag_name (callers phrase their own error).
fn parse_release(body: &Value) -> Option<ReleaseInfo> {
    let tag = body.get("tag_name")?.as_str()?.to_string();
    let digests = body
        .get("assets")
        .and_then(Value::as_array)
        .map(|assets| {
            assets
                .iter()
                .filter_map(|asset| {
                    let name = asset.get("name")?.as_str()?;
                    let digest = asset.get("digest")?.as_str()?;
                    Some((name.to_string(), digest.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ReleaseInfo { tag, digests })
}

/// GET one GitHub API URL and parse the JSON. The optional bearer token is
/// attached here and only here: GitHub's anonymous API budget is 60
/// requests/hour per IP (a token raises it to 5000), while the download
/// host neither needs nor should see credentials. The token is a parameter,
/// not read from the env, so tests stay hermetic. `what`/`hint` shape the
/// error; the body is truncated to its leading text because GitHub error
/// bodies are JSON whose message is the useful part.
async fn get_release_json(
    client: &reqwest::Client,
    url: &str,
    github_token: Option<&str>,
    what: &str,
    hint: &str,
) -> Result<Value> {
    let mut request = client.get(url);
    if let Some(token) = github_token {
        request = request.bearer_auth(token);
    }
    let resp = request.send().await?;
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
            "could not {what} ({status} from {url}): {body} — {hint}"
        )));
    }
    Ok(resp.json().await?)
}

/// The latest release — its tag plus the assets' digests, which ride along
/// so the later download needs no second API call (`{api_base}` is a
/// parameter for tests; production passes `API_BASE`).
#[doc(hidden)]
pub async fn resolve_latest_tag(
    client: &reqwest::Client,
    api_base: &str,
    github_token: Option<&str>,
) -> Result<ReleaseInfo> {
    let url = format!("{api_base}/repos/{REPO}/releases/latest");
    let body = get_release_json(
        client,
        &url,
        github_token,
        "resolve the latest release",
        "pass --version X.Y.Z to pin a release",
    )
    .await?;
    parse_release(&body).ok_or_else(|| {
        Error::Other(format!(
            "the latest-release response from {url} has no tag_name"
        ))
    })
}

/// A pinned release from the GitHub API — the counterpart of
/// [`resolve_latest_tag`] for `--version` / APPFLOWY_VERSION runs, which
/// resolve their tag locally and only fetch the metadata (for the asset
/// digests) once a download is actually happening.
#[doc(hidden)]
pub async fn fetch_release_by_tag(
    client: &reqwest::Client,
    api_base: &str,
    github_token: Option<&str>,
    tag: &str,
) -> Result<ReleaseInfo> {
    let url = format!("{api_base}/repos/{REPO}/releases/tags/{tag}");
    let body = get_release_json(
        client,
        &url,
        github_token,
        &format!("fetch release {tag}"),
        &format!("check that {tag} is a published release"),
    )
    .await?;
    parse_release(&body)
        .ok_or_else(|| Error::Other(format!("the release response from {url} has no tag_name")))
}

/// Lowercase hex sha256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in &digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Download one release asset, verifying it against the sha256 the release
/// recorded when it has one (`{download_base}` is a parameter for tests;
/// production passes `DOWNLOAD_BASE`). Verification happens before the
/// `--version` smoke test executes the bytes — a substituted binary could
/// simply print the expected version, but not fake the digest. `expected`
/// is None for releases that predate digests, leaving the smoke test as
/// the guard.
#[doc(hidden)]
pub async fn download_asset(
    client: &reqwest::Client,
    download_base: &str,
    tag: &str,
    asset: &str,
    expected_sha256: Option<&str>,
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
    let bytes = resp.bytes().await?.to_vec();
    if let Some(expected) = expected_sha256 {
        let actual = sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(Error::Other(format!(
                "checksum mismatch for {asset} from {tag}: release says \
                 sha256 {expected}, download hashes to {actual} — the bytes \
                 were corrupted or tampered with; retry, and report it if it \
                 persists"
            )));
        }
    }
    Ok(bytes)
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

    // GitHub's anonymous API budget is 60 requests/hour per IP — quick to
    // exhaust behind a shared NAT or in CI — so a GITHUB_TOKEN / GH_TOKEN
    // in the environment is attached to the API requests (never the
    // download host, which shouldn't see credentials). Read here, not
    // inside the request helpers, so they stay hermetic for tests.
    let github_token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|token| !token.trim().is_empty());

    let client = http_client()?;
    // `release` is None for pinned runs: the tag is resolved locally so
    // `--check` and the up-to-date short-circuit stay network-free, and the
    // metadata (asset digests) is fetched only when a download is happening.
    let (tag, target, release) = match pin.as_deref() {
        Some(pin) => {
            let (tag, version) = resolve_pinned_release(pin)?;
            (tag, version, None)
        }
        None => {
            let release = resolve_latest_tag(&client, API_BASE, github_token.as_deref()).await?;
            let tag = release.tag.clone();
            let version = parse_version(&tag).ok_or_else(|| {
                Error::Other(format!(
                    "latest release tag '{tag}' is not a stable vX.Y.Z — \
                     prerelease/build tags are not supported by this updater"
                ))
            })?;
            (tag, version, Some(release))
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

    // Pinned runs fetch the release metadata only now — `--check` and the
    // up-to-date short-circuit never reach this point. The latest path
    // already holds it from tag resolution; fetching again would spend
    // rate-limit budget for nothing.
    let release = match release {
        Some(release) => release,
        None => fetch_release_by_tag(&client, API_BASE, github_token.as_deref(), &tag).await?,
    };

    let bytes = download_asset(
        &client,
        DOWNLOAD_BASE,
        &tag,
        asset,
        release.sha256_digest(asset),
    )
    .await?;

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
    fn rejects_prerelease_and_build_metadata_instead_of_truncating() {
        // These used to parse as their stable core (`0.9.0`), silently
        // querying/installing a different tag than the one requested.
        assert_eq!(parse_version("0.9.0-rc.1"), None);
        assert_eq!(parse_version("v0.9.0-rc.1"), None);
        assert_eq!(parse_version("0.9.0+build.7"), None);
        assert_eq!(parse_version("1.2.3-"), None);
        // The smoke test must not accept an rc binary output as stable
        // either (that would also collapse the two versions).
        assert_eq!(version_from_version_output("appflowy-cli 0.9.0-rc.1"), None);
    }

    #[test]
    fn pinned_prereleases_are_rejected_not_retargeted_to_stable() {
        assert_eq!(
            resolve_pinned_release("v0.9.0").unwrap(),
            ("v0.9.0".to_string(), Version(0, 9, 0))
        );
        for pin in ["0.9.0-rc.1", "v0.9.0-rc.1", "0.9.0+build.7"] {
            match resolve_pinned_release(pin) {
                Err(Error::Validation(message)) => {
                    assert!(
                        message.contains(pin),
                        "message should quote {pin}: {message}"
                    );
                }
                other => panic!("expected a validation error for {pin}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn cmd_update_rejects_a_prerelease_pin_before_any_release_lookup() {
        let args = UpdateArgs {
            version: Some("0.9.0-rc.1".to_string()),
            check: true,
        };
        match cmd_update(&args, true).await {
            Err(Error::Validation(message)) => {
                assert!(
                    message.contains("0.9.0-rc.1"),
                    "message should quote the pin: {message}"
                );
            }
            // A regression that truncates the suffix would treat this as the
            // stable 0.9.0 pin and return Ok from --check instead.
            other => panic!("expected a validation error, got {other:?}"),
        }
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

    fn cleanup_dir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "appflowy-update-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn staging_name(pid: u32) -> String {
        if cfg!(windows) {
            format!("appflowy.new-{pid}.exe")
        } else {
            format!("appflowy.new-{pid}")
        }
    }

    #[test]
    fn keeps_staging_files_that_may_belong_to_a_concurrent_update() {
        let dir = cleanup_dir("live-staging");
        // A different pid than this test process: the old prefix-only sweep
        // deleted exactly this file while its updater was verifying it.
        let live = dir.join(staging_name(4242));
        std::fs::write(&live, b"download").unwrap();
        let unrelated = dir.join("notes.txt");
        std::fs::write(&unrelated, b"keep").unwrap();

        cleanup_stale_at(&dir, "appflowy", "appflowy", SystemTime::now());

        assert!(
            live.exists(),
            "a concurrent update's staging file must not be swept"
        );
        assert!(unrelated.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removes_provably_stale_staging_and_backup_files() {
        let dir = cleanup_dir("stale-staging");
        let stale = dir.join(staging_name(5151));
        std::fs::write(&stale, b"abandoned download").unwrap();
        let backup = dir.join("appflowy.old");
        std::fs::write(&backup, b"old binary").unwrap();
        // Another app's staging file must survive even an old mtime.
        let other = dir.join(staging_name(6161).replace("appflowy", "otherapp"));
        std::fs::write(&other, b"other").unwrap();

        // Inject a clock past the threshold instead of rewriting mtimes, so
        // the test exercises the same age comparison production uses.
        let later = SystemTime::now() + STAGING_STALE_AFTER + Duration::from_secs(60);
        cleanup_stale_at(&dir, "appflowy", "appflowy", later);

        assert!(!stale.exists(), "a provably stale staging file is swept");
        assert!(!backup.exists(), "the Windows .old backup is swept");
        assert!(other.exists(), "only this app's staging files are swept");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sweeps_a_staging_file_whose_real_mtime_is_old() {
        use std::fs::FileTimes;

        let dir = cleanup_dir("old-mtime");
        let stale = dir.join(staging_name(7171));
        std::fs::write(&stale, b"abandoned download").unwrap();
        let old = SystemTime::now() - STAGING_STALE_AFTER - Duration::from_secs(60);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .unwrap();
        file.set_times(FileTimes::new().set_modified(old))
            .unwrap();
        drop(file);

        // Production clock, real metadata: verifies the sweep reads mtime.
        cleanup_stale(&dir, "appflowy", "appflowy");

        assert!(!stale.exists());
        std::fs::remove_dir_all(&dir).ok();
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

//! Command-line interface for AppFlowy.
//!
//! A thin layer over the same client/converters the MCP server uses, aimed
//! at scriptable workflows (backups via cron, bulk import, quick lookups).
//!
//! Authentication, in priority order:
//! 1. APPFLOWY_EMAIL / APPFLOWY_PASSWORD environment variables (or .env)
//! 2. tokens saved by `appflowy-cli login` in ~/.config/appflowy-cli/
//!
//! Addressing follows AppFlowy's hierarchy. The workspace comes from
//! `appflowy-cli use <workspace>` (saved as the default) or the global
//! `-w/--workspace` flag; everything below it is a PATH of space/page
//! segments — names or UUIDs — resolved level by level, e.g. `demo/notes`.
//!
//! Port of Python `appflowy_mcp/cli.py`. Known intentional downgrades:
//! - `py_repr` approximates Python's `repr()` for the common cases instead
//!   of being a full port.

use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use reqwest::Method;
use rmcp::transport::io::stdio;
use rmcp::ServiceExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tracing_subscriber::EnvFilter;

use crate::client::AppFlowyClient;
use crate::error::{Error, Result};
use crate::importer::MarkdownImporter;
use crate::markdown::parse_content_to_blocks;
use crate::server::AppServer;
use crate::tools::export::{
    export_views_to_directory, fetch_page_markdown, ExportManifest, DEFAULT_PAGE_BACKOFF_SECS,
    DEFAULT_PAGE_RETRIES, MANIFEST_FILE_NAME,
};
use crate::tools::util::{create_page_with_blocks, expand_tilde, unique_path};
use crate::update::UpdateArgs;

// ==================== argument parsing ====================

#[derive(Parser)]
#[command(
    name = "appflowy-cli",
    version,
    about = "AppFlowy command-line client. Select a workspace once with \
             `use`, then address spaces and pages by PATH, e.g. `demo/notes`."
)]
struct Cli {
    /// Print raw JSON instead of summaries.
    #[arg(long, global = true)]
    json: bool,
    /// workspace id or name (overrides the `use` default)
    #[arg(short = 'w', long, global = true)]
    workspace: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args, Default)]
struct ServeArgs {
    /// Serve Streamable HTTP at ADDR (e.g. 127.0.0.1:8000) instead of stdio.
    /// The MCP endpoint is http://ADDR/mcp.
    #[arg(long, value_name = "ADDR")]
    http: Option<String>,
    /// Disable loopback restrictions: also accept non-loopback Host headers.
    /// Required to bind --http to a non-loopback address. Security note: any
    /// client that can reach the endpoint can drive every MCP tool with the
    /// server's AppFlowy credentials.
    #[arg(long)]
    allow_remote: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server over stdio (same as the `appflowy-mcp` entry
    /// point); pass --http to serve Streamable HTTP instead.
    Serve(ServeArgs),
    /// Log in and save session tokens for later commands (the password itself
    /// is never stored).
    Login(LoginArgs),
    /// Delete saved session tokens.
    Logout,
    /// Set (or show) the default workspace.
    Use {
        /// workspace id or name
        target: Option<String>,
    },
    /// List workspaces (* = default).
    Workspaces,
    /// List children of a space/page PATH.
    Ls {
        /// e.g. demo or demo/notes (default: root)
        path: Option<String>,
    },
    /// Print the view tree under PATH.
    Tree {
        /// space/page path (default: root)
        path: Option<String>,
    },
    /// Full-text search the workspace.
    Search {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: i64,
    },
    /// Export PATH as Markdown: a leaf page becomes one .md file, a
    /// space/subtree becomes a directory, no PATH exports the whole
    /// workspace.
    Export {
        /// space/page path (default: whole workspace)
        path: Option<String>,
        /// destination file or directory
        #[arg(short = 'o', long, required = true)]
        output: String,
    },
    /// Import local Markdown under PATH: a local file becomes one page, a
    /// local directory becomes a page subtree.
    Import(ImportArgs),
    /// Create a page under PATH from Markdown (a file, --content, or stdin).
    Save(SaveArgs),
    /// Update this binary in place from GitHub Releases (the latest release,
    /// or --version / APPFLOWY_VERSION to pin). Also re-syncs the
    /// `appflowy-mcp` sibling name. Needs no AppFlowy login.
    Update(UpdateArgs),
}

#[derive(Args)]
struct LoginArgs {
    /// prompted for interactively if omitted
    #[arg(long)]
    email: Option<String>,
    /// prompted for interactively if omitted
    #[arg(long)]
    password: Option<String>,
    /// self-hosted AppFlowy Cloud URL
    #[arg(long)]
    base_url: Option<String>,
}

#[derive(Args)]
struct ImportArgs {
    /// destination space/page path
    path: String,
    /// local .md file or directory
    local: String,
    /// page title for single-file import
    #[arg(long)]
    title: Option<String>,
    /// keep local image paths instead of uploading them
    #[arg(long)]
    no_assets: bool,
}

#[derive(Args)]
struct SaveArgs {
    /// parent space/page path
    path: String,
    title: String,
    /// Markdown file to use as content
    #[arg(long)]
    file: Option<String>,
    /// inline Markdown content
    #[arg(long)]
    content: Option<String>,
}

// ==================== config & credentials ====================

#[derive(Debug, Default, Serialize, Deserialize)]
struct Credentials {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

fn config_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| "~/.config".to_string());
    expand_tilde(&base).join("appflowy-cli")
}

fn credentials_path() -> PathBuf {
    config_dir().join("credentials.json")
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// Persist the session tokens (never the password) with mode 600 on unix.
async fn save_credentials(client: &AppFlowyClient) -> Result<()> {
    let path = credentials_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stored = Credentials {
        email: client.email().await,
        base_url: Some(client.base_url().await),
        access_token: client.access_token().await,
        refresh_token: client.refresh_token_value().await,
    };
    std::fs::write(&path, serde_json::to_string_pretty(&stored)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Restore a saved session into the client. Returns true if loaded.
async fn load_credentials(client: &AppFlowyClient) -> bool {
    let path = credentials_path();
    if !path.is_file() {
        return false;
    }
    let stored: Credentials = match std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
    {
        Some(stored) => stored,
        None => return false,
    };
    let Some(refresh_token) = stored.refresh_token.filter(|t| !t.is_empty()) else {
        return false;
    };
    if client.email().await.is_none() {
        client.set_email(stored.email).await;
    }
    if let Some(base_url) = stored.base_url.filter(|u| !u.is_empty()) {
        if std::env::var_os("APPFLOWY_BASE_URL").is_none() {
            client.set_base_url(base_url).await;
        }
    }
    client.set_access_token(stored.access_token).await;
    client.set_refresh_token(Some(refresh_token)).await;
    true
}

fn load_config() -> Map<String, Value> {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn save_config(config: &Map<String, Value>) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(config)?)?;
    Ok(())
}

// ==================== addressing ====================

/// Equivalent of the Python `UUID_RE.match` anchored regex.
fn is_uuid(token: &str) -> bool {
    let bytes = token.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(i, &b)| match i {
        8 | 13 | 18 | 23 => b == b'-',
        _ => b.is_ascii_hexdigit(),
    })
}

/// Accept a workspace id or name; returns the workspace object (a bare
/// `{workspace_id, workspace_name}` dict for an unmatched UUID, like Python).
async fn resolve_workspace(client: &AppFlowyClient, token: &str) -> Result<Value> {
    let body = client
        .request(Method::GET, "/api/workspace", None, None)
        .await?;
    let workspaces = body.as_array().cloned().unwrap_or_default();
    if is_uuid(token) {
        for ws in &workspaces {
            if ws.get("workspace_id").and_then(Value::as_str) == Some(token) {
                return Ok(ws.clone());
            }
        }
        return Ok(json!({"workspace_id": token, "workspace_name": token}));
    }
    let folded = token.to_lowercase();
    let matches: Vec<&Value> = workspaces
        .iter()
        .filter(|ws| {
            ws.get("workspace_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase()
                == folded
        })
        .collect();
    if matches.len() == 1 {
        return Ok(matches[0].clone());
    }
    let mut names: Vec<&str> = workspaces
        .iter()
        .map(|ws| {
            ws.get("workspace_name")
                .and_then(Value::as_str)
                .unwrap_or("?")
        })
        .collect();
    names.sort();
    let kind = if matches.is_empty() {
        "Unknown"
    } else {
        "Ambiguous"
    };
    Err(Error::Other(format!(
        "{kind} workspace '{token}'. Available: {}",
        names.join(", ")
    )))
}

/// Workspace id from -w/--workspace, falling back to the `use` default.
async fn current_workspace(client: &AppFlowyClient, workspace_arg: Option<&str>) -> Result<String> {
    let token = match workspace_arg {
        Some(token) => token.to_string(),
        None => {
            let config = load_config();
            match config
                .get("workspace")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                Some(token) => token.to_string(),
                None => {
                    return Err(Error::Other(
                        "No workspace selected. Run `appflowy-cli use <workspace>` \
                         or pass -w <workspace>."
                            .into(),
                    ));
                }
            }
        }
    };
    let workspace = resolve_workspace(client, &token).await?;
    Ok(workspace
        .get("workspace_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string())
}

/// Walk PATH segment by segment from the workspace root; returns the
/// resolved view node (the workspace root view when PATH is empty).
async fn resolve_path(
    client: &AppFlowyClient,
    workspace_id: &str,
    path: Option<&str>,
) -> Result<Value> {
    let params = [("depth", "10".to_string())];
    let mut node = client
        .request(
            Method::GET,
            &format!("/api/workspace/{workspace_id}/folder"),
            Some(&params),
            None,
        )
        .await?;
    for segment in path.unwrap_or("").split('/').filter(|s| !s.is_empty()) {
        let children = node
            .get("children")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut matches: Vec<&Value> = children
            .iter()
            .filter(|c| c.get("view_id").and_then(Value::as_str) == Some(segment))
            .collect();
        if matches.is_empty() {
            let folded = segment.to_lowercase();
            matches = children
                .iter()
                .filter(|c| {
                    c.get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_lowercase()
                        == folded
                })
                .collect();
        }
        if matches.len() == 1 {
            node = matches[0].clone();
            continue;
        }
        let where_ = node
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("workspace root");
        if !matches.is_empty() {
            let listing = matches
                .iter()
                .map(|c| {
                    format!(
                        "{} ({})",
                        str_field_or(c, "name", "?"),
                        str_field_or(c, "view_id", "?")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::Other(format!(
                "Ambiguous segment '{segment}' under {where_} — use a view_id: {listing}"
            )));
        }
        let names = children
            .iter()
            .map(|c| str_field_or(c, "name", "?"))
            .collect::<Vec<_>>()
            .join(", ");
        let available = if names.is_empty() { "(none)" } else { &names };
        return Err(Error::Other(format!(
            "No '{segment}' under {where_}. Children: {available}"
        )));
    }
    Ok(node)
}

// ==================== small output helpers ====================

fn str_field_or<'a>(value: &'a Value, key: &str, default: &'a str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or(default)
}

/// Non-empty export destinations are rejected unless the directory already
/// holds a usable export manifest for this workspace — that marks it as a
/// previous incremental-export destination, so re-exporting (incrementally)
/// there is allowed, mirroring the MCP tools' rule.
fn manifest_usable(directory: &std::path::Path, workspace_id: &str) -> bool {
    std::fs::read_to_string(directory.join(MANIFEST_FILE_NAME))
        .ok()
        .and_then(|text| serde_json::from_str::<ExportManifest>(&text).ok())
        .is_some_and(|manifest| manifest.is_usable_for(workspace_id))
}

/// Python truthiness for a JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn print_pretty<T: Serialize>(data: &T) {
    println!(
        "{}",
        serde_json::to_string_pretty(data).expect("JSON serialization of CLI output failed")
    );
}

fn view_marker(view: &Value) -> String {
    if view
        .get("is_space")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return "space".to_string();
    }
    match view.get("layout") {
        None => "page".to_string(),
        Some(value) => match value.as_i64() {
            Some(0) => "page".to_string(),
            Some(1) => "grid".to_string(),
            Some(2) => "board".to_string(),
            Some(3) => "calendar".to_string(),
            Some(4) => "chat".to_string(),
            Some(n) => format!("layout{n}"),
            // Python falls back to `str(layout)`.
            None => format!("layout{value}"),
        },
    }
}

/// Approximation of Python `repr(str)`: single quotes by default, double
/// quotes when the string contains a single quote but no double quote;
/// escapes the active quote, backslashes, and the common control characters.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Read one line from stdin after writing `prompt` (stdout for the email,
/// stderr for the password, matching Python `input`/`getpass` stream
/// choices). For the interactive password prompt this is only the fallback
/// used when stdin is not a TTY — the TTY path goes through
/// `rpassword::prompt_password`, which disables terminal echo.
fn prompt_line(prompt: &str, stderr: bool) -> Result<String> {
    if stderr {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
    } else {
        print!("{prompt}");
        std::io::stdout().flush()?;
    }
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line)?;
    if read == 0 {
        return Err(Error::Other("EOF when reading a line".into()));
    }
    Ok(line
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string())
}

// ==================== commands ====================

async fn cmd_login(client: &AppFlowyClient, args: &LoginArgs) -> Result<()> {
    let email = match args.email.clone().filter(|e| !e.is_empty()) {
        Some(email) => email,
        None => prompt_line("Email: ", false)?,
    };
    let password = match args.password.clone().filter(|p| !p.is_empty()) {
        Some(password) => password,
        // Two-track prompt: rpassword disables terminal echo but reads
        // directly from the TTY, which would block or fail when the password
        // is piped through stdin (scripts), so piped stdin keeps the plain
        // read_line fallback.
        None if std::io::stdin().is_terminal() => {
            rpassword::prompt_password_stderr("Password: ")
                .map_err(|err| Error::Other(format!("reading password: {err}")))?
        }
        None => prompt_line("Password: ", true)?,
    };
    if let Some(base_url) = args.base_url.as_deref().filter(|u| !u.is_empty()) {
        client.set_base_url(base_url).await;
    }
    client.set_email(Some(email.clone())).await;
    client.set_password(Some(password)).await;
    client.login().await?;
    save_credentials(client).await?;
    println!(
        "Logged in as {email}. Tokens saved to {}",
        credentials_path().display()
    );
    Ok(())
}

fn cmd_logout() -> Result<()> {
    let path = credentials_path();
    if path.is_file() {
        std::fs::remove_file(&path)?;
        println!("Removed {}", path.display());
    } else {
        println!("No saved credentials.");
    }
    Ok(())
}

async fn cmd_use(client: &AppFlowyClient, target: Option<&str>) -> Result<()> {
    let mut config = load_config();
    let Some(target) = target.filter(|t| !t.is_empty()) else {
        let current = config
            .get("workspace")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        match current {
            Some(current) => println!("{current}"),
            None => println!("(no default workspace; run `appflowy-cli use <workspace>`)"),
        }
        return Ok(());
    };
    let workspace = resolve_workspace(client, target).await?;
    let name = workspace
        .get("workspace_name")
        .cloned()
        .unwrap_or(Value::Null);
    config.insert("workspace".to_string(), name.clone());
    save_config(&config)?;
    println!(
        "Default workspace: {} ({})",
        name.as_str().unwrap_or(""),
        str_field_or(&workspace, "workspace_id", "")
    );
    Ok(())
}

async fn cmd_workspaces(client: &AppFlowyClient, json_out: bool) -> Result<()> {
    let workspaces = client
        .request(Method::GET, "/api/workspace", None, None)
        .await?;
    if json_out {
        print_pretty(&workspaces);
        return Ok(());
    }
    let config = load_config();
    let default = config.get("workspace").and_then(Value::as_str);
    for ws in workspaces.as_array().into_iter().flatten() {
        let star =
            if default.is_some() && ws.get("workspace_name").and_then(Value::as_str) == default {
                "*"
            } else {
                " "
            };
        println!(
            "{star} {}  {}",
            str_field_or(ws, "workspace_id", "None"),
            str_field_or(ws, "workspace_name", "None")
        );
    }
    Ok(())
}

async fn cmd_ls(
    client: &AppFlowyClient,
    workspace_id: &str,
    path: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let node = resolve_path(client, workspace_id, path).await?;
    let children = node
        .get("children")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if json_out {
        print_pretty(&children);
        return Ok(());
    }
    if children.is_empty() {
        println!("(empty)");
    }
    for child in &children {
        println!(
            "{:<8} {}  {}",
            view_marker(child),
            str_field_or(child, "view_id", "None"),
            str_field_or(child, "name", "None")
        );
    }
    Ok(())
}

fn print_tree(view: &Value, indent: usize) {
    println!(
        "{}[{}] {}  ({})",
        "  ".repeat(indent),
        view_marker(view),
        str_field_or(view, "name", "None"),
        str_field_or(view, "view_id", "None")
    );
    for child in view
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        print_tree(child, indent + 1);
    }
}

async fn cmd_tree(
    client: &AppFlowyClient,
    workspace_id: &str,
    path: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let node = resolve_path(client, workspace_id, path).await?;
    if json_out {
        print_pretty(&node);
    } else {
        print_tree(&node, 0);
    }
    Ok(())
}

async fn cmd_search(
    client: &AppFlowyClient,
    workspace_id: &str,
    query: &str,
    limit: i64,
    json_out: bool,
) -> Result<()> {
    let params = [("query", query.to_string()), ("limit", limit.to_string())];
    let hits = client
        .request(
            Method::GET,
            &format!("/api/search/{workspace_id}"),
            Some(&params),
            None,
        )
        .await?;
    if json_out {
        print_pretty(&hits);
        return Ok(());
    }
    let hits = hits.as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        println!("(no results)");
    }
    for hit in &hits {
        let preview = match hit.get("preview") {
            None => py_repr(""),
            Some(Value::Null) => "None".to_string(),
            Some(Value::String(s)) => py_repr(s),
            Some(other) => other.to_string(),
        };
        println!("{}  {preview}", str_field_or(hit, "object_id", "None"));
    }
    Ok(())
}

async fn cmd_export(
    client: &AppFlowyClient,
    workspace_id: &str,
    path: Option<&str>,
    output: &str,
    json_out: bool,
) -> Result<()> {
    let node = resolve_path(client, workspace_id, path).await?;
    let is_root = path.unwrap_or("").trim_matches('/').is_empty();
    let is_tree = is_root || truthy(node.get("is_space")) || truthy(node.get("children"));

    if is_tree {
        let directory = expand_tilde(output);
        if directory.exists() {
            let mut entries = std::fs::read_dir(&directory)?;
            if entries.next().is_some() && !manifest_usable(&directory, workspace_id) {
                return Err(Error::Other(format!(
                    "Destination directory is not empty: {}",
                    directory.display()
                )));
            }
        }
        std::fs::create_dir_all(&directory)?;

        let mut exported: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let roots = if is_root {
            node.get("children")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        } else {
            vec![node.clone()]
        };
        export_views_to_directory(
            client,
            workspace_id,
            &roots,
            &directory,
            &mut exported,
            &mut warnings,
        )
        .await?;
        let result = json!({
            "path": directory.display().to_string(),
            "exported_files": exported,
            "warnings": warnings,
        });
        if json_out {
            print_pretty(&result);
        } else {
            for file in &exported {
                println!("{file}");
            }
            for warning in &warnings {
                eprintln!("warning: {warning}");
            }
            println!("{} files -> {}", exported.len(), directory.display());
        }
        return Ok(());
    }

    // Leaf page -> a single Markdown file.
    let view_id = node
        .get("view_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Other("view node has no view_id".into()))?;
    let name = node
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(view_id)
        .to_string();
    let markdown = fetch_page_markdown(
        client,
        workspace_id,
        view_id,
        Some(&name),
        DEFAULT_PAGE_RETRIES,
        DEFAULT_PAGE_BACKOFF_SECS,
    )
    .await?;
    let mut path = expand_tilde(output);
    if path.is_dir() {
        path.push(format!("{name}.md"));
    } else {
        let is_markdown = path
            .extension()
            .map(|ext| {
                let ext = ext.to_string_lossy().to_lowercase();
                ext == "md" || ext == "markdown"
            })
            .unwrap_or(false);
        if !is_markdown {
            if let Some(file_name) = path.file_name() {
                let mut file_name = file_name.to_os_string();
                file_name.push(".md");
                path.set_file_name(file_name);
            } else {
                path.push(".md");
            }
        }
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let path = unique_path(&path);
    std::fs::write(&path, &markdown)?;
    if json_out {
        print_pretty(&json!({
            "page_id": view_id,
            "name": name,
            "path": path.display().to_string(),
        }));
    } else {
        println!("{}", path.display());
    }
    Ok(())
}

async fn cmd_import(
    client: &AppFlowyClient,
    workspace_id: &str,
    args: &ImportArgs,
    json_out: bool,
) -> Result<()> {
    let node = resolve_path(client, workspace_id, Some(&args.path)).await?;
    if args.path.trim_matches('/').is_empty() {
        return Err(Error::Other(
            "Import needs a space or page as PATH; importing directly under \
             the workspace root would create spaces. Run `appflowy-cli ls` \
             and pick a space."
                .into(),
        ));
    }
    let local = expand_tilde(&args.local);
    let parent_view_id = node
        .get("view_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Other("view node has no view_id".into()))?;
    let mut importer = MarkdownImporter::new(client.clone(), workspace_id);
    let local_str = local.to_string_lossy().into_owned();
    let summary = if local.is_dir() {
        importer
            .import_directory(&local_str, parent_view_id, !args.no_assets)
            .await?
    } else {
        importer
            .import_file(
                &local_str,
                parent_view_id,
                args.title.as_deref(),
                !args.no_assets,
            )
            .await?
    };
    if json_out {
        print_pretty(&summary);
        return Ok(());
    }
    for page in &summary.created_pages {
        println!("{}  {}", page.view_id, page.title);
    }
    for warning in &summary.warnings {
        eprintln!("warning: {warning}");
    }
    println!("{} pages imported", summary.created_pages.len());
    Ok(())
}

async fn cmd_save(
    client: &AppFlowyClient,
    workspace_id: &str,
    args: &SaveArgs,
    json_out: bool,
) -> Result<()> {
    let node = resolve_path(client, workspace_id, Some(&args.path)).await?;
    if args.path.trim_matches('/').is_empty() {
        return Err(Error::Other(
            "Save needs a space or page as PATH; a page created directly \
             under the workspace root would become a space."
                .into(),
        ));
    }
    let content = if let Some(file) = args.file.as_deref().filter(|f| !f.is_empty()) {
        std::fs::read_to_string(expand_tilde(file))?
    } else if let Some(content) = &args.content {
        content.clone()
    } else {
        std::io::read_to_string(std::io::stdin())?
    };
    let blocks = parse_content_to_blocks(&content, "markdown").map_err(Error::Other)?;
    let parent_view_id = node
        .get("view_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Other("view node has no view_id".into()))?;
    let page = create_page_with_blocks(
        client,
        workspace_id,
        parent_view_id,
        &args.title,
        blocks,
        0,
        None,
        None,
    )
    .await?;
    if json_out {
        print_pretty(&page);
        return Ok(());
    }
    match page.get("view_id") {
        Some(Value::String(view_id)) => println!("{view_id}"),
        Some(other) => println!("{other}"),
        None => println!("{page}"),
    }
    Ok(())
}

// ==================== entry point ====================

/// Port of Python `ensure_authenticated`: reuse a live access token, log in
/// with the configured email/password otherwise.
async fn ensure_authenticated(client: &AppFlowyClient) -> Result<()> {
    if client.access_token().await.is_some() {
        return Ok(());
    }
    if client.email().await.is_some() && client.password().await.is_some() {
        return client
            .login()
            .await
            .map(|_| ())
            .map_err(|e| Error::Other(format!("Auto-login failed: {e}")));
    }
    Err(Error::Other(
        "Not authenticated. Please login first.".into(),
    ))
}

async fn run_inner(cli: Cli) -> Result<()> {
    if let Command::Serve(args) = &cli.command {
        return serve_with(args)
            .await
            .map_err(|e| Error::Other(format!("{e:#}")));
    }

    // Self-update talks to GitHub, not AppFlowy — skip the auth chain.
    if let Command::Update(args) = &cli.command {
        return crate::update::cmd_update(args, cli.json).await;
    }

    dotenvy::dotenv().ok();
    let client = AppServer::from_env().client;

    // login/logout skip the authentication chain entirely.
    match &cli.command {
        Command::Login(args) => return cmd_login(&client, args).await,
        Command::Logout => return cmd_logout(),
        _ => {}
    }

    let mut stored = false;
    if client.email().await.is_some() && client.password().await.is_some() {
        ensure_authenticated(&client).await?;
    } else if load_credentials(&client).await {
        stored = true;
    } else {
        return Err(Error::Other(
            "Not authenticated. Run `appflowy-cli login`, or set \
             APPFLOWY_EMAIL and APPFLOWY_PASSWORD."
                .into(),
        ));
    }

    let refresh_before = client.refresh_token_value().await;
    let result = dispatch(&cli, &client).await;
    // GoTrue rotates refresh tokens; persist the new one or the next run's
    // refresh would fail.
    if stored && client.refresh_token_value().await != refresh_before {
        save_credentials(&client).await?;
    }
    result
}

async fn dispatch(cli: &Cli, client: &AppFlowyClient) -> Result<()> {
    match &cli.command {
        Command::Use { target } => cmd_use(client, target.as_deref()).await,
        Command::Workspaces => cmd_workspaces(client, cli.json).await,
        Command::Ls { path } => {
            let workspace_id = current_workspace(client, cli.workspace.as_deref()).await?;
            cmd_ls(client, &workspace_id, path.as_deref(), cli.json).await
        }
        Command::Tree { path } => {
            let workspace_id = current_workspace(client, cli.workspace.as_deref()).await?;
            cmd_tree(client, &workspace_id, path.as_deref(), cli.json).await
        }
        Command::Search { query, limit } => {
            let workspace_id = current_workspace(client, cli.workspace.as_deref()).await?;
            cmd_search(client, &workspace_id, query, *limit, cli.json).await
        }
        Command::Export { path, output } => {
            let workspace_id = current_workspace(client, cli.workspace.as_deref()).await?;
            cmd_export(client, &workspace_id, path.as_deref(), output, cli.json).await
        }
        Command::Import(args) => {
            let workspace_id = current_workspace(client, cli.workspace.as_deref()).await?;
            cmd_import(client, &workspace_id, args, cli.json).await
        }
        Command::Save(args) => {
            let workspace_id = current_workspace(client, cli.workspace.as_deref()).await?;
            cmd_save(client, &workspace_id, args, cli.json).await
        }
        // Handled in run_inner before the authentication chain.
        Command::Serve(_) | Command::Login(_) | Command::Logout | Command::Update(_) => Ok(()),
    }
}

/// CLI entry for the `appflowy` binary: parse args, run, and translate the
/// outcome into a process exit code the way `cli.py`'s `main` does
/// (`error: {e}` on stderr, exit 1). Deliberately does NOT initialize
/// tracing — the Python CLI is silent except for its own output.
pub async fn run() -> i32 {
    let cli = Cli::parse();
    match run_inner(cli).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// MCP logs never go to stdout: stdio mode uses stdout as the protocol
/// channel, and the HTTP mode keeps it clean for consistency. Idempotent —
/// both serve paths (and tests) may call this.
fn init_tracing() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .init();
    });
}

/// MCP server over stdio (the `appflowy-mcp` entry point). stdout is the
/// protocol channel — every log line must go to stderr.
pub async fn serve() -> anyhow::Result<()> {
    init_tracing();

    dotenvy::dotenv().ok();
    let server = AppServer::from_env();
    server.warm_login().await;

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Entry for the CLI `serve` subcommand: stdio by default, Streamable HTTP
/// when `--http` is given.
async fn serve_with(args: &ServeArgs) -> anyhow::Result<()> {
    match &args.http {
        None => serve().await,
        Some(addr) => serve_http(addr, args.allow_remote).await,
    }
}

/// MCP server over Streamable HTTP at `http://ADDR/mcp`.
async fn serve_http(addr: &str, allow_remote: bool) -> anyhow::Result<()> {
    init_tracing();

    let bind: std::net::SocketAddr = addr
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --http address {addr:?}: {e}"))?;
    if !bind.ip().is_loopback() && !allow_remote {
        anyhow::bail!(
            "refusing to bind the MCP HTTP server to non-loopback {bind} without \
             `--allow-remote`: any reachable client could drive every tool with \
             your AppFlowy credentials. Re-run with --allow-remote to override."
        );
    }

    dotenvy::dotenv().ok();
    let server = AppServer::from_env();
    server.warm_login().await;

    let listener = tokio::net::TcpListener::bind(bind).await?;
    let local = listener.local_addr()?;
    tracing::info!("MCP Streamable HTTP listening on http://{local}/mcp");
    axum::serve(listener, mcp_http_router(&server, allow_remote))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// The `/mcp` router for Streamable HTTP, pub for tests. Defaults: only
/// loopback `Host` authorities pass (DNS-rebinding protection), and any
/// browser `Origin` is rejected; `allow_remote` disables the host allowlist
/// for public deployments.
#[doc(hidden)]
pub fn mcp_http_router(server: &AppServer, allow_remote: bool) -> axum::Router {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    let mut config = StreamableHttpServerConfig::default().enforce_origin_validation();
    if allow_remote {
        config = config.disable_allowed_hosts();
    }

    let handler = server.clone();
    let service = StreamableHttpService::new(
        move || -> std::result::Result<AppServer, std::io::Error> { Ok(handler.clone()) },
        std::sync::Arc::new(LocalSessionManager::default()),
        config,
    );
    axum::Router::new().route_service("/mcp", service)
}

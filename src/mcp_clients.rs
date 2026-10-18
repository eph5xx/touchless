//! Connects AI apps on this Mac to the bundled `touchless-mcp` server.
//!
//! Apps with a CLI (Codex, Claude Code, Droid) are changed through it; the rest
//! get their config file edited directly. Status always comes from the config
//! files, which is fast and never starts any servers.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::process::Command;
use tokio::sync::OnceCell;
use toml_edit::{DocumentMut, Item};

const SERVER_NAME: &str = "touchless";
const PATH_MARKER: &str = "__TOUCHLESS_PATH__";
const SHELL_TIMEOUT: Duration = Duration::from_secs(5);
const CLI_TIMEOUT: Duration = Duration::from_secs(20);
/// Where CLIs usually live, in case the login shell doesn't add them to PATH.
const EXTRA_DIRS: [&str; 6] = [
    "~/.local/bin",
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "~/.factory/bin",
    "~/.bun/bin",
    "~/.volta/bin",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientId {
    Codex,
    ClaudeCode,
    Cursor,
    Droid,
    ClaudeDesktop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The `touchless` entry runs this copy of `touchless-mcp`.
    Connected,
    /// The `touchless` entry runs some other binary.
    Outdated,
    Disconnected,
    /// The app isn't installed.
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Json,
    Toml,
}

#[derive(Debug, Serialize)]
pub struct ClientView {
    pub id: ClientId,
    pub name: &'static str,
    pub status: Status,
    pub hint: &'static str,
}

#[derive(Debug, Serialize)]
pub struct McpView {
    pub clients: Vec<ClientView>,
    pub server_path: String,
    pub server_exists: bool,
}

impl ClientId {
    pub const ALL: [ClientId; 5] = [
        ClientId::Codex,
        ClientId::ClaudeCode,
        ClientId::Cursor,
        ClientId::Droid,
        ClientId::ClaudeDesktop,
    ];

    fn name(self) -> &'static str {
        match self {
            ClientId::Codex => "Codex",
            ClientId::ClaudeCode => "Claude Code",
            ClientId::Cursor => "Cursor",
            ClientId::Droid => "Factory Droid",
            ClientId::ClaudeDesktop => "Claude Desktop",
        }
    }

    fn cli(self) -> Option<&'static str> {
        match self {
            ClientId::Codex => Some("codex"),
            ClientId::ClaudeCode => Some("claude"),
            ClientId::Droid => Some("droid"),
            ClientId::Cursor | ClientId::ClaudeDesktop => None,
        }
    }

    fn config_path(self, home: &Path) -> PathBuf {
        match self {
            ClientId::Codex => home.join(".codex/config.toml"),
            ClientId::ClaudeCode => home.join(".claude.json"),
            ClientId::Cursor => home.join(".cursor/mcp.json"),
            ClientId::Droid => home.join(".factory/mcp.json"),
            ClientId::ClaudeDesktop => {
                home.join("Library/Application Support/Claude/claude_desktop_config.json")
            }
        }
    }

    fn format(self) -> Format {
        match self {
            ClientId::Codex => Format::Toml,
            _ => Format::Json,
        }
    }

    /// `~/.claude.json` holds Claude Code's whole state, so only its CLI may change it.
    fn can_edit_file(self) -> bool {
        self != ClientId::ClaudeCode
    }

    fn ready_hint(self) -> &'static str {
        match self {
            ClientId::Codex => "Start a new Codex session to use it.",
            ClientId::ClaudeCode => "Start a new Claude Code session to use it.",
            ClientId::Cursor => "Available right away.",
            ClientId::Droid => "Start a new Droid session to use it.",
            ClientId::ClaudeDesktop => "Quit and reopen Claude to use it.",
        }
    }
}

/// Where to look for apps and CLIs. Tests point `home` at a temp folder.
pub struct Env {
    pub home: PathBuf,
    pub app_dirs: Vec<PathBuf>,
    pub search_path: String,
}

impl Env {
    pub async fn detect() -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("Could not find your home folder")?;
        let current = std::env::var("PATH").ok();
        let search_path = search_path(shell_path().await, current.as_deref(), &home);
        Ok(Self {
            app_dirs: vec![PathBuf::from("/Applications"), home.join("Applications")],
            home,
            search_path,
        })
    }

    fn find_cli(&self, id: ClientId) -> Option<PathBuf> {
        id.cli()
            .and_then(|name| find_program(name, &self.search_path))
    }

    fn has_app(&self, bundle: &str) -> bool {
        self.app_dirs.iter().any(|dir| dir.join(bundle).is_dir())
    }

    fn installed(&self, id: ClientId) -> bool {
        let home = &self.home;
        match id {
            ClientId::Codex => self.find_cli(id).is_some() || home.join(".codex").is_dir(),
            ClientId::ClaudeCode => self.find_cli(id).is_some(),
            ClientId::Droid => self.find_cli(id).is_some() || home.join(".factory").is_dir(),
            ClientId::Cursor => self.has_app("Cursor.app") || home.join(".cursor").is_dir(),
            ClientId::ClaudeDesktop => self.has_app("Claude.app") || id.config_path(home).is_file(),
        }
    }
}

pub fn view(env: &Env, server: &Path) -> McpView {
    let clients = ClientId::ALL
        .iter()
        .map(|&id| {
            let status = status_from(
                env.installed(id),
                configured_command(env, id).as_deref(),
                server,
            );
            ClientView {
                id,
                name: id.name(),
                status,
                hint: hint(id, status),
            }
        })
        .collect();
    McpView {
        clients,
        server_path: server.display().to_string(),
        server_exists: server.is_file(),
    }
}

pub fn status_from(installed: bool, command: Option<&str>, server: &Path) -> Status {
    match command {
        _ if !installed => Status::Missing,
        None => Status::Disconnected,
        Some(command) if Path::new(command) == server => Status::Connected,
        Some(_) => Status::Outdated,
    }
}

fn hint(id: ClientId, status: Status) -> &'static str {
    match status {
        Status::Connected => id.ready_hint(),
        Status::Outdated => "Points to another copy of Touchless. Update to use this one.",
        Status::Disconnected => "Not connected.",
        Status::Missing => "Not found on this Mac.",
    }
}

/// The command of the app's `touchless` entry, if it has one.
fn configured_command(env: &Env, id: ClientId) -> Option<String> {
    let text = fs::read_to_string(id.config_path(&env.home)).ok()?;
    match id.format() {
        Format::Json => json_command(&text),
        Format::Toml => toml_command(&text),
    }
}

/// Adds the `touchless` entry, replacing an existing one.
pub async fn connect(env: &Env, id: ClientId, server: &Path) -> Result<(), String> {
    if !env.installed(id) {
        return Err(format!("{} isn't installed on this Mac.", id.name()));
    }
    let server = server
        .to_str()
        .ok_or("The server path isn't valid UTF-8.")?;
    // `droid mcp add` splits its command on spaces, so such paths go into the file instead.
    let cli = env
        .find_cli(id)
        .filter(|_| !(id == ClientId::Droid && server.contains(char::is_whitespace)));
    if let Some(cli) = cli {
        if configured_command(env, id).is_some() {
            // If this fails, `add` fails too and reports why.
            let _ = run_cli(env, &cli, &remove_args(id)).await;
        }
        return run_cli(env, &cli, &add_args(id, server)).await;
    }
    if !id.can_edit_file() {
        return Err(format!("Could not find the {} CLI.", id.name()));
    }
    edit_config(env, id, |text| match id.format() {
        Format::Json => json_upsert(text, json_entry(id, server)).map(Some),
        Format::Toml => toml_upsert(text, server).map(Some),
    })
}

/// Removes the `touchless` entry, if there is one.
pub async fn disconnect(env: &Env, id: ClientId) -> Result<(), String> {
    if configured_command(env, id).is_none() {
        return Ok(());
    }
    if let Some(cli) = env.find_cli(id) {
        return run_cli(env, &cli, &remove_args(id)).await;
    }
    if !id.can_edit_file() {
        return Err(format!("Could not find the {} CLI.", id.name()));
    }
    edit_config(env, id, |text| match id.format() {
        Format::Json => json_remove(text),
        Format::Toml => toml_remove(text),
    })
}

pub fn add_args(id: ClientId, server: &str) -> Vec<String> {
    let args: &[&str] = match id {
        ClientId::Codex => &["mcp", "add", SERVER_NAME, "--"],
        ClientId::ClaudeCode => &["mcp", "add", "--scope", "user", SERVER_NAME, "--"],
        ClientId::Droid => &["mcp", "add", SERVER_NAME],
        ClientId::Cursor | ClientId::ClaudeDesktop => &[],
    };
    args.iter()
        .copied()
        .chain([server])
        .map(String::from)
        .collect()
}

pub fn remove_args(id: ClientId) -> Vec<String> {
    let args: &[&str] = match id {
        ClientId::ClaudeCode => &["mcp", "remove", "--scope", "user", SERVER_NAME],
        _ => &["mcp", "remove", SERVER_NAME],
    };
    args.iter().copied().map(String::from).collect()
}

async fn run_cli(env: &Env, cli: &Path, args: &[String]) -> Result<(), String> {
    let program = cli.file_name().unwrap_or(cli.as_os_str()).to_string_lossy();
    let shown = format!("{program} {}", args.join(" "));
    let mut command = Command::new(cli);
    command
        .args(args)
        .env("PATH", &env.search_path)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(CLI_TIMEOUT, command.output())
        .await
        .map_err(|_| format!("`{shown}` took too long."))?
        .map_err(|error| format!("Could not run {}: {error}", cli.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|text| !text.is_empty())
        .unwrap_or("no output");
    Err(format!("`{shown}` failed: {detail}"))
}

/// Applies `change` to the app's config file. `change` returns `None` when there is nothing to write.
fn edit_config(
    env: &Env,
    id: ClientId,
    change: impl FnOnce(&str) -> Result<Option<String>, String>,
) -> Result<(), String> {
    let path = id.config_path(&env.home);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("Could not read {}: {error}", path.display())),
    };
    let Some(updated) = change(&text).map_err(|error| format!("{}: {error}", path.display()))?
    else {
        return Ok(());
    };
    write_atomic(&path, &updated)
        .map_err(|error| format!("Could not write {}: {error}", path.display()))
}

/// Writes through a temp file and a rename so the app never sees a half-written file.
fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    // Follow a symlinked config (e.g. from a dotfiles repo) instead of replacing the link.
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .ok_or_else(|| io::Error::other("no parent folder"))?;
    fs::create_dir_all(dir)?;
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(".{name}.touchless-tmp"));
    let result = (|| {
        fs::write(&temp, contents)?;
        if let Ok(metadata) = fs::metadata(&target) {
            fs::set_permissions(&temp, metadata.permissions())?;
        }
        fs::rename(&temp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn json_entry(id: ClientId, server: &str) -> Value {
    match id {
        ClientId::Droid => json!({ "type": "stdio", "command": server, "args": [] }),
        _ => json!({ "command": server, "args": [] }),
    }
}

fn parse_json(text: &str) -> Result<Map<String, Value>, String> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err("expected a JSON object at the top level. Fix the file and try again.".into()),
        Err(error) => Err(format!(
            "isn't valid JSON ({error}). Fix the file and try again."
        )),
    }
}

fn to_json(map: Map<String, Value>) -> String {
    let mut text = serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_default();
    text.push('\n');
    text
}

pub fn json_command(text: &str) -> Option<String> {
    let map = parse_json(text).ok()?;
    let command = map.get("mcpServers")?.get(SERVER_NAME)?.get("command")?;
    command.as_str().map(String::from)
}

/// Sets `mcpServers.touchless`, keeping every other key where it was.
pub fn json_upsert(text: &str, entry: Value) -> Result<String, String> {
    let mut map = parse_json(text)?;
    let servers = map
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or("`mcpServers` isn't a JSON object. Fix the file and try again.")?;
    servers.insert(SERVER_NAME.to_owned(), entry);
    Ok(to_json(map))
}

/// Removes `mcpServers.touchless`; `None` if it isn't there.
pub fn json_remove(text: &str) -> Result<Option<String>, String> {
    let mut map = parse_json(text)?;
    let removed = map
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .and_then(|servers| servers.shift_remove(SERVER_NAME));
    Ok(removed.map(|_| to_json(map)))
}

fn parse_toml(text: &str) -> Result<DocumentMut, String> {
    text.parse::<DocumentMut>()
        .map_err(|error| format!("isn't valid TOML ({error}). Fix the file and try again."))
}

pub fn toml_command(text: &str) -> Option<String> {
    let document = parse_toml(text).ok()?;
    let command = document
        .get("mcp_servers")?
        .get(SERVER_NAME)?
        .get("command")?;
    command.as_str().map(String::from)
}

/// Sets `[mcp_servers.touchless]`, keeping comments and other tables.
pub fn toml_upsert(text: &str, server: &str) -> Result<String, String> {
    let mut document = parse_toml(text)?;
    let servers = document.entry("mcp_servers").or_insert_with(|| {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    let servers = servers
        .as_table_like_mut()
        .ok_or("`mcp_servers` isn't a table. Fix the file and try again.")?;
    let mut entry = toml_edit::Table::new();
    entry.insert("command", toml_edit::value(server));
    entry.insert("args", toml_edit::value(toml_edit::Array::new()));
    servers.insert(SERVER_NAME, Item::Table(entry));
    Ok(document.to_string())
}

/// Removes `[mcp_servers.touchless]`; `None` if it isn't there.
pub fn toml_remove(text: &str) -> Result<Option<String>, String> {
    let mut document = parse_toml(text)?;
    let Some(servers) = document
        .get_mut("mcp_servers")
        .and_then(Item::as_table_like_mut)
    else {
        return Ok(None);
    };
    if servers.remove(SERVER_NAME).is_none() {
        return Ok(None);
    }
    if servers.is_empty() {
        document.remove("mcp_servers");
    }
    Ok(Some(document.to_string()))
}

/// The user's PATH from a login shell, looked up once. Apps opened from Finder
/// only get a minimal PATH, which misses Homebrew, nvm and friends.
async fn shell_path() -> Option<&'static str> {
    static SHELL_PATH: OnceCell<Option<String>> = OnceCell::const_new();
    SHELL_PATH.get_or_init(login_shell_path).await.as_deref()
}

async fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_owned());
    let script = format!("printf '%s%s%s' '{PATH_MARKER}' \"$PATH\" '{PATH_MARKER}'");
    let mut command = Command::new(shell);
    command
        .args(["-l", "-i", "-c", &script])
        // Keeps oh-my-zsh from prompting for updates.
        .env("DISABLE_AUTO_UPDATE", "true")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(SHELL_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    parse_shell_path(&String::from_utf8_lossy(&output.stdout))
}

/// Pulls PATH out of the shell's output; rc files may print anything before it.
pub fn parse_shell_path(output: &str) -> Option<String> {
    let start = output.find(PATH_MARKER)? + PATH_MARKER.len();
    let rest = &output[start..];
    let path = rest[..rest.find(PATH_MARKER)?].trim();
    (!path.is_empty()).then(|| path.to_owned())
}

fn search_path(shell: Option<&str>, current: Option<&str>, home: &Path) -> String {
    let home = home.to_string_lossy();
    let extras = EXTRA_DIRS.map(|dir| match dir.strip_prefix("~/") {
        Some(rest) => format!("{home}/{rest}"),
        None => dir.to_owned(),
    });
    let mut dirs: Vec<String> = Vec::new();
    let all = shell
        .unwrap_or_default()
        .split(':')
        .map(String::from)
        .chain(extras)
        .chain(current.unwrap_or_default().split(':').map(String::from));
    for dir in all {
        if !dir.is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs.join(":")
}

fn find_program(name: &str, search_path: &str) -> Option<PathBuf> {
    search_path
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|path| {
            fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SERVER: &str = "/Applications/Touchless.app/Contents/MacOS/touchless-mcp";

    struct TempHome(PathBuf);

    impl TempHome {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "touchless-mcp-clients-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn env(&self) -> Env {
            Env {
                home: self.0.clone(),
                app_dirs: vec![self.0.join("Applications")],
                search_path: String::new(),
            }
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.0.join(relative)).unwrap()
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn status(env: &Env, id: ClientId) -> Status {
        let view = view(env, Path::new(SERVER));
        view.clients
            .into_iter()
            .find(|client| client.id == id)
            .unwrap()
            .status
    }

    #[test]
    fn json_upsert_keeps_other_servers_and_key_order() {
        let text = r#"{"zeta": 1, "mcpServers": {"other": {"command": "x"}, "touchless": {"command": "/old"}, "last": {}}, "alpha": true}"#;
        let updated = json_upsert(text, json!({ "command": SERVER })).unwrap();
        let value: Value = serde_json::from_str(&updated).unwrap();
        let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["zeta", "mcpServers", "alpha"]);
        let servers: Vec<_> = value["mcpServers"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(servers, ["other", "touchless", "last"]);
        assert_eq!(value["mcpServers"]["other"]["command"], "x");
        assert_eq!(json_command(&updated).as_deref(), Some(SERVER));
    }

    #[test]
    fn json_upsert_starts_from_an_empty_file() {
        let updated = json_upsert("", json!({ "command": SERVER })).unwrap();
        assert_eq!(json_command(&updated).as_deref(), Some(SERVER));
    }

    #[test]
    fn json_remove_keeps_the_rest() {
        let text =
            r#"{"b": 1, "mcpServers": {"x": {}, "touchless": {"command": "/a"}, "y": {}}, "a": 2}"#;
        let updated = json_remove(text).unwrap().unwrap();
        let value: Value = serde_json::from_str(&updated).unwrap();
        let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["b", "mcpServers", "a"]);
        let servers: Vec<_> = value["mcpServers"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(servers, ["x", "y"]);
        assert_eq!(json_remove(&updated).unwrap(), None);
    }

    #[test]
    fn invalid_json_is_refused() {
        assert!(json_upsert("{ not json", json!({})).is_err());
        assert!(json_upsert("[]", json!({})).is_err());
        assert!(json_upsert(r#"{"mcpServers": []}"#, json!({})).is_err());
        assert!(json_remove("{ not json").is_err());
    }

    #[test]
    fn toml_upsert_and_remove_keep_comments_and_other_tables() {
        let text = "# my settings\nmodel = \"o3\" # favorite\n\n[mcp_servers.other]\ncommand = \"x\"\n\n[profiles.fast]\nmodel = \"mini\"\n";
        let updated = toml_upsert(text, SERVER).unwrap();
        assert!(updated.contains("# my settings"));
        assert!(updated.contains("# favorite"));
        assert!(updated.contains("[mcp_servers.other]"));
        assert!(updated.contains("[profiles.fast]"));
        assert_eq!(toml_command(&updated).as_deref(), Some(SERVER));

        let again = toml_upsert(&updated, "/new/path").unwrap();
        assert_eq!(toml_command(&again).as_deref(), Some("/new/path"));
        assert_eq!(again.matches("[mcp_servers.touchless]").count(), 1);

        let removed = toml_remove(&again).unwrap().unwrap();
        assert_eq!(removed, text);
        assert_eq!(toml_remove(&removed).unwrap(), None);
    }

    #[test]
    fn toml_round_trip_on_an_empty_file() {
        let updated = toml_upsert("", SERVER).unwrap();
        assert!(!updated.contains("[mcp_servers]\n"));
        assert_eq!(toml_command(&updated).as_deref(), Some(SERVER));
        assert_eq!(toml_remove(&updated).unwrap().unwrap().trim(), "");
        assert!(toml_upsert("not = [valid", SERVER).is_err());
    }

    #[test]
    fn status_covers_every_state() {
        let home = TempHome::new();
        let env = home.env();
        assert_eq!(status(&env, ClientId::Codex), Status::Missing);

        home.write(".codex/config.toml", "model = \"o3\"\n");
        assert_eq!(status(&env, ClientId::Codex), Status::Disconnected);

        home.write(
            ".codex/config.toml",
            &toml_upsert("", "/somewhere/else").unwrap(),
        );
        assert_eq!(status(&env, ClientId::Codex), Status::Outdated);

        home.write(".codex/config.toml", &toml_upsert("", SERVER).unwrap());
        assert_eq!(status(&env, ClientId::Codex), Status::Connected);

        // Claude Code needs its CLI, which isn't on the empty search path.
        home.write(
            ".claude.json",
            &json_upsert("", json!({ "command": SERVER })).unwrap(),
        );
        assert_eq!(status(&env, ClientId::ClaudeCode), Status::Missing);

        fs::create_dir_all(home.0.join("Applications/Cursor.app")).unwrap();
        assert_eq!(status(&env, ClientId::Cursor), Status::Disconnected);
        home.write(
            ".cursor/mcp.json",
            r#"{"mcpServers": {"touchless": {"command": "/old"}}}"#,
        );
        assert_eq!(status(&env, ClientId::Cursor), Status::Outdated);

        home.write(
            "Library/Application Support/Claude/claude_desktop_config.json",
            r#"{"preferences": {}}"#,
        );
        assert_eq!(status(&env, ClientId::ClaudeDesktop), Status::Disconnected);
    }

    #[test]
    fn status_from_compares_the_command() {
        let server = Path::new(SERVER);
        assert_eq!(status_from(false, Some(SERVER), server), Status::Missing);
        assert_eq!(status_from(true, None, server), Status::Disconnected);
        assert_eq!(status_from(true, Some(SERVER), server), Status::Connected);
        assert_eq!(
            status_from(true, Some("/dev/build"), server),
            Status::Outdated
        );
    }

    #[tokio::test]
    async fn connect_and_disconnect_edit_files() {
        let home = TempHome::new();
        let env = home.env();
        let server = Path::new(SERVER);

        assert!(connect(&env, ClientId::Cursor, server).await.is_err());
        assert!(!home.0.join(".cursor").exists());

        home.write(
            ".cursor/mcp.json",
            r#"{"mcpServers": {"other": {"command": "x"}}}"#,
        );
        connect(&env, ClientId::Cursor, server).await.unwrap();
        assert_eq!(status(&env, ClientId::Cursor), Status::Connected);
        disconnect(&env, ClientId::Cursor).await.unwrap();
        assert_eq!(status(&env, ClientId::Cursor), Status::Disconnected);
        assert_eq!(json_command(&home.read(".cursor/mcp.json")), None);
        assert!(home.read(".cursor/mcp.json").contains("\"other\""));

        // Droid falls back to its file when the CLI isn't found.
        fs::create_dir_all(home.0.join(".factory")).unwrap();
        connect(&env, ClientId::Droid, server).await.unwrap();
        let droid: Value = serde_json::from_str(&home.read(".factory/mcp.json")).unwrap();
        assert_eq!(droid["mcpServers"]["touchless"]["type"], "stdio");

        home.write(".codex/config.toml", "# keep me\n");
        connect(&env, ClientId::Codex, server).await.unwrap();
        assert!(home.read(".codex/config.toml").contains("# keep me\n"));
        assert_eq!(status(&env, ClientId::Codex), Status::Connected);

        home.write(".cursor/mcp.json", "{ broken");
        assert!(connect(&env, ClientId::Cursor, server).await.is_err());
        assert_eq!(home.read(".cursor/mcp.json"), "{ broken");
    }

    #[test]
    fn shell_path_ignores_noise_before_the_marker() {
        let output = format!(
            "Welcome back!\nLast login: today\n{PATH_MARKER}/opt/homebrew/bin:/usr/bin{PATH_MARKER}"
        );
        assert_eq!(
            parse_shell_path(&output).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        assert_eq!(parse_shell_path("no marker here"), None);
        assert_eq!(
            parse_shell_path(&format!("{PATH_MARKER}{PATH_MARKER}")),
            None
        );
    }

    #[test]
    fn search_path_adds_known_folders_once() {
        let path = search_path(
            Some("/a:/opt/homebrew/bin"),
            Some("/usr/bin:/a"),
            Path::new("/Users/me"),
        );
        let dirs: Vec<_> = path.split(':').collect();
        assert_eq!(dirs[0], "/a");
        assert_eq!(
            dirs.iter()
                .filter(|dir| **dir == "/opt/homebrew/bin")
                .count(),
            1
        );
        assert!(dirs.contains(&"/Users/me/.local/bin"));
        assert_eq!(dirs.last(), Some(&"/usr/bin"));
    }

    #[test]
    fn cli_arguments_match_each_app() {
        assert_eq!(
            add_args(ClientId::Codex, SERVER),
            ["mcp", "add", "touchless", "--", SERVER]
        );
        assert_eq!(
            add_args(ClientId::ClaudeCode, SERVER),
            ["mcp", "add", "--scope", "user", "touchless", "--", SERVER]
        );
        assert_eq!(
            add_args(ClientId::Droid, SERVER),
            ["mcp", "add", "touchless", SERVER]
        );
        assert_eq!(remove_args(ClientId::Codex), ["mcp", "remove", "touchless"]);
        assert_eq!(
            remove_args(ClientId::ClaudeCode),
            ["mcp", "remove", "--scope", "user", "touchless"]
        );
        assert_eq!(remove_args(ClientId::Droid), ["mcp", "remove", "touchless"]);
    }
}

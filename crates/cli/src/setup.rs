//! `zenodotus setup <client>`: how to register the stdio server with an MCP client, and, behind `--write`, the edit
//! of that client's config file.

use std::path::{Path, PathBuf};

use zenodotus_core::CoreError;
use zenodotus_core::config::Env;
use zenodotus_core::time::Clock;

use crate::error::AppError;

/// The name the server is registered under.
pub const SERVER_NAME: &str = "zenodotus";

/// An MCP client that starts servers over stdio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Client {
    /// Claude Code, through `claude mcp add`.
    ClaudeCode,
    /// Cursor, through `~/.cursor/mcp.json`.
    Cursor,
    /// Codex, through `codex mcp add` or `~/.codex/config.toml`.
    Codex,
    /// VS Code with Copilot, through `~/.copilot/mcp-config.json`.
    Vscode,
    /// Any other client: the command and the usual JSON entry.
    Generic,
}

/// The server process a client starts: the program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    /// The `zenodotus` executable.
    pub program: String,
    /// `serve`, preceded by `--config <PATH>` when one was given.
    pub args: Vec<String>,
}

impl Server {
    /// The server started as `program [--config <config>] serve`.
    #[must_use]
    pub fn new(program: &Path, config: Option<&Path>) -> Self {
        let mut args = Vec::new();
        if let Some(config) = config {
            args.push("--config".to_owned());
            args.push(config.display().to_string());
        }
        args.push("serve".to_owned());
        Self {
            program: program.display().to_string(),
            args,
        }
    }

    /// The command line, quoted for a POSIX shell.
    fn command_line(&self) -> String {
        std::iter::once(&self.program)
            .chain(&self.args)
            .map(|word| shell_quote(word))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The JSON entry under `mcpServers`; VS Code also wants the transport and the tools.
    fn json_entry(&self, client: Client) -> serde_json::Value {
        let mut entry = serde_json::json!({"command": self.program, "args": self.args});
        if client == Client::Vscode {
            entry["type"] = "stdio".into();
            entry["tools"] = serde_json::json!(["*"]);
        }
        entry
    }

    /// The `[mcp_servers.zenodotus]` table of Codex.
    fn toml_entry(&self) -> toml_edit::Table {
        let mut table = toml_edit::Table::new();
        table.insert("command", toml_edit::value(self.program.as_str()));
        let args: toml_edit::Array = self.args.iter().map(String::as_str).collect();
        table.insert("args", toml_edit::value(args));
        table
    }
}

/// What `--write` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The edited config file.
    pub path: PathBuf,
    /// The copy of the file as it was, when it existed.
    pub backup: Option<PathBuf>,
    /// Whether an entry named [`SERVER_NAME`] was already there and was replaced.
    pub replaced: bool,
}

/// `word`, single-quoted unless it only holds characters a POSIX shell takes literally.
fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// A non-empty environment variable.
fn var(env: Env<'_>, name: &str) -> Option<PathBuf> {
    env(name).filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// The home directory: `$HOME`, or `%USERPROFILE%` on Windows.
fn home(env: Env<'_>) -> Result<PathBuf, AppError> {
    var(env, "HOME")
        .or_else(|| var(env, "USERPROFILE"))
        .ok_or(AppError::Core(CoreError::NoLocation { what: "home directory" }))
}

/// The config file `client` reads, when zenodotus can edit it.
///
/// # Errors
///
/// [`AppError::Usage`] for a client whose config zenodotus does not edit, and [`AppError::Core`] when no home
/// directory is known.
pub fn config_file(client: Client, env: Env<'_>) -> Result<PathBuf, AppError> {
    match client {
        Client::ClaudeCode => Err(AppError::Usage(
            "Claude Code keeps its own config; run the `claude mcp add` command that `zenodotus setup claude-code` \
             prints"
                .to_owned(),
        )),
        Client::Generic => Err(AppError::Usage(
            "a generic client has no known config file; add the entry that `zenodotus setup generic` prints".to_owned(),
        )),
        Client::Cursor => Ok(home(env)?.join(".cursor/mcp.json")),
        Client::Codex => Ok(match var(env, "CODEX_HOME") {
            Some(dir) => dir,
            None => home(env)?.join(".codex"),
        }
        .join("config.toml")),
        Client::Vscode => Ok(match var(env, "COPILOT_HOME") {
            Some(dir) => dir,
            None => home(env)?.join(".copilot"),
        }
        .join("mcp-config.json")),
    }
}

/// `{"mcpServers": {"zenodotus": <entry>}}`, pretty-printed.
fn json_snippet(server: &Server, client: Client) -> String {
    let snippet = serde_json::json!({"mcpServers": {SERVER_NAME: server.json_entry(client)}});
    serde_json::to_string_pretty(&snippet).unwrap_or_default()
}

/// The instructions that register `server` with `client`.
///
/// # Errors
///
/// [`AppError::Core`] when the client's config file has no known location.
pub fn instructions(client: Client, server: &Server, env: Env<'_>) -> Result<String, AppError> {
    let command = server.command_line();
    Ok(match client {
        Client::ClaudeCode => format!(
            "Claude Code: run\n\n  claude mcp add --scope user {SERVER_NAME} -- {command}\n\n\
             then check it with `claude mcp list`.\n"
        ),
        Client::Generic => format!(
            "Any MCP client over stdio: start `{command}`. The usual JSON entry is:\n\n{}\n",
            json_snippet(server, client)
        ),
        Client::Cursor | Client::Vscode => {
            let (name, flag) = if client == Client::Cursor {
                ("Cursor", "cursor")
            } else {
                ("VS Code (Copilot)", "vscode")
            };
            format!(
                "{name} reads {}. Add this entry, or run `zenodotus setup {flag} --write`, which backs the file up \
                 first:\n\n{}\n",
                config_file(client, env)?.display(),
                json_snippet(server, client)
            )
        }
        Client::Codex => format!(
            "Codex: run\n\n  codex mcp add {SERVER_NAME} -- {command}\n\nor add this table to {}, or run \
             `zenodotus setup codex --write`, which backs the file up first:\n\n{}",
            config_file(client, env)?.display(),
            edit_toml("", server).unwrap_or_default().0
        ),
    })
}

/// `text` with `server` added under `mcpServers`, and whether it replaced an entry.
fn edit_json(text: &str, server: &Server, client: Client) -> Result<(String, bool), String> {
    let mut value: serde_json::Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let root = value.as_object_mut().ok_or("the top level is not a JSON object")?;
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or("`mcpServers` is not an object")?;
    let replaced = servers
        .insert(SERVER_NAME.to_owned(), server.json_entry(client))
        .is_some();
    // A `Value` always serializes.
    let mut edited = serde_json::to_string_pretty(&value).unwrap_or_default();
    edited.push('\n');
    Ok((edited, replaced))
}

/// `text` with `server` added under `[mcp_servers]`, and whether it replaced an entry.
fn edit_toml(text: &str, server: &Server) -> Result<(String, bool), String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|error: toml_edit::TomlError| error.to_string())?;
    let servers = doc
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            toml_edit::Item::Table(table)
        })
        .as_table_mut()
        .ok_or("`mcp_servers` is not a table")?;
    let replaced = servers
        .insert(SERVER_NAME, toml_edit::Item::Table(server.toml_entry()))
        .is_some();
    Ok((doc.to_string(), replaced))
}

/// Adds `server` to `client`'s config file, after copying the file to a timestamped backup.
///
/// # Errors
///
/// [`AppError::Usage`] for a client whose config zenodotus does not edit, [`AppError::ClientConfig`] when the file
/// is not valid JSON or TOML of the expected shape, and [`AppError::Io`] when it cannot be read or written.
pub fn write(client: Client, server: &Server, env: Env<'_>, clock: &dyn Clock) -> Result<Written, AppError> {
    let path = config_file(client, env)?;
    let original = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(AppError::io(&path)(error)),
    };
    let edited = if client == Client::Codex {
        edit_toml(original.as_deref().unwrap_or_default(), server)
    } else {
        edit_json(original.as_deref().unwrap_or("{}"), server, client)
    };
    let (edited, replaced) = edited.map_err(|reason| AppError::ClientConfig {
        path: path.clone(),
        reason,
    })?;
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(AppError::io(parent))?;
    let backup = match original {
        Some(text) => {
            let backup = sibling(&path, &format!("{}.bak", clock.now().unix()));
            std::fs::write(&backup, text).map_err(AppError::io(&backup))?;
            Some(backup)
        }
        None => None,
    };
    let temporary = sibling(&path, "zenodotus.tmp");
    std::fs::write(&temporary, edited).map_err(AppError::io(&temporary))?;
    std::fs::rename(&temporary, &path).map_err(AppError::io(&path))?;
    Ok(Written { path, backup, replaced })
}

/// `path` with `.suffix` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use zenodotus_core::time::{FixedClock, Timestamp};

    use super::*;

    const HOME: &str = "/home/u";

    fn env_with(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    fn server() -> Server {
        Server::new(Path::new("/opt/bin/zenodotus"), None)
    }

    fn clock() -> FixedClock {
        FixedClock::new(Timestamp::from_unix(1_700_000_000))
    }

    #[test]
    fn the_server_runs_serve_with_an_optional_config() {
        assert_eq!(
            server(),
            Server {
                program: "/opt/bin/zenodotus".to_owned(),
                args: vec!["serve".to_owned()],
            }
        );
        let with_config = Server::new(Path::new("/opt/bin/zenodotus"), Some(Path::new("/etc/z.toml")));
        assert_eq!(with_config.args, ["--config", "/etc/z.toml", "serve"]);
    }

    #[test]
    fn config_files_follow_each_clients_home_variable() {
        let env = env_with(&[("HOME", HOME)]);
        assert_eq!(
            config_file(Client::Cursor, &env).unwrap(),
            Path::new("/home/u/.cursor/mcp.json")
        );
        assert_eq!(
            config_file(Client::Codex, &env).unwrap(),
            Path::new("/home/u/.codex/config.toml")
        );
        assert_eq!(
            config_file(Client::Vscode, &env).unwrap(),
            Path::new("/home/u/.copilot/mcp-config.json")
        );
        let env = env_with(&[("HOME", HOME), ("CODEX_HOME", "/c"), ("COPILOT_HOME", "/p")]);
        assert_eq!(config_file(Client::Codex, &env).unwrap(), Path::new("/c/config.toml"));
        assert_eq!(
            config_file(Client::Vscode, &env).unwrap(),
            Path::new("/p/mcp-config.json")
        );
        let windows = env_with(&[("USERPROFILE", "C:/Users/u")]);
        assert_eq!(
            config_file(Client::Cursor, &windows).unwrap(),
            Path::new("C:/Users/u/.cursor/mcp.json")
        );
    }

    #[test]
    fn clients_without_an_editable_file_or_a_home_are_errors() {
        let env = env_with(&[("HOME", HOME)]);
        for client in [Client::ClaudeCode, Client::Generic] {
            let error = config_file(client, &env).unwrap_err();
            assert!(matches!(error, AppError::Usage(_)), "{error}");
        }
        let nowhere = env_with(&[("HOME", "")]);
        let error = config_file(Client::Cursor, &nowhere).unwrap_err();
        assert!(matches!(error, AppError::Core(_)), "{error}");
    }

    #[test]
    fn claude_code_gets_the_mcp_add_command() {
        let env = env_with(&[("HOME", HOME)]);
        let text = instructions(Client::ClaudeCode, &server(), &env).unwrap();
        assert_eq!(
            text,
            "Claude Code: run\n\n  claude mcp add --scope user zenodotus -- /opt/bin/zenodotus serve\n\n\
             then check it with `claude mcp list`.\n"
        );
    }

    #[test]
    fn json_clients_get_their_file_and_entry() {
        let env = env_with(&[("HOME", HOME)]);
        let cursor = instructions(Client::Cursor, &server(), &env).unwrap();
        assert_eq!(
            cursor,
            "Cursor reads /home/u/.cursor/mcp.json. Add this entry, or run `zenodotus setup cursor --write`, which \
             backs the file up first:\n\n{\n  \"mcpServers\": {\n    \"zenodotus\": {\n      \"args\": [\n        \
             \"serve\"\n      ],\n      \"command\": \"/opt/bin/zenodotus\"\n    }\n  }\n}\n"
        );
        let vscode = instructions(Client::Vscode, &server(), &env).unwrap();
        assert!(
            vscode.starts_with("VS Code (Copilot) reads /home/u/.copilot/mcp-config.json."),
            "{vscode}"
        );
        assert!(vscode.contains("\"type\": \"stdio\""), "{vscode}");
        assert!(vscode.contains("\"tools\": [\n        \"*\"\n      ]"), "{vscode}");
        let generic = instructions(Client::Generic, &server(), &env).unwrap();
        assert!(
            generic.starts_with("Any MCP client over stdio: start `/opt/bin/zenodotus serve`."),
            "{generic}"
        );
        assert!(generic.contains("\"mcpServers\""), "{generic}");
    }

    #[test]
    fn codex_gets_the_command_and_the_table() {
        let env = env_with(&[("HOME", HOME)]);
        let text = instructions(Client::Codex, &server(), &env).unwrap();
        assert_eq!(
            text,
            "Codex: run\n\n  codex mcp add zenodotus -- /opt/bin/zenodotus serve\n\nor add this table to \
             /home/u/.codex/config.toml, or run `zenodotus setup codex --write`, which backs the file up first:\n\n\
             [mcp_servers.zenodotus]\ncommand = \"/opt/bin/zenodotus\"\nargs = [\"serve\"]\n"
        );
    }

    #[test]
    fn programs_with_shell_characters_are_quoted() {
        let env = env_with(&[("HOME", HOME)]);
        let spaced = Server::new(Path::new("/Program Files/it's/zenodotus"), None);
        let text = instructions(Client::ClaudeCode, &spaced, &env).unwrap();
        assert!(text.contains("-- '/Program Files/it'\\''s/zenodotus' serve"), "{text}");
    }

    #[test]
    fn instructions_need_a_home_for_file_clients() {
        let nowhere = env_with(&[]);
        assert!(instructions(Client::ClaudeCode, &server(), &nowhere).is_ok());
        assert!(instructions(Client::Generic, &server(), &nowhere).is_ok());
        for client in [Client::Cursor, Client::Codex, Client::Vscode] {
            assert!(matches!(
                instructions(client, &server(), &nowhere),
                Err(AppError::Core(_))
            ));
        }
    }

    /// A temporary home with the given variables pointing into it.
    struct TempHome {
        dir: tempfile::TempDir,
    }

    impl TempHome {
        fn new() -> Self {
            Self {
                dir: tempfile::TempDir::new().unwrap(),
            }
        }

        fn env(&self) -> impl Fn(&str) -> Option<String> + use<> {
            let home = self.dir.path().display().to_string();
            move |name: &str| (name == "HOME").then(|| home.clone())
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.dir.path().join(relative)
        }
    }

    #[test]
    fn writing_creates_a_missing_json_file_without_a_backup() {
        let home = TempHome::new();
        let written = write(Client::Cursor, &server(), &home.env(), &clock()).unwrap();
        let path = home.path(".cursor/mcp.json");
        assert_eq!(
            written,
            Written {
                path: path.clone(),
                backup: None,
                replaced: false,
            }
        );
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"mcpServers": {"zenodotus": {"command": "/opt/bin/zenodotus", "args": ["serve"]}}})
        );
    }

    #[test]
    fn writing_keeps_other_servers_and_backs_the_file_up() {
        let home = TempHome::new();
        let path = home.path(".copilot/mcp-config.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = r#"{"mcpServers": {"other": {"command": "x"}, "zenodotus": {"command": "old"}}, "keep": 1}"#;
        std::fs::write(&path, original).unwrap();
        let written = write(Client::Vscode, &server(), &home.env(), &clock()).unwrap();
        let backup = home.path(".copilot/mcp-config.json.1700000000.bak");
        assert_eq!(written.backup.as_deref(), Some(backup.as_path()));
        assert!(written.replaced);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "keep": 1,
                "mcpServers": {
                    "other": {"command": "x"},
                    "zenodotus": {"type": "stdio", "command": "/opt/bin/zenodotus", "args": ["serve"], "tools": ["*"]}
                }
            })
        );
    }

    #[test]
    fn writing_codex_keeps_the_rest_of_the_toml() {
        let home = TempHome::new();
        let path = home.path(".codex/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# mine\nmodel = \"m\"\n\n[mcp_servers.other]\ncommand = \"x\"\n").unwrap();
        let written = write(Client::Codex, &server(), &home.env(), &clock()).unwrap();
        assert!(!written.replaced);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\nmodel = \"m\"\n\n[mcp_servers.other]\ncommand = \"x\"\n\n[mcp_servers.zenodotus]\n\
             command = \"/opt/bin/zenodotus\"\nargs = [\"serve\"]\n"
        );
        let again = write(Client::Codex, &server(), &home.env(), &clock()).unwrap();
        assert!(again.replaced);
        let fresh = TempHome::new();
        write(Client::Codex, &server(), &fresh.env(), &clock()).unwrap();
        assert_eq!(
            std::fs::read_to_string(fresh.path(".codex/config.toml")).unwrap(),
            "[mcp_servers.zenodotus]\ncommand = \"/opt/bin/zenodotus\"\nargs = [\"serve\"]\n"
        );
    }

    #[test]
    fn files_of_the_wrong_shape_are_left_alone() {
        let cases: [(Client, &str, &str); 5] = [
            (Client::Cursor, ".cursor/mcp.json", "{not json"),
            (Client::Cursor, ".cursor/mcp.json", "[1, 2]"),
            (Client::Cursor, ".cursor/mcp.json", r#"{"mcpServers": 3}"#),
            (Client::Codex, ".codex/config.toml", "= nope"),
            (Client::Codex, ".codex/config.toml", "mcp_servers = 3\n"),
        ];
        for (client, relative, text) in cases {
            let home = TempHome::new();
            let path = home.path(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            let error = write(client, &server(), &home.env(), &clock()).unwrap_err();
            assert!(
                matches!(&error, AppError::ClientConfig { path: p, .. } if *p == path),
                "{error}"
            );
            assert_eq!(error.exit_code(), 2);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
            assert_eq!(
                std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
                1,
                "no backup or temporary file"
            );
        }
    }

    #[test]
    fn unwritable_places_and_uneditable_clients_are_errors() {
        let home = TempHome::new();
        std::fs::write(home.path(".cursor"), "a file where the directory goes").unwrap();
        let error = write(Client::Cursor, &server(), &home.env(), &clock()).unwrap_err();
        assert!(matches!(error, AppError::Io { .. }), "{error}");
        let home = TempHome::new();
        std::fs::create_dir_all(home.path(".codex/config.toml")).unwrap();
        let error = write(Client::Codex, &server(), &home.env(), &clock()).unwrap_err();
        assert!(matches!(error, AppError::Io { .. }), "{error}");
        let error = write(Client::ClaudeCode, &server(), &home.env(), &clock()).unwrap_err();
        assert!(matches!(error, AppError::Usage(_)), "{error}");
    }
}

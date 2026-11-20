//! The command line.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use zenodotus_mcp::server::{MAX_CHARS, MAX_RESULTS};

/// Deterministic documentation retrieval over MCP.
#[derive(Debug, Parser)]
#[command(name = "zenodotus", version, about)]
pub struct Cli {
    /// The config file; otherwise `$ZENODOTUS_CONFIG`, then `$XDG_CONFIG_HOME/zenodotus/zenodotus.toml`, then
    /// `~/.config/zenodotus/zenodotus.toml`.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Write a commented config file and create the data directory.
    Init,
    /// Add a library, or a version of one, to the config.
    Add(AddArgs),
    /// List the configured libraries and versions with their indexed status.
    List(JsonFlag),
    /// Index every configured version, or the versions of one library.
    Sync(SyncArgs),
    /// Search one library, as the `query_docs` tool does.
    Search(SearchArgs),
    /// Serve the MCP tools over stdio, or over Streamable HTTP with --http.
    Serve(ServeArgs),
    /// Show the sync status of every indexed version and the index size.
    Status(JsonFlag),
}

/// `--json`.
#[derive(Debug, Args)]
pub struct JsonFlag {
    /// Print JSON instead of text.
    #[arg(long)]
    pub json: bool,
}

/// `add`.
#[derive(Debug, Args)]
pub struct AddArgs {
    /// The library id, such as `tokio` or `next.js`.
    pub id: String,
    /// The version label.
    #[arg(long)]
    pub version: String,
    /// Where the documents come from.
    #[command(flatten)]
    pub source: SourceArgs,
    /// Use this version when a query names none.
    #[arg(long)]
    pub default: bool,
    /// The display name of the library.
    #[arg(long)]
    pub name: Option<String>,
    /// A one-line description of the library.
    #[arg(long)]
    pub description: Option<String>,
    /// With --github: a branch, tag or commit.
    #[arg(long = "ref", value_name = "REF", requires = "github")]
    pub reference: Option<String>,
    /// With --github: a directory to index; may repeat.
    #[arg(long, value_name = "DIR", requires = "github")]
    pub docs_path: Vec<String>,
    /// With --website: the most pages to fetch.
    #[arg(long, value_name = "N", requires = "website")]
    pub max_pages: Option<u32>,
}

/// The source of an added version: exactly one of the source flags.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub struct SourceArgs {
    /// A local directory of Markdown files.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,
    /// A GitHub repository, `owner/name`.
    #[arg(long, value_name = "REPO")]
    pub github: Option<String>,
    /// Markdown documents at fixed URLs.
    #[arg(long, value_name = "URL", num_args = 1..)]
    pub url: Vec<String>,
    /// An `llms.txt` file.
    #[arg(long, value_name = "URL")]
    pub llms_txt: Option<String>,
    /// The start page of a documentation website.
    #[arg(long, value_name = "URL")]
    pub website: Option<String>,
}

/// `sync`.
#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Only this library; without it, every library is synced and unconfigured versions are removed.
    pub id: Option<String>,
    /// Only this version of the library.
    #[arg(long, requires = "id")]
    pub version: Option<String>,
    /// Rebuild the whole index from scratch, replacing it unless a version fails (a `partial` version still counts).
    #[arg(long, conflicts_with_all = ["id", "version"])]
    pub rebuild: bool,
}

/// `search`.
#[derive(Debug, Args)]
pub struct SearchArgs {
    /// The library id.
    pub id: String,
    /// The question or keywords.
    #[arg(required = true, num_args = 1..)]
    pub query: Vec<String>,
    /// The version; the default version when absent.
    #[arg(long)]
    pub version: Option<String>,
    /// A code language to favour.
    #[arg(long)]
    pub language: Option<String>,
    /// How many sections to return, from 1 to 20.
    #[arg(long, default_value_t = MAX_RESULTS.2)]
    pub limit: u64,
    /// The character budget, from 1000 to 100000.
    #[arg(long, default_value_t = MAX_CHARS.2)]
    pub max_chars: u64,
    /// Print the structured result as JSON.
    #[arg(long)]
    pub json: bool,
}

/// `serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Serve Streamable HTTP instead of stdio, on ADDR or else on `settings.http.listen`.
    #[arg(long, value_name = "ADDR")]
    pub http: Option<Option<SocketAddr>>,
}

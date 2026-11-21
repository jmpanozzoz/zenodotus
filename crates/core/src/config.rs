//! The `zenodotus.toml` file: `[settings]` and `[[library]]` entries, their defaults, validation and locations.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::ecosystem::Ecosystem;
use crate::error::CoreError;
use crate::ident::PROTECTED_WORDS;
use crate::ids::{CanonicalId, LibraryId, Scope, Version, to_hex};
use crate::model::SourceSpec;

/// Reads one environment variable; injected so that tests control the environment.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Largest accepted `[settings.index]` weight: each unit repeats a field's tokens once more in the index.
pub const MAX_INDEX_WEIGHT: u32 = 16;
/// Largest accepted `settings.fetch.retries`.
pub const MAX_RETRIES: u32 = 10;
/// The `max_chars` a `query_docs` call gets when it names none.
pub const DEFAULT_MAX_CHARS: u64 = 12_000;
/// The least interval between two requests to a host that the host table (design §11.2) does not list, in ms.
pub const DEFAULT_HOST_INTERVAL_MS: u64 = 1_000;
/// Hosts whose published rate allows a shorter interval than [`DEFAULT_HOST_INTERVAL_MS`], with that interval in ms.
const HOST_TABLE_MS: [(&str, u64); 2] = [("rubygems.org", 100), ("pkg.go.dev", 100)];

/// The whole configuration file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Process-wide settings.
    #[serde(default)]
    pub settings: Settings,
    /// The configured libraries, in file order.
    #[serde(default, rename = "library")]
    pub libraries: Vec<LibrarySpec>,
}

/// One `[[library]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibrarySpec {
    /// The library id.
    pub id: LibraryId,
    /// The canonical ID whose lookups at one of the configured versions resolve to this library (design §6.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical: Option<CanonicalId>,
    /// Display name; the id when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// One-line description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Other names `resolve_library` should match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// The library home page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<Url>,
    /// The documentation license.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// The indexed versions, in file order.
    #[serde(default, rename = "version")]
    pub versions: Vec<VersionSpec>,
}

impl LibrarySpec {
    /// The display name, falling back to the id.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(self.id.as_str())
    }
}

/// One `[[library.version]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionSpec {
    /// The version label.
    pub version: Version,
    /// Whether queries without a version use this one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
    /// Where the documents come from.
    pub source: SourceSpec,
    /// Hosts this source may reach besides those of its own URLs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_hosts: Vec<String>,
}

/// `[settings]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Where the index and lock live; see [`Settings::data_dir`].
    pub data_dir: Option<PathBuf>,
    /// The `User-Agent` sent with every request.
    pub user_agent: String,
    /// `[settings.fetch]`.
    pub fetch: FetchSettings,
    /// `[settings.http]`.
    pub http: HttpSettings,
    /// `[settings.ranking]`.
    pub ranking: RankingSettings,
    /// `[settings.index]`.
    pub index: IndexSettings,
    /// A recipes checkout that replaces the built-in snapshot; for contributors.
    pub recipes_dir: Option<PathBuf>,
    /// Whether every network request is refused.
    pub offline: bool,
    /// Where downloaded artifacts are cached; see [`Settings::artifacts_dir`].
    pub artifacts_dir: Option<PathBuf>,
    /// Canonical ID prefixes, such as `npm:@acme/`, that are never looked up in a public registry.
    pub private_scopes: Vec<String>,
    /// Hosts rewritten to another base URL after every check, for tests and mirrors.
    pub host_overrides: BTreeMap<String, Url>,
    /// `[settings.on_demand]`.
    pub on_demand: OnDemandSettings,
    /// `[settings.service]`.
    pub service: ServiceSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            data_dir: None,
            user_agent: format!(
                "zenodotus/{} (+https://github.com/jmpanozzoz/zenodotus)",
                env!("CARGO_PKG_VERSION")
            ),
            fetch: FetchSettings::default(),
            http: HttpSettings::default(),
            ranking: RankingSettings::default(),
            index: IndexSettings::default(),
            recipes_dir: None,
            offline: false,
            artifacts_dir: None,
            private_scopes: Vec::new(),
            host_overrides: BTreeMap::new(),
            on_demand: OnDemandSettings::default(),
            service: ServiceSettings::default(),
        }
    }
}

/// `[settings.on_demand]`: indexing a library the first time it is asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OnDemandSettings {
    /// Whether an unindexed library is indexed when asked for.
    pub enabled: bool,
    /// How long a query waits for the first answer, in seconds.
    pub wait_secs: u64,
}

impl Default for OnDemandSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            wait_secs: 45,
        }
    }
}

/// `[settings.service]`: the background service's budgets and load gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceSettings {
    /// Most bytes of indexed text before eviction.
    pub disk_cap_bytes: u64,
    /// Most bytes of cached artifacts.
    pub artifacts_cap_bytes: u64,
    /// Time between two cycles, in seconds.
    pub poll_secs: u64,
    /// Most downloads at once.
    pub max_downloads: u64,
    /// Most bytes downloaded per second.
    pub max_bandwidth_bytes_per_sec: u64,
    /// How many of each ecosystem's most used libraries to index ahead; 0 disables it.
    pub top_n: u64,
    /// Days between two checks of a runtime's sources.
    pub runtime_recheck_days: u64,
    /// Time between two load samples, in seconds.
    pub sample_secs: u64,
    /// The load window, in seconds.
    pub window_secs: u64,
    /// CPU idle share, in percent, above which a cycle may start.
    pub idle_start_pct: u8,
    /// CPU idle share, in percent, below which a running cycle pauses.
    pub idle_pause_pct: u8,
    /// The process niceness of the installed service.
    pub nice: i8,
}

impl Default for ServiceSettings {
    fn default() -> Self {
        Self {
            disk_cap_bytes: 2_147_483_648,
            artifacts_cap_bytes: 4_294_967_296,
            poll_secs: 60,
            max_downloads: 2,
            max_bandwidth_bytes_per_sec: 2_097_152,
            top_n: 0,
            runtime_recheck_days: 7,
            sample_secs: 10,
            window_secs: 60,
            idle_start_pct: 30,
            idle_pause_pct: 50,
            nice: 19,
        }
    }
}

/// `[settings.fetch]`: limits for every network request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FetchSettings {
    /// Whether plain `http://` URLs may be fetched.
    pub allow_http: bool,
    /// Whether private, loopback and link-local addresses may be reached.
    pub allow_private_networks: bool,
    /// Largest HTML or Markdown page, in bytes.
    pub page_max_bytes: u64,
    /// Largest document of a `url` source, such as `llms-full.txt`, in bytes.
    pub document_max_bytes: u64,
    /// Largest compressed tarball, in bytes.
    pub tarball_max_bytes: u64,
    /// Largest kept tarball entry, in bytes.
    pub tar_entry_max_bytes: u64,
    /// Largest total of kept, decompressed tarball entries, in bytes.
    pub tar_total_max_bytes: u64,
    /// Most tarball entries read.
    pub tar_max_entries: u64,
    /// Most links followed from one `llms.txt`.
    pub llms_txt_max_links: u64,
    /// TCP and TLS connection timeout.
    pub connect_timeout_secs: u64,
    /// Timeout of one page or document request.
    pub request_timeout_secs: u64,
    /// Timeout of one tarball download.
    pub tarball_timeout_secs: u64,
    /// Least time between two requests to the same host, in milliseconds; it can only lengthen the host table's.
    pub host_interval_ms: u64,
    /// Per-host intervals, in milliseconds; each can only lengthen the host table's.
    pub host_intervals_ms: BTreeMap<String, u64>,
    /// Retries after a timeout, a connection error or a 5xx answer.
    pub retries: u32,
}

impl Default for FetchSettings {
    fn default() -> Self {
        Self {
            allow_http: false,
            allow_private_networks: false,
            page_max_bytes: 10_485_760,
            document_max_bytes: 52_428_800,
            tarball_max_bytes: 536_870_912,
            tar_entry_max_bytes: 5_242_880,
            tar_total_max_bytes: 524_288_000,
            tar_max_entries: 50_000,
            llms_txt_max_links: 500,
            connect_timeout_secs: 10,
            request_timeout_secs: 60,
            tarball_timeout_secs: 300,
            host_interval_ms: DEFAULT_HOST_INTERVAL_MS,
            host_intervals_ms: BTreeMap::new(),
            retries: 3,
        }
    }
}

/// `[settings.http]`: the Streamable HTTP transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpSettings {
    /// The address `serve --http` binds when none is given.
    pub listen: SocketAddr,
    /// `Host` header values accepted besides the loopback defaults.
    pub allowed_hosts: Vec<String>,
    /// `Origin` header values accepted; a request with any other `Origin` is refused, every one when empty. A request
    /// without `Origin` passes.
    pub allowed_origins: Vec<String>,
    /// Largest request body, in bytes.
    pub max_body_bytes: u64,
    /// Most requests served at once.
    pub max_in_flight: u64,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            max_body_bytes: 65_536,
            max_in_flight: 64,
        }
    }
}

/// `[settings.ranking]`: query-time ranking; changes take effect without a rebuild.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RankingSettings {
    /// Boost of exact identifier matches on `symbols_exact`.
    pub symbol_boost: f32,
    /// Boost of `"quoted"` phrases, slop 0.
    pub quoted_phrase_boost: f32,
    /// Boost of sections with code in the requested `language`; 0 disables it.
    pub language_boost: f32,
    /// Fraction of non-stop words that must match; 0 means at least one.
    pub min_should_match: f32,
    /// Largest share of the sections a non-stop word may appear in and still decide which sections are candidates. A
    /// word in more sections only adds to the score, unless the word has synonyms, `min_should_match` asks for more
    /// than one word, or the other words are in fewer sections of the whole index than asked for; a search that comes
    /// up short runs again with every word. 1 disables it.
    pub common_word_share: f32,
    /// Most prose characters between a section and the code block that continues it; 0 disables the expansion.
    pub adjacent_max_prose_chars: u64,
    /// Words that also match their listed synonyms, at half weight.
    pub synonyms: BTreeMap<String, Vec<String>>,
}

impl Default for RankingSettings {
    fn default() -> Self {
        Self {
            symbol_boost: 0.5,
            quoted_phrase_boost: 2.0,
            language_boost: 1.0,
            min_should_match: 0.0,
            common_word_share: 0.2,
            adjacent_max_prose_chars: 0,
            synonyms: BTreeMap::new(),
        }
    }
}

/// `[settings.index]`: index-time weights; a change requires `sync --rebuild`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexSettings {
    /// Repetitions of the section's own heading, or of the document title for a section before the first heading.
    pub title_weight: u32,
    /// Repetitions of the document title and the ancestor headings.
    pub heading_weight: u32,
    /// Repetitions of extracted symbols.
    pub symbol_weight: u32,
    /// Repetitions of prose.
    pub body_weight: u32,
    /// Repetitions of code.
    pub code_weight: u32,
    /// Repetitions of the document path.
    pub path_weight: u32,
}

impl Default for IndexSettings {
    fn default() -> Self {
        Self {
            title_weight: 3,
            heading_weight: 2,
            symbol_weight: 4,
            body_weight: 1,
            code_weight: 1,
            path_weight: 1,
        }
    }
}

impl IndexSettings {
    /// The weights by name, in a fixed order.
    fn named(&self) -> [(&'static str, u32); 6] {
        [
            ("index.title_weight", self.title_weight),
            ("index.heading_weight", self.heading_weight),
            ("index.symbol_weight", self.symbol_weight),
            ("index.body_weight", self.body_weight),
            ("index.code_weight", self.code_weight),
            ("index.path_weight", self.path_weight),
        ]
    }

    /// The lowercase hex SHA-256 of the weights, stored in every commit so that an index built with other weights
    /// is detected at open.
    #[must_use]
    pub fn hash(&self) -> String {
        let mut hasher = Sha256::new();
        for (name, weight) in self.named() {
            hasher.update(format!("{name}={weight}\n"));
        }
        to_hex(&hasher.finalize())
    }
}

impl Settings {
    /// Checks every value against its range.
    ///
    /// # Errors
    /// [`CoreError::InvalidSettings`] naming the first value out of range.
    pub fn validate(&self) -> Result<(), CoreError> {
        let invalid = |field, reason| Err(CoreError::InvalidSettings { field, reason });
        if self.user_agent.trim().is_empty() {
            return invalid("user_agent", "must not be empty");
        }
        let fetch = &self.fetch;
        for (field, value) in [
            ("fetch.page_max_bytes", fetch.page_max_bytes),
            ("fetch.document_max_bytes", fetch.document_max_bytes),
            ("fetch.tarball_max_bytes", fetch.tarball_max_bytes),
            ("fetch.tar_entry_max_bytes", fetch.tar_entry_max_bytes),
            ("fetch.tar_total_max_bytes", fetch.tar_total_max_bytes),
            ("fetch.tar_max_entries", fetch.tar_max_entries),
            ("fetch.connect_timeout_secs", fetch.connect_timeout_secs),
            ("fetch.request_timeout_secs", fetch.request_timeout_secs),
            ("fetch.tarball_timeout_secs", fetch.tarball_timeout_secs),
            ("http.max_body_bytes", self.http.max_body_bytes),
            ("http.max_in_flight", self.http.max_in_flight),
            ("service.poll_secs", self.service.poll_secs),
            ("service.max_downloads", self.service.max_downloads),
            ("service.runtime_recheck_days", self.service.runtime_recheck_days),
            ("service.sample_secs", self.service.sample_secs),
            ("service.window_secs", self.service.window_secs),
        ] {
            if value == 0 {
                return invalid(field, "must be at least 1");
            }
        }
        if fetch.retries > MAX_RETRIES {
            return invalid("fetch.retries", "must be at most 10");
        }
        self.ranking.validate()?;
        for (field, weight) in self.index.named() {
            if weight > MAX_INDEX_WEIGHT {
                return invalid(field, "must be at most 16");
            }
        }
        if self.index.body_weight == 0 {
            return invalid("index.body_weight", "must be at least 1");
        }
        self.validate_v2()
    }

    /// Checks the settings added in v2.
    fn validate_v2(&self) -> Result<(), CoreError> {
        let invalid = |field, reason| Err(CoreError::InvalidSettings { field, reason });
        for (field, pct) in [
            ("service.idle_start_pct", self.service.idle_start_pct),
            ("service.idle_pause_pct", self.service.idle_pause_pct),
        ] {
            if pct > 100 {
                return invalid(field, "must be at most 100");
            }
        }
        if !(-20..=19).contains(&self.service.nice) {
            return invalid("service.nice", "must be between -20 and 19");
        }
        let web = |url: &Url| matches!(url.scheme(), "http" | "https");
        if !self
            .host_overrides
            .iter()
            .all(|(host, url)| is_host_name(host) && web(url))
        {
            return invalid(
                "host_overrides",
                "keys must be host names and values `http` or `https` URLs",
            );
        }
        let ecosystem_prefix = |scope: &String| {
            scope
                .split_once(':')
                .is_some_and(|(prefix, _)| Ecosystem::from_prefix(prefix).is_some())
        };
        if !self.private_scopes.iter().all(ecosystem_prefix) {
            return invalid(
                "private_scopes",
                "each entry must start with an ecosystem prefix such as `npm:`",
            );
        }
        Ok(())
    }

    /// Warnings about values that have no effect: intervals below the host table, which only lengthens.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        let configured = self.fetch.host_interval_ms;
        if configured < DEFAULT_HOST_INTERVAL_MS {
            warnings.push(format!(
                "settings.fetch.host_interval_ms = {configured} is below the {DEFAULT_HOST_INTERVAL_MS} ms every host \
                 gets; {DEFAULT_HOST_INTERVAL_MS} ms applies"
            ));
        }
        for (host, &configured) in &self.fetch.host_intervals_ms {
            let table = table_interval_ms(host);
            if configured < table && !is_loopback(host) {
                warnings.push(format!(
                    "settings.fetch.host_intervals_ms.{host:?} = {configured} is below the {table} ms of the host \
                     table; {table} ms applies"
                ));
            }
        }
        warnings
    }

    /// The artifact cache: `artifacts_dir` when set, resolved like [`Settings::data_dir`], else
    /// `<cache dir>/zenodotus/artifacts` (see [`cache_dir`]).
    ///
    /// # Errors
    /// [`CoreError::NoLocation`] when the path needs a variable that is unset.
    pub fn artifacts_dir(&self, config_path: &Path, env: Env<'_>) -> Result<PathBuf, CoreError> {
        match &self.artifacts_dir {
            Some(dir) => resolve_dir(dir, config_path, env, "artifacts directory"),
            None => Ok(cache_dir(env)?.join("zenodotus/artifacts")),
        }
    }

    /// The data directory: `data_dir` when set (`~/` expands to `$HOME`; a relative path is taken from the config
    /// file's directory), then `$XDG_DATA_HOME/zenodotus`, then `~/.local/share/zenodotus`.
    ///
    /// # Errors
    /// [`CoreError::NoLocation`] when the path needs `$HOME` and it is unset.
    pub fn data_dir(&self, config_path: &Path, env: Env<'_>) -> Result<PathBuf, CoreError> {
        let home = || {
            var(env, "HOME")
                .map(PathBuf::from)
                .ok_or(CoreError::NoLocation { what: "data directory" })
        };
        let Some(dir) = &self.data_dir else {
            if let Some(xdg) = var(env, "XDG_DATA_HOME") {
                return Ok(PathBuf::from(xdg).join("zenodotus"));
            }
            return Ok(home()?.join(".local/share/zenodotus"));
        };
        resolve_dir(dir, config_path, env, "data directory")
    }
}

/// A configured directory: `~/` expands to `$HOME`, and a relative path is taken from the config file's directory.
fn resolve_dir(dir: &Path, config_path: &Path, env: Env<'_>, what: &'static str) -> Result<PathBuf, CoreError> {
    if let Ok(rest) = dir.strip_prefix("~") {
        let home = var(env, "HOME").ok_or(CoreError::NoLocation { what })?;
        return Ok(PathBuf::from(home).join(rest));
    }
    Ok(config_path.parent().unwrap_or(Path::new("")).join(dir))
}

/// The interval the host table (design §11.2) gives `host`, in milliseconds.
#[must_use]
pub fn table_interval_ms(host: &str) -> u64 {
    HOST_TABLE_MS
        .iter()
        .find(|(listed, _)| *listed == host)
        .map_or(DEFAULT_HOST_INTERVAL_MS, |(_, interval)| *interval)
}

/// Whether `host` is a loopback IP address, which takes the configured interval alone.
fn is_loopback(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// An operating system, for the per-user directories that differ between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// Linux and other Unix systems.
    Linux,
    /// macOS.
    Macos,
    /// Windows.
    Windows,
}

impl Os {
    /// The operating system this binary was built for.
    #[cfg(windows)]
    pub const CURRENT: Self = Self::Windows;
    /// The operating system this binary was built for.
    #[cfg(target_os = "macos")]
    pub const CURRENT: Self = Self::Macos;
    /// The operating system this binary was built for.
    #[cfg(not(any(windows, target_os = "macos")))]
    pub const CURRENT: Self = Self::Linux;
}

/// The per-user cache directory (design §16), fixed whatever the config: `~/.cache` on Linux, ignoring
/// `XDG_CACHE_HOME`; `~/Library/Caches` on macOS; `%LOCALAPPDATA%` on Windows. Only the test-only
/// `ZENODOTUS_CACHE_DIR` overrides it.
///
/// # Errors
/// [`CoreError::NoLocation`] when the variable it comes from is unset.
pub fn cache_dir(env: Env<'_>) -> Result<PathBuf, CoreError> {
    cache_dir_on(Os::CURRENT, env)
}

/// [`cache_dir`] as it is on `os`.
///
/// # Errors
/// As [`cache_dir`].
pub fn cache_dir_on(os: Os, env: Env<'_>) -> Result<PathBuf, CoreError> {
    let missing = CoreError::NoLocation {
        what: "cache directory",
    };
    if let Some(dir) = var(env, "ZENODOTUS_CACHE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = || var(env, "HOME").map(PathBuf::from);
    match os {
        Os::Linux => home().map(|home| home.join(".cache")),
        Os::Macos => home().map(|home| home.join("Library/Caches")),
        Os::Windows => var(env, "LOCALAPPDATA").map(PathBuf::from),
    }
    .ok_or(missing)
}

impl RankingSettings {
    /// Checks boosts, the match fraction and the synonym table.
    fn validate(&self) -> Result<(), CoreError> {
        let invalid = |field, reason| Err(CoreError::InvalidSettings { field, reason });
        for (field, boost) in [
            ("ranking.symbol_boost", self.symbol_boost),
            ("ranking.quoted_phrase_boost", self.quoted_phrase_boost),
            ("ranking.language_boost", self.language_boost),
        ] {
            if !boost.is_finite() || boost < 0.0 {
                return invalid(field, "must be a finite number of at least 0");
            }
        }
        if !(0.0..=1.0).contains(&self.min_should_match) {
            return invalid("ranking.min_should_match", "must be between 0 and 1");
        }
        if !(self.common_word_share > 0.0 && self.common_word_share <= 1.0) {
            return invalid("ranking.common_word_share", "must be above 0 and at most 1");
        }
        let blank = |word: &String| word.trim().is_empty();
        if self
            .synonyms
            .iter()
            .any(|(word, synonyms)| blank(word) || synonyms.is_empty() || synonyms.iter().any(blank))
        {
            return invalid("ranking.synonyms", "words and synonym lists must not be empty");
        }
        // Queries are split into lowercase words before synonyms are looked up, so any other key never matches.
        let one_word = |word: &String| {
            PROTECTED_WORDS.contains(&word.as_str()) || word.chars().all(|c| c.is_alphanumeric() && !c.is_uppercase())
        };
        if !self.synonyms.keys().all(one_word) {
            return invalid(
                "ranking.synonyms",
                "each word must be one lowercase word of letters and digits, or a word queries keep whole such as \
                 `node.js`, as queries are split",
            );
        }
        Ok(())
    }
}

impl Config {
    /// Reads, parses and validates the config file at `path`.
    ///
    /// # Errors
    /// [`CoreError::ConfigIo`] when the file cannot be read, otherwise as [`Config::parse`].
    pub fn load(path: &Path) -> Result<Self, CoreError> {
        let text = std::fs::read_to_string(path).map_err(|source| CoreError::ConfigIo {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(path, &text)
    }

    /// As [`Config::load`], except that a missing file gives the default configuration.
    ///
    /// # Errors
    /// As [`Config::load`], for any problem other than a missing file.
    pub fn load_or_default(path: &Path) -> Result<Self, CoreError> {
        match Self::load(path) {
            Err(CoreError::ConfigIo { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            other => other,
        }
    }

    /// Parses and validates `text`; `path` only labels errors.
    ///
    /// # Errors
    /// [`CoreError::ConfigParse`] for invalid TOML, unknown keys or invalid ids; otherwise as [`Config::validate`].
    pub fn parse(path: &Path, text: &str) -> Result<Self, CoreError> {
        let config: Self = toml::from_str(text).map_err(|error| CoreError::ConfigParse {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Checks the settings and every library.
    ///
    /// # Errors
    /// The first broken rule: [`CoreError::InvalidSettings`], [`CoreError::DuplicateLibrary`],
    /// [`CoreError::NoVersions`], [`CoreError::DuplicateVersion`], [`CoreError::MultipleDefaults`] or
    /// [`CoreError::InvalidSource`].
    pub fn validate(&self) -> Result<(), CoreError> {
        self.settings.validate()?;
        let mut seen = BTreeSet::new();
        for library in &self.libraries {
            if !seen.insert(&library.id) {
                return Err(CoreError::DuplicateLibrary {
                    library: library.id.to_string(),
                });
            }
            validate_library(library)?;
        }
        Ok(())
    }

    /// The configured scope a canonical ID names at `version`: the library that declares `canonical` and lists that
    /// version (design §6.4). `None` sends the lookup to the catalog.
    #[must_use]
    pub fn configured_scope(&self, canonical: &CanonicalId, version: &Version) -> Option<Scope> {
        self.libraries
            .iter()
            .filter(|library| library.canonical.as_ref() == Some(canonical))
            .find(|library| library.versions.iter().any(|spec| &spec.version == version))
            .map(|library| Scope::new(library.id.clone(), version.clone()))
    }

    /// The library with id `id`.
    ///
    /// # Errors
    /// [`CoreError::UnknownLibrary`] when no library has that id.
    pub fn library(&self, id: &LibraryId) -> Result<&LibrarySpec, CoreError> {
        self.libraries
            .iter()
            .find(|library| &library.id == id)
            .ok_or_else(|| CoreError::UnknownLibrary {
                library: id.to_string(),
            })
    }
}

/// Checks the versions of one library.
fn validate_library(library: &LibrarySpec) -> Result<(), CoreError> {
    let name = library.id.to_string();
    if library.versions.is_empty() {
        return Err(CoreError::NoVersions { library: name });
    }
    let mut seen = BTreeSet::new();
    for spec in &library.versions {
        if !seen.insert(&spec.version) {
            return Err(CoreError::DuplicateVersion {
                library: name,
                version: spec.version.to_string(),
            });
        }
        let invalid = |reason| CoreError::InvalidSource {
            library: name.clone(),
            version: spec.version.to_string(),
            reason,
        };
        spec.source.check().map_err(invalid)?;
        if !spec.extra_hosts.iter().all(|host| is_host_name(host)) {
            return Err(invalid("`extra_hosts` entries must be host names"));
        }
    }
    if library.versions.iter().filter(|spec| spec.default).count() > 1 {
        return Err(CoreError::MultipleDefaults { library: name });
    }
    Ok(())
}

/// Whether `host` is a bare DNS name: dot-separated labels of ASCII letters, digits and `-`.
fn is_host_name(host: &str) -> bool {
    !host.is_empty()
        && host
            .split('.')
            .all(|label| !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// The config file: `cli_flag`, then `$ZENODOTUS_CONFIG`, then `$XDG_CONFIG_HOME/zenodotus/zenodotus.toml`, then
/// `~/.config/zenodotus/zenodotus.toml`. Empty variables count as unset.
///
/// # Errors
/// [`CoreError::NoLocation`] when none of them is available.
pub fn locate_config(cli_flag: Option<&Path>, env: Env<'_>) -> Result<PathBuf, CoreError> {
    if let Some(path) = cli_flag {
        return Ok(path.to_owned());
    }
    if let Some(path) = var(env, "ZENODOTUS_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    if let Some(xdg) = var(env, "XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("zenodotus/zenodotus.toml"));
    }
    var(env, "HOME")
        .map(|home| PathBuf::from(home).join(".config/zenodotus/zenodotus.toml"))
        .ok_or(CoreError::NoLocation { what: "config file" })
}

/// A non-empty environment variable.
fn var(env: Env<'_>, name: &str) -> Option<String> {
    env(name).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{CanonicalId, Scope};

    const EXAMPLE: &str = r#"
# Comments are allowed anywhere.
[settings]
data_dir = "data"

[settings.ranking]
symbol_boost = 2.5
synonyms = { env = ["environment"] }

[[library]]
id = "tokio"
name = "Tokio"
aliases = ["tokio-rs"]
homepage = "https://tokio.rs/"
license = "MIT"

[[library.version]]
version = "1.40"
default = true
source = { kind = "github", repo = "tokio-rs/website", reference = "abc123", paths = ["content"] }
extra_hosts = ["objects.githubusercontent.com"]

[[library.version]]
version = "local"
source = { kind = "local", path = "/srv/tokio" }
"#;

    fn path() -> &'static Path {
        Path::new("/etc/zenodotus.toml")
    }

    fn parse(text: &str) -> Result<Config, CoreError> {
        Config::parse(path(), text)
    }

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    fn library(id: &str, versions: &str) -> String {
        format!("[[library]]\nid = \"{id}\"\n{versions}")
    }

    fn version(label: &str, default: bool) -> String {
        format!(
            "[[library.version]]\nversion = \"{label}\"\ndefault = {default}\nsource = {{ kind = \"local\", path = \"d\" }}\n"
        )
    }

    #[test]
    fn the_shipped_example_is_valid() {
        let config = parse(include_str!("../../../examples/zenodotus.toml")).unwrap();
        let ids: Vec<&str> = config.libraries.iter().map(|library| library.id.as_str()).collect();
        assert_eq!(ids, ["tokio", "pydantic", "my-project", "vite", "fastapi", "mdn-fetch"]);
        let kinds: Vec<&str> = config
            .libraries
            .iter()
            .flat_map(|library| &library.versions)
            .map(|version| match version.source {
                SourceSpec::Local { .. } => "local",
                SourceSpec::GitHub { .. } => "github",
                SourceSpec::Url { .. } => "url",
                SourceSpec::LlmsTxt { .. } => "llms_txt",
                SourceSpec::Website { .. } => "website",
            })
            .collect();
        assert_eq!(
            kinds,
            ["github", "github", "github", "local", "llms_txt", "url", "website"]
        );
    }

    #[test]
    fn an_empty_file_gives_every_default() {
        let config = parse("").unwrap();
        assert_eq!(config, Config::default());
        let settings = config.settings;
        assert_eq!(settings.fetch.page_max_bytes, 10_485_760);
        assert_eq!(settings.fetch.tarball_max_bytes, 536_870_912);
        assert_eq!(settings.fetch.retries, 3);
        assert_eq!(settings.http.listen.to_string(), "127.0.0.1:8080");
        assert_eq!(settings.http.max_in_flight, 64);
        assert!((settings.ranking.quoted_phrase_boost - 2.0).abs() < f32::EPSILON);
        assert!(
            (settings.ranking.symbol_boost - 0.5).abs() < f32::EPSILON,
            "tuned in iteration 7"
        );
        assert_eq!(settings.index.symbol_weight, 4, "tuned in iteration 12");
        assert_eq!(settings.index.title_weight, 3, "tuned in iteration 19");
        assert!(settings.user_agent.starts_with("zenodotus/0."));
        assert_eq!(
            settings.user_agent,
            format!(
                "zenodotus/{} (+https://github.com/jmpanozzoz/zenodotus)",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(settings.fetch.host_interval_ms, 1_000);
        assert!(settings.fetch.host_intervals_ms.is_empty());
        assert_eq!(settings.recipes_dir, None);
        assert!(!settings.offline);
        assert_eq!(settings.artifacts_dir, None);
        assert_eq!(settings.private_scopes, Vec::<String>::new());
        assert!(settings.host_overrides.is_empty());
        assert_eq!(
            settings.on_demand,
            OnDemandSettings {
                enabled: true,
                wait_secs: 45
            }
        );
        assert_eq!(
            settings.service,
            ServiceSettings {
                disk_cap_bytes: 2_147_483_648,
                artifacts_cap_bytes: 4_294_967_296,
                poll_secs: 60,
                max_downloads: 2,
                max_bandwidth_bytes_per_sec: 2_097_152,
                top_n: 0,
                runtime_recheck_days: 7,
                sample_secs: 10,
                window_secs: 60,
                idle_start_pct: 30,
                idle_pause_pct: 50,
                nice: 19,
            }
        );
        assert_eq!(DEFAULT_MAX_CHARS, 12_000);
        assert_eq!(settings.warnings(), Vec::<String>::new());
    }

    const V2_SETTINGS: &str = r#"
[settings]
recipes_dir = "~/src/zenodotus/recipes"
offline = true
artifacts_dir = "/home/u/.cache/zenodotus/artifacts"
private_scopes = ["npm:@acme/", "maven:com.acme."]

[settings.fetch]
host_interval_ms = 1500
host_intervals_ms = { "docs.example.org" = 2000 }

[settings.host_overrides]
"registry.npmjs.org" = "http://127.0.0.1:4873"

[settings.on_demand]
enabled = false
wait_secs = 20

[settings.service]
poll_secs = 30
top_n = 100
nice = 10

[[library]]
id = "react"
canonical = "npm:react"

[[library.version]]
version = "19.1.0"
source = { kind = "local", path = "docs" }
"#;

    #[test]
    fn parses_the_v2_keys() {
        let config = parse(V2_SETTINGS).unwrap();
        let settings = &config.settings;
        assert_eq!(settings.recipes_dir, Some(PathBuf::from("~/src/zenodotus/recipes")));
        assert!(settings.offline);
        assert_eq!(
            settings.artifacts_dir,
            Some(PathBuf::from("/home/u/.cache/zenodotus/artifacts"))
        );
        assert_eq!(settings.private_scopes, ["npm:@acme/", "maven:com.acme."]);
        assert_eq!(settings.fetch.host_interval_ms, 1_500);
        assert_eq!(settings.fetch.host_intervals_ms["docs.example.org"], 2_000);
        assert_eq!(
            settings.host_overrides["registry.npmjs.org"].as_str(),
            "http://127.0.0.1:4873/"
        );
        assert!(!settings.on_demand.enabled);
        assert_eq!(settings.on_demand.wait_secs, 20);
        assert_eq!(settings.service.poll_secs, 30);
        assert_eq!(settings.service.top_n, 100);
        assert_eq!(settings.service.nice, 10);
        assert_eq!(settings.service.max_downloads, 2);
        assert_eq!(
            config.libraries[0].canonical.as_ref().map(CanonicalId::as_str),
            Some("npm:react")
        );
        let text = toml::to_string(&config).unwrap();
        assert_eq!(parse(&text).unwrap(), config);
        for text in [
            "[settings.on_demand]\nwait = 1",
            "[settings.service]\npoll = 1",
            "[[library]]\nid = \"a\"\ncanonical = \"cobol:a\"\n",
        ] {
            assert!(matches!(parse(text), Err(CoreError::ConfigParse { .. })), "{text}");
        }
    }

    #[test]
    fn a_configured_canonical_id_resolves_to_the_slug_scope_at_its_versions_only() {
        let config = parse(V2_SETTINGS).unwrap();
        let react = CanonicalId::parse("npm:react").unwrap();
        let at = |version| config.configured_scope(&react, &Version::parse(version).unwrap());
        assert_eq!(
            at("19.1.0"),
            Some(Scope::new(
                LibraryId::parse("react").unwrap(),
                Version::parse("19.1.0").unwrap()
            ))
        );
        assert_eq!(at("18.3.1"), None);
        let vue = CanonicalId::parse("npm:vue").unwrap();
        assert_eq!(config.configured_scope(&vue, &Version::parse("19.1.0").unwrap()), None);

        assert!(matches!(
            parse("[[library]]\nid = \"react\"\ncanonical = \"npm:react\"\n"),
            Err(CoreError::NoVersions { library }) if library == "react"
        ));
    }

    #[test]
    fn new_settings_are_checked() {
        for (key, value) in [("idle_start_pct", "101"), ("idle_pause_pct", "101")] {
            assert_eq!(
                settings_error(&format!("[settings.service]\n{key} = {value}")),
                message(&format!("service.{key}"), "must be at most 100")
            );
        }
        for key in [
            "poll_secs",
            "max_downloads",
            "sample_secs",
            "window_secs",
            "runtime_recheck_days",
        ] {
            assert_eq!(
                settings_error(&format!("[settings.service]\n{key} = 0")),
                message(&format!("service.{key}"), "must be at least 1")
            );
        }
        for nice in ["-21", "20"] {
            assert_eq!(
                settings_error(&format!("[settings.service]\nnice = {nice}")),
                message("service.nice", "must be between -20 and 19")
            );
        }
        assert!(parse("[settings.service]\nnice = -20\nidle_start_pct = 100").is_ok());
        for overrides in [
            r#""bad host" = "http://127.0.0.1:1""#,
            r#""a.org" = "ftp://127.0.0.1/""#,
        ] {
            assert_eq!(
                settings_error(&format!("[settings.host_overrides]\n{overrides}")),
                message(
                    "host_overrides",
                    "keys must be host names and values `http` or `https` URLs"
                )
            );
        }
        for scope in ["", "react", "cobol:x"] {
            assert_eq!(
                settings_error(&format!("[settings]\nprivate_scopes = [\"{scope}\"]")),
                message(
                    "private_scopes",
                    "each entry must start with an ecosystem prefix such as `npm:`"
                )
            );
        }
        assert!(parse("[settings]\nprivate_scopes = [\"pypi:\"]").is_ok());
    }

    #[test]
    fn intervals_below_the_host_table_warn() {
        let config = parse(
            "[settings.fetch]\nhost_interval_ms = 500\n\
             host_intervals_ms = { \"crates.io\" = 500, \"docs.example.org\" = 2000, \"rubygems.org\" = 100, \
             \"pkg.go.dev\" = 50, \"127.0.0.1\" = 100, \"[::1]\" = 100 }",
        )
        .unwrap();
        assert_eq!(
            config.settings.warnings(),
            [
                "settings.fetch.host_interval_ms = 500 is below the 1000 ms every host gets; 1000 ms applies",
                "settings.fetch.host_intervals_ms.\"crates.io\" = 500 is below the 1000 ms of the host table; 1000 ms \
                 applies",
                "settings.fetch.host_intervals_ms.\"pkg.go.dev\" = 50 is below the 100 ms of the host table; 100 ms \
                 applies",
            ]
        );
        assert_eq!(table_interval_ms("crates.io"), 1_000);
        assert_eq!(table_interval_ms("rubygems.org"), 100);
        assert_eq!(table_interval_ms("pkg.go.dev"), 100);
        assert_eq!(table_interval_ms("docs.example.org"), 1_000);
    }

    #[test]
    fn the_cache_directory_is_fixed_per_user() {
        let home = env(&[
            ("HOME", "/home/u"),
            ("XDG_CACHE_HOME", "/xdg"),
            ("LOCALAPPDATA", "C:\\L"),
        ]);
        assert_eq!(cache_dir_on(Os::Linux, &home).unwrap(), Path::new("/home/u/.cache"));
        assert_eq!(
            cache_dir_on(Os::Macos, &home).unwrap(),
            Path::new("/home/u/Library/Caches")
        );
        assert_eq!(cache_dir_on(Os::Windows, &home).unwrap(), Path::new("C:\\L"));
        assert_eq!(cache_dir(&home).unwrap(), Path::new("/home/u/.cache"));
        let test_only = env(&[("ZENODOTUS_CACHE_DIR", "/tmp/c"), ("HOME", "/home/u")]);
        for os in [Os::Linux, Os::Macos, Os::Windows] {
            assert_eq!(cache_dir_on(os, &test_only).unwrap(), Path::new("/tmp/c"));
            assert!(matches!(
                cache_dir_on(os, &env(&[("ZENODOTUS_CACHE_DIR", "")])),
                Err(CoreError::NoLocation {
                    what: "cache directory"
                })
            ));
        }
    }

    #[test]
    fn artifacts_dir_resolves_like_data_dir() {
        let config_path = Path::new("/etc/z/zenodotus.toml");
        let with = |dir: Option<&str>| Settings {
            artifacts_dir: dir.map(PathBuf::from),
            ..Settings::default()
        };
        let home = env(&[("HOME", "/home/u")]);
        assert_eq!(
            with(Some("/abs")).artifacts_dir(config_path, &home).unwrap(),
            Path::new("/abs")
        );
        assert_eq!(
            with(Some("rel")).artifacts_dir(config_path, &home).unwrap(),
            Path::new("/etc/z/rel")
        );
        assert_eq!(
            with(Some("~/a")).artifacts_dir(config_path, &home).unwrap(),
            Path::new("/home/u/a")
        );
        assert_eq!(
            with(None).artifacts_dir(config_path, &home).unwrap(),
            Path::new("/home/u/.cache/zenodotus/artifacts")
        );
        assert_eq!(
            with(None)
                .artifacts_dir(config_path, &env(&[("ZENODOTUS_CACHE_DIR", "/tmp/c")]))
                .unwrap(),
            Path::new("/tmp/c/zenodotus/artifacts")
        );
        for dir in [None, Some("~/a")] {
            assert!(matches!(
                with(dir).artifacts_dir(config_path, &env(&[])),
                Err(CoreError::NoLocation { .. })
            ));
        }
    }

    #[test]
    fn parses_the_example_config() {
        let config = parse(EXAMPLE).unwrap();
        assert_eq!(config.settings.data_dir, Some(PathBuf::from("data")));
        assert!((config.settings.ranking.symbol_boost - 2.5).abs() < f32::EPSILON);
        assert_eq!(config.settings.ranking.synonyms["env"], ["environment"]);
        let tokio = &config.libraries[0];
        assert_eq!(tokio.display_name(), "Tokio");
        assert_eq!(tokio.aliases, ["tokio-rs"]);
        assert_eq!(tokio.homepage.as_ref().map(Url::as_str), Some("https://tokio.rs/"));
        assert_eq!(tokio.versions.len(), 2);
        assert!(tokio.versions[0].default);
        assert!(!tokio.versions[1].default);
        assert_eq!(tokio.versions[0].extra_hosts, ["objects.githubusercontent.com"]);
    }

    #[test]
    fn round_trips_through_toml() {
        let config = parse(EXAMPLE).unwrap();
        let text = toml::to_string(&config).unwrap();
        assert_eq!(parse(&text).unwrap(), config);
    }

    #[test]
    fn the_display_name_falls_back_to_the_id() {
        let config = parse(&library("vite", &version("6", false))).unwrap();
        assert_eq!(config.libraries[0].display_name(), "vite");
    }

    #[test]
    fn the_eval_corpus_config_is_valid() {
        let text = include_str!("../../../eval/corpus.toml");
        let config = parse(text).unwrap();
        assert_eq!(config.libraries.len(), 5);
    }

    #[test]
    fn invalid_toml_unknown_keys_and_bad_ids_are_parse_errors() {
        for text in [
            "[settings",
            "[settings]\ncolour = 1",
            "[settings.fetch]\nretry = 1",
            "[settings.ranking]\nphrase_boost = 1.0",
            "[[library]]\nid = \"Bad Id\"\n",
            "[[library]]\nid = \"a\"\nhome = \"x\"\n",
        ] {
            assert!(
                matches!(parse(text), Err(CoreError::ConfigParse { ref path, .. }) if path == super::tests::path()),
                "{text}"
            );
        }
    }

    #[test]
    fn a_library_must_have_versions() {
        assert!(matches!(
            parse(&library("a", "")),
            Err(CoreError::NoVersions { library }) if library == "a"
        ));
    }

    #[test]
    fn library_ids_are_unique() {
        let text = library("a", &version("1", false)) + &library("a", &version("2", false));
        assert!(matches!(parse(&text), Err(CoreError::DuplicateLibrary { library }) if library == "a"));
    }

    #[test]
    fn versions_are_unique_within_a_library() {
        let text = library("a", &(version("1", false) + &version("1", false)));
        assert!(matches!(
            parse(&text),
            Err(CoreError::DuplicateVersion { library, version }) if library == "a" && version == "1"
        ));
        let text = library("a", &version("1", false)) + &library("b", &version("1", false));
        assert!(parse(&text).is_ok());
    }

    #[test]
    fn at_most_one_default_version() {
        let text = library("a", &(version("1", true) + &version("2", true)));
        assert!(matches!(parse(&text), Err(CoreError::MultipleDefaults { library }) if library == "a"));
    }

    #[test]
    fn sources_are_checked_with_their_reason() {
        let text = "[[library]]\nid = \"a\"\n[[library.version]]\nversion = \"1\"\nsource = { kind = \"github\", repo = \"x\" }\n";
        assert!(matches!(
            parse(text),
            Err(CoreError::InvalidSource { library, version, reason })
                if library == "a" && version == "1" && reason == "`repo` must be `owner/name`"
        ));
    }

    #[test]
    fn extra_hosts_must_be_host_names() {
        for host in ["", "a..b", "https://x.dev", "x.dev/path", "x.dev:443", "a_b.dev"] {
            let text = format!(
                "[[library]]\nid = \"a\"\n[[library.version]]\nversion = \"1\"\nsource = {{ kind = \"local\", path = \"d\" }}\nextra_hosts = [\"{host}\"]\n"
            );
            assert!(
                matches!(
                    parse(&text),
                    Err(CoreError::InvalidSource {
                        reason: "`extra_hosts` entries must be host names",
                        ..
                    })
                ),
                "{host}"
            );
        }
    }

    /// The message of the `InvalidSettings` error that `toml_text` must produce.
    fn settings_error(toml_text: &str) -> String {
        let error = parse(toml_text).unwrap_err();
        assert!(
            matches!(error, CoreError::InvalidSettings { .. }),
            "{toml_text}: {error:?}"
        );
        error.to_string()
    }

    fn message(field: &str, reason: &str) -> String {
        format!("invalid setting `{field}`: {reason}")
    }

    #[test]
    fn every_size_timeout_and_limit_must_be_positive() {
        for (table, key) in [
            ("fetch", "page_max_bytes"),
            ("fetch", "document_max_bytes"),
            ("fetch", "tarball_max_bytes"),
            ("fetch", "tar_entry_max_bytes"),
            ("fetch", "tar_total_max_bytes"),
            ("fetch", "tar_max_entries"),
            ("fetch", "connect_timeout_secs"),
            ("fetch", "request_timeout_secs"),
            ("fetch", "tarball_timeout_secs"),
            ("http", "max_body_bytes"),
            ("http", "max_in_flight"),
        ] {
            assert_eq!(
                settings_error(&format!("[settings.{table}]\n{key} = 0")),
                message(&format!("{table}.{key}"), "must be at least 1")
            );
        }
    }

    #[test]
    fn simple_setting_rules() {
        assert_eq!(
            settings_error("[settings]\nuser_agent = \"  \""),
            message("user_agent", "must not be empty")
        );
        assert_eq!(
            settings_error("[settings.fetch]\nretries = 11"),
            message("fetch.retries", "must be at most 10")
        );
        assert!(parse("[settings.fetch]\nretries = 10").is_ok());
        assert_eq!(
            settings_error("[settings.ranking]\nmin_should_match = 1.5"),
            message("ranking.min_should_match", "must be between 0 and 1")
        );
        assert_eq!(
            settings_error("[settings.ranking]\nmin_should_match = -0.1"),
            message("ranking.min_should_match", "must be between 0 and 1")
        );
        assert!(parse("[settings.ranking]\nmin_should_match = 1.0").is_ok());
        for value in ["0.0", "1.5", "nan"] {
            assert_eq!(
                settings_error(&format!("[settings.ranking]\ncommon_word_share = {value}")),
                message("ranking.common_word_share", "must be above 0 and at most 1")
            );
        }
        assert!(parse("[settings.ranking]\ncommon_word_share = 1.0").is_ok());
        assert_eq!(RankingSettings::default().common_word_share, 0.2);
        assert_eq!(
            settings_error("[settings.index]\nbody_weight = 0"),
            message("index.body_weight", "must be at least 1")
        );
    }

    #[test]
    fn boosts_must_be_finite_and_not_negative() {
        for key in ["symbol_boost", "quoted_phrase_boost", "language_boost"] {
            for value in ["-1.0", "nan", "inf"] {
                assert_eq!(
                    settings_error(&format!("[settings.ranking]\n{key} = {value}")),
                    message(&format!("ranking.{key}"), "must be a finite number of at least 0")
                );
            }
            assert!(parse(&format!("[settings.ranking]\n{key} = 0.0")).is_ok());
        }
    }

    #[test]
    fn index_weights_have_a_ceiling() {
        for key in [
            "title_weight",
            "heading_weight",
            "symbol_weight",
            "body_weight",
            "code_weight",
            "path_weight",
        ] {
            assert_eq!(
                settings_error(&format!("[settings.index]\n{key} = 17")),
                message(&format!("index.{key}"), "must be at most 16")
            );
            assert!(parse(&format!("[settings.index]\n{key} = 16")).is_ok());
        }
    }

    #[test]
    fn synonym_tables_must_be_complete() {
        for table in ["{ \" \" = [\"x\"] }", "{ env = [] }", "{ env = [\"\"] }"] {
            assert_eq!(
                settings_error(&format!("[settings.ranking]\nsynonyms = {table}")),
                message("ranking.synonyms", "words and synonym lists must not be empty")
            );
        }
    }

    #[test]
    fn synonym_keys_are_single_lowercase_words() {
        for key in ["useState", "front-end", "two words", "snake_case", "Node.js"] {
            assert_eq!(
                settings_error(&format!("[settings.ranking]\nsynonyms = {{ \"{key}\" = [\"x\"] }}")),
                message(
                    "ranking.synonyms",
                    "each word must be one lowercase word of letters and digits, or a word queries keep whole such \
                     as `node.js`, as queries are split"
                ),
                "{key}"
            );
        }
        for key in PROTECTED_WORDS {
            let text = format!("[settings.ranking]\nsynonyms = {{ \"{key}\" = [\"x\"] }}");
            assert!(parse(&text).is_ok(), "{key}");
        }
    }

    #[test]
    fn the_index_hash_depends_on_every_weight_only() {
        let base = IndexSettings::default();
        assert_eq!(base.hash(), IndexSettings::default().hash());
        assert_eq!(base.hash().len(), 64);
        let variants = [
            IndexSettings {
                title_weight: base.title_weight + 1,
                ..base.clone()
            },
            IndexSettings {
                heading_weight: base.heading_weight + 1,
                ..base.clone()
            },
            IndexSettings {
                symbol_weight: base.symbol_weight + 1,
                ..base.clone()
            },
            IndexSettings {
                body_weight: base.body_weight + 1,
                ..base.clone()
            },
            IndexSettings {
                code_weight: base.code_weight + 1,
                ..base.clone()
            },
            IndexSettings {
                path_weight: base.path_weight + 1,
                ..base.clone()
            },
        ];
        let mut hashes: BTreeSet<String> = variants.iter().map(IndexSettings::hash).collect();
        hashes.insert(base.hash());
        assert_eq!(hashes.len(), 7);
    }

    #[test]
    fn load_reads_the_file_and_reports_io_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("zenodotus.toml");
        assert!(matches!(
            Config::load(&file),
            Err(CoreError::ConfigIo { ref path, ref source })
                if path == &file && source.kind() == std::io::ErrorKind::NotFound
        ));
        assert_eq!(Config::load_or_default(&file).unwrap(), Config::default());
        std::fs::write(&file, EXAMPLE).unwrap();
        assert_eq!(Config::load(&file).unwrap(), parse(EXAMPLE).unwrap());
        assert_eq!(Config::load_or_default(&file).unwrap(), parse(EXAMPLE).unwrap());
        assert!(matches!(
            Config::load_or_default(dir.path()),
            Err(CoreError::ConfigIo { .. })
        ));
    }

    #[test]
    fn finds_a_library_by_id() {
        let config = parse(EXAMPLE).unwrap();
        let tokio = LibraryId::parse("tokio").unwrap();
        assert_eq!(config.library(&tokio).unwrap().id, tokio);
        assert!(matches!(
            config.library(&LibraryId::parse("serde").unwrap()),
            Err(CoreError::UnknownLibrary { library }) if library == "serde"
        ));
    }

    #[test]
    fn config_location_precedence() {
        let flag = Some(Path::new("/x/flag.toml"));
        let all = env(&[
            ("ZENODOTUS_CONFIG", "/x/env.toml"),
            ("XDG_CONFIG_HOME", "/xdg"),
            ("HOME", "/home/u"),
        ]);
        assert_eq!(locate_config(flag, &all).unwrap(), Path::new("/x/flag.toml"));
        assert_eq!(locate_config(None, &all).unwrap(), Path::new("/x/env.toml"));
        let no_env_var = env(&[
            ("ZENODOTUS_CONFIG", ""),
            ("XDG_CONFIG_HOME", "/xdg"),
            ("HOME", "/home/u"),
        ]);
        assert_eq!(
            locate_config(None, &no_env_var).unwrap(),
            Path::new("/xdg/zenodotus/zenodotus.toml")
        );
        assert_eq!(
            locate_config(None, &env(&[("HOME", "/home/u")])).unwrap(),
            Path::new("/home/u/.config/zenodotus/zenodotus.toml")
        );
        assert!(matches!(
            locate_config(None, &env(&[])),
            Err(CoreError::NoLocation { what: "config file" })
        ));
    }

    #[test]
    fn data_dir_precedence_and_expansion() {
        let config_path = Path::new("/etc/z/zenodotus.toml");
        let with = |dir: Option<&str>| Settings {
            data_dir: dir.map(PathBuf::from),
            ..Settings::default()
        };
        let home = env(&[("HOME", "/home/u")]);
        let xdg = env(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/home/u")]);
        assert_eq!(
            with(Some("/abs")).data_dir(config_path, &home).unwrap(),
            Path::new("/abs")
        );
        assert_eq!(
            with(Some("rel/data")).data_dir(config_path, &home).unwrap(),
            Path::new("/etc/z/rel/data")
        );
        assert_eq!(
            with(Some("data")).data_dir(Path::new("zenodotus.toml"), &home).unwrap(),
            Path::new("data")
        );
        assert_eq!(
            with(Some("~/z")).data_dir(config_path, &home).unwrap(),
            Path::new("/home/u/z")
        );
        assert_eq!(
            with(None).data_dir(config_path, &xdg).unwrap(),
            Path::new("/xdg/zenodotus")
        );
        assert_eq!(
            with(None).data_dir(config_path, &home).unwrap(),
            Path::new("/home/u/.local/share/zenodotus")
        );
        for dir in [None, Some("~/z")] {
            assert!(matches!(
                with(dir).data_dir(config_path, &env(&[])),
                Err(CoreError::NoLocation { what: "data directory" })
            ));
        }
    }
}

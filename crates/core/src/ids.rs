//! Validated identifiers: libraries, versions, scopes, documents and sections.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ecosystem::Ecosystem;
use crate::error::CoreError;
use crate::runtime::Runtime;

/// Separator between the parts of a [`DocKey`]: the ASCII unit separator, which no valid part contains.
const KEY_SEPARATOR: char = '\u{1f}';
/// Longest accepted library id or id segment, in bytes.
pub const MAX_ID_LEN: usize = 64;
/// Longest accepted version, in bytes.
pub const MAX_VERSION_LEN: usize = 128;
/// Longest accepted name part of a canonical ID, in bytes: npm's limit, the longest of every registry.
pub const MAX_CANONICAL_NAME_LEN: usize = 214;
/// Longest accepted document path, in bytes.
const MAX_DOC_PATH_LEN: usize = 1024;

/// A library identifier such as `tokio` or `vercel/next.js`.
///
/// One optional `org/` prefix is allowed. Each segment starts with a lowercase letter or digit and continues with
/// lowercase letters, digits, `.`, `_` or `-`, up to 64 bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LibraryId(String);

impl LibraryId {
    /// Validates `value` as a library id.
    ///
    /// # Errors
    /// [`CoreError::InvalidLibraryId`] when the value breaks the rules above.
    pub fn parse(value: &str) -> Result<Self, CoreError> {
        let reject = |reason| CoreError::InvalidLibraryId {
            value: value.to_owned(),
            reason,
        };
        let mut segments = value.split('/');
        let first = segments.next().unwrap_or_default();
        match (segments.next(), segments.next()) {
            (_, Some(_)) => return Err(reject("at most one `org/` prefix is allowed")),
            (Some(second), None) => check_slug(first).and(check_slug(second)).map_err(reject)?,
            (None, None) => check_slug(first).map_err(reject)?,
        }
        Ok(Self(value.to_owned()))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A documentation version such as `2.9`, `v1.10.18` or `latest`, compared exactly and case-sensitively.
///
/// Allowed characters are ASCII letters, digits, `.`, `_`, `+` and `-`, plus `!` in `PyPI` scopes and `/` in `GitHub`
/// scopes; length 1 to 128 bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version(String);

impl Version {
    /// Validates `value` as a version of a configured library or of an ecosystem without extra characters.
    ///
    /// # Errors
    /// [`CoreError::InvalidVersion`] when the value breaks the rules above.
    pub fn parse(value: &str) -> Result<Self, CoreError> {
        Self::parse_in(value, None)
    }

    /// Validates `value` as a version of a scope in `ecosystem` (design §6.3): `PyPI` versions may also hold `!` (PEP
    /// 440 epochs) and GitHub versions `/` (git refs such as `release/1.x`).
    ///
    /// # Errors
    /// [`CoreError::InvalidVersion`] when the value breaks the rules.
    pub fn parse_in(value: &str, ecosystem: Option<Ecosystem>) -> Result<Self, CoreError> {
        let reject = |reason| CoreError::InvalidVersion {
            value: value.to_owned(),
            reason,
        };
        if value.is_empty() || value.len() > MAX_VERSION_LEN {
            return Err(reject("must be 1 to 128 bytes long"));
        }
        let extra = |b: u8| match ecosystem {
            Some(Ecosystem::Pypi) => b == b'!',
            Some(Ecosystem::Github) => b == b'/',
            _ => false,
        };
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-') || extra(b))
        {
            return Err(reject(
                "only ASCII letters, digits, `.`, `_`, `+` and `-` are allowed, plus `!` for PyPI and `/` for GitHub",
            ));
        }
        Ok(Self(value.to_owned()))
    }

    /// The version as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A canonical ID such as `npm:react` or `runtime:rust`: an ecosystem prefix and a package name in that ecosystem's
/// grammar (design §6.2).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CanonicalId {
    ecosystem: Ecosystem,
    text: String,
}

impl CanonicalId {
    /// Validates `value` as a canonical ID. `swift:github.com/<owner>/<repo>` is parsed to `github:<owner>/<repo>`,
    /// and GitHub names are lowercased.
    ///
    /// # Errors
    /// [`CoreError::InvalidCanonicalId`] when the prefix is unknown or the name breaks its ecosystem's grammar.
    pub fn parse(value: &str) -> Result<Self, CoreError> {
        let reject = |reason| CoreError::InvalidCanonicalId {
            value: value.to_owned(),
            reason,
        };
        let (prefix, name) = value.split_once(':').ok_or_else(|| reject("missing the `<prefix>:`"))?;
        let ecosystem = Ecosystem::from_prefix(prefix).ok_or_else(|| reject("unknown ecosystem prefix"))?;
        check_common_name(name).map_err(reject)?;
        let (ecosystem, name) = match ecosystem {
            Ecosystem::Swift => (Ecosystem::Github, swift_repo(name).map_err(reject)?),
            Ecosystem::Github => (Ecosystem::Github, name.to_ascii_lowercase()),
            other => (other, name.to_owned()),
        };
        check_name(ecosystem, &name).map_err(reject)?;
        Ok(Self {
            text: format!("{}:{name}", ecosystem.prefix()),
            ecosystem,
        })
    }

    /// The ID of a runtime.
    #[must_use]
    pub fn runtime(runtime: Runtime) -> Self {
        Self {
            ecosystem: Ecosystem::Runtime,
            text: format!("runtime:{}", runtime.name()),
        }
    }

    /// The ecosystem.
    #[must_use]
    pub const fn ecosystem(&self) -> Ecosystem {
        self.ecosystem
    }

    /// The name after the prefix.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.text[self.ecosystem.prefix().len() + 1..]
    }

    /// The whole ID, `<prefix>:<name>`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

/// The rules every canonical name meets: 1 to 214 bytes, no whitespace or control character, no `..` segment and no
/// leading or trailing `/`.
fn check_common_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > MAX_CANONICAL_NAME_LEN {
        return Err("the name must be 1 to 214 bytes long");
    }
    if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("the name must not contain whitespace or control characters");
    }
    if name.starts_with('/') || name.ends_with('/') || name.split('/').any(|part| part == "..") {
        return Err("the name must not start or end with `/` or contain a `..` segment");
    }
    Ok(())
}

/// The `owner/repo` of a `SwiftPM` `github.com/<owner>/<repo>` name, lowercased.
fn swift_repo(name: &str) -> Result<String, &'static str> {
    let lower = name.to_ascii_lowercase();
    lower
        .strip_prefix("github.com/")
        .filter(|repo| !has_git_suffix(repo))
        .map(str::to_owned)
        .ok_or("SwiftPM names must be `github.com/<owner>/<repo>` without `.git`")
}

/// Whether a repository name ends in `.git`, which `SwiftPM` URLs may carry and IDs never do.
fn has_git_suffix(repo: &str) -> bool {
    std::path::Path::new(repo)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("git"))
}

/// Checks a name against its ecosystem's grammar (design §6.2).
fn check_name(ecosystem: Ecosystem, name: &str) -> Result<(), &'static str> {
    fn package(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
    }
    fn lower_package(b: u8) -> bool {
        b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
    }
    let all = |text: &str, allowed: fn(u8) -> bool| !text.is_empty() && text.bytes().all(allowed);
    let two_parts = |separator: char, allowed: fn(u8) -> bool| {
        name.split_once(separator)
            .is_some_and(|(a, b)| all(a, allowed) && all(b, allowed))
    };
    let valid = match ecosystem {
        Ecosystem::Npm => {
            let npm = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-');
            match name.strip_prefix('@') {
                Some(scoped) => scoped
                    .split_once('/')
                    .is_some_and(|(scope, rest)| all(scope, npm) && all(rest, npm)),
                None => all(name, npm),
            }
        }
        Ecosystem::Pypi => {
            all(name, |b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !name.starts_with('-')
                && !name.ends_with('-')
                && !name.contains("--")
        }
        Ecosystem::Maven => two_parts(':', package),
        Ecosystem::Nuget => all(name, lower_package),
        Ecosystem::Go => all(name, |b| package(b) || matches!(b, b'~' | b'/')),
        Ecosystem::Crates => all(name, |b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-')
        }),
        // A Swift name reaches this check as the `owner/repo` of its GitHub id (`swift_repo`).
        Ecosystem::Packagist | Ecosystem::Github | Ecosystem::Swift => two_parts('/', lower_package),
        Ecosystem::Rubygems => all(name, package),
        Ecosystem::Pub => all(name, |b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        Ecosystem::Runtime => Runtime::from_name(name).is_some(),
    };
    if valid { Ok(()) } else { Err(grammar(ecosystem)) }
}

/// The grammar of an ecosystem's names, as an error reason.
const fn grammar(ecosystem: Ecosystem) -> &'static str {
    match ecosystem {
        Ecosystem::Npm => "npm names are an optional `@scope/` and npm's name characters",
        Ecosystem::Pypi => "PyPI names must be PEP 503 normalized: lowercase, with runs of `-_.` written as one `-`",
        Ecosystem::Maven => "Maven names are `groupId:artifactId`",
        Ecosystem::Nuget => "NuGet names are lowercase letters, digits, `.`, `_` and `-`",
        Ecosystem::Go => "Go module paths are letters, digits, `.`, `_`, `~`, `/` and `-`",
        Ecosystem::Crates => "crates.io names are lowercase letters, digits, `_` and `-`",
        Ecosystem::Packagist => "Packagist names are lowercase `vendor/name`",
        Ecosystem::Rubygems => "gem names are letters, digits, `.`, `_` and `-`",
        Ecosystem::Pub => "pub.dev names are lowercase letters, digits and `_`",
        Ecosystem::Github | Ecosystem::Swift => "GitHub names are `owner/repo`",
        Ecosystem::Runtime => "unknown runtime",
    }
}

/// A Context7-style ID: `/owner/repo` names `github:owner/repo`, and `/owner/repo/<v>` or `/owner/repo@<v>` adds an
/// explicit version (design §6.2). `None` when `text` is not of that form.
#[must_use]
pub fn context7_alias(text: &str) -> Option<(CanonicalId, Option<Version>)> {
    let rest = text.strip_prefix('/')?;
    let (path, at_version) = match rest.split_once('@') {
        Some((path, version)) => (path, Some(version)),
        None => (rest, None),
    };
    let parts: Vec<&str> = path.split('/').collect();
    let (repo, version) = match (parts.as_slice(), at_version) {
        ([owner, repo], version) => (format!("{owner}/{repo}"), version),
        ([owner, repo, version], None) => (format!("{owner}/{repo}"), Some(*version)),
        _ => return None,
    };
    let id = CanonicalId::parse(&format!("github:{repo}")).ok()?;
    match version {
        Some(version) => Some((id, Some(Version::parse_in(version, Some(Ecosystem::Github)).ok()?))),
        None => Some((id, None)),
    }
}

/// The library of a scope: a configured slug or a canonical ID (design §6.1).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LibraryRef {
    /// A configured library's slug, as in v1.
    Slug(LibraryId),
    /// A canonical ID.
    Canonical(CanonicalId),
}

impl LibraryRef {
    /// Parses `value`: a runtime module alias such as `node:fs` names its runtime; any other text with a `:` is a
    /// canonical ID, and text without one is a slug.
    ///
    /// # Errors
    /// [`CoreError::InvalidLibraryId`] or [`CoreError::InvalidCanonicalId`].
    pub fn parse(value: &str) -> Result<Self, CoreError> {
        if !value.contains(':') {
            return LibraryId::parse(value).map(Self::Slug);
        }
        if let [runtime] = Runtime::from_alias(value).as_slice() {
            return Ok(Self::Canonical(runtime.canonical_id()));
        }
        CanonicalId::parse(value).map(Self::Canonical)
    }

    /// The rendered text, which document keys and chunk ids hash.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Slug(id) => id.as_str(),
            Self::Canonical(id) => id.as_str(),
        }
    }

    /// The ecosystem of a canonical ID; `None` for a slug.
    #[must_use]
    pub fn ecosystem(&self) -> Option<Ecosystem> {
        match self {
            Self::Slug(_) => None,
            Self::Canonical(id) => Some(id.ecosystem()),
        }
    }
}

impl From<LibraryId> for LibraryRef {
    fn from(id: LibraryId) -> Self {
        Self::Slug(id)
    }
}

impl From<CanonicalId> for LibraryRef {
    fn from(id: CanonicalId) -> Self {
        Self::Canonical(id)
    }
}

/// One library at one version: the unit that is synced, filtered on and reported.
///
/// Serialized as v1's object `{"library": "<library>", "version": "<version>"}`, the library as its text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "ScopeRecord", into = "ScopeRecord")]
pub struct Scope {
    /// The library.
    pub library: LibraryRef,
    /// The version.
    pub version: Version,
}

/// The stored form of a [`Scope`].
#[derive(Serialize, Deserialize)]
struct ScopeRecord {
    library: String,
    version: String,
}

impl Scope {
    /// Creates a scope.
    #[must_use]
    pub fn new(library: impl Into<LibraryRef>, version: Version) -> Self {
        Self {
            library: library.into(),
            version,
        }
    }

    /// Parses `<library>@<version>`, split on the last `@`, checking the version against the library's ecosystem.
    ///
    /// # Errors
    /// [`CoreError::InvalidVersion`] without an `@` or for a bad version, otherwise as [`LibraryRef::parse`].
    pub fn parse(text: &str) -> Result<Self, CoreError> {
        let (library, version) = text.rsplit_once('@').ok_or_else(|| CoreError::InvalidVersion {
            value: text.to_owned(),
            reason: "a scope is `<library>@<version>`",
        })?;
        Self::from_parts(library, version)
    }

    /// The scope of `library` at `version`, both as text.
    fn from_parts(library: &str, version: &str) -> Result<Self, CoreError> {
        let library = LibraryRef::parse(library)?;
        let version = Version::parse_in(version, library.ecosystem())?;
        Ok(Self { library, version })
    }

    /// The scope hash: the SHA-256 of the scope text, hex-encoded, which names every per-scope file.
    #[must_use]
    pub fn hash(&self) -> String {
        sha256_hex(&self.to_string())
    }
}

impl TryFrom<ScopeRecord> for Scope {
    type Error = CoreError;
    fn try_from(record: ScopeRecord) -> Result<Self, Self::Error> {
        Self::from_parts(&record.library, &record.version)
    }
}

impl From<Scope> for ScopeRecord {
    fn from(scope: Scope) -> Self {
        Self {
            library: scope.library.as_str().to_owned(),
            version: scope.version.0,
        }
    }
}

/// The lowercase hex SHA-256 of `text`.
#[must_use]
pub fn sha256_hex(text: &str) -> String {
    to_hex(&Sha256::digest(text.as_bytes()))
}

impl fmt::Display for LibraryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for CanonicalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl fmt::Display for LibraryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for Scope {
    /// Renders `library@version`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.library, self.version)
    }
}

/// The key of one source document within one scope: `library \u{1f} version \u{1f} doc_path`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DocKey(String);

impl DocKey {
    /// Builds the key of `doc_path` inside `scope`.
    ///
    /// # Errors
    /// [`CoreError::InvalidDocPath`] when the path is empty, longer than 1024 bytes, or contains a control character.
    pub fn new(scope: &Scope, doc_path: &str) -> Result<Self, CoreError> {
        let reject = |reason| CoreError::InvalidDocPath {
            value: doc_path.to_owned(),
            reason,
        };
        if doc_path.is_empty() || doc_path.len() > MAX_DOC_PATH_LEN {
            return Err(reject("must be 1 to 1024 bytes long"));
        }
        if doc_path.chars().any(char::is_control) {
            return Err(reject("control characters are not allowed"));
        }
        let (library, version) = (scope.library.as_str(), scope.version.as_str());
        Ok(Self(format!(
            "{library}{KEY_SEPARATOR}{version}{KEY_SEPARATOR}{doc_path}"
        )))
    }

    /// The key as stored in the index.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The document path part of the key.
    #[must_use]
    pub fn doc_path(&self) -> &str {
        // Library ids and versions cannot contain the separator, so the path starts after the second one.
        self.0.splitn(3, KEY_SEPARATOR).nth(2).unwrap_or_default()
    }
}

/// The identifier of one section: lowercase hex SHA-256 over the length-prefixed parts
/// `library`, `version`, `doc_path` and `ordinal`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChunkId(String);

impl ChunkId {
    /// Derives the id of section number `ordinal` of `doc_path` in `scope`.
    #[must_use]
    pub fn derive(scope: &Scope, doc_path: &str, ordinal: u32) -> Self {
        let mut hasher = Sha256::new();
        let ordinal = ordinal.to_le_bytes();
        for part in [
            scope.library.as_str().as_bytes(),
            scope.version.as_str().as_bytes(),
            doc_path.as_bytes(),
            &ordinal,
        ] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
        Self(to_hex(&hasher.finalize()))
    }

    /// The id as 64 lowercase hex characters.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Checks one library-id segment, returning the broken rule.
fn check_slug(segment: &str) -> Result<(), &'static str> {
    let bytes = segment.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_ID_LEN {
        return Err("each segment must be 1 to 64 bytes long");
    }
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
        return Err("each segment must start with a lowercase letter or a digit");
    }
    if !bytes
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err("only lowercase letters, digits, `.`, `_` and `-` are allowed");
    }
    Ok(())
}

/// Lowercase hex encoding.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

impl TryFrom<String> for LibraryId {
    type Error = CoreError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<LibraryId> for String {
    fn from(id: LibraryId) -> Self {
        id.0
    }
}

impl TryFrom<String> for CanonicalId {
    type Error = CoreError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<CanonicalId> for String {
    fn from(id: CanonicalId) -> Self {
        id.text
    }
}

impl TryFrom<String> for Version {
    type Error = CoreError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Version> for String {
    fn from(version: Version) -> Self {
        version.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(library: &str, version: &str) -> Scope {
        Scope::new(LibraryId::parse(library).unwrap(), Version::parse(version).unwrap())
    }

    #[test]
    fn library_id_accepts_plain_and_namespaced_slugs() {
        for ok in [
            "tokio",
            "next.js",
            "a",
            "0x",
            "my_lib-2",
            "vercel/next.js",
            &"a".repeat(MAX_ID_LEN),
        ] {
            assert_eq!(LibraryId::parse(ok).unwrap().as_str(), ok);
        }
    }

    #[test]
    fn library_id_rejects_invalid_values() {
        let too_long = "a".repeat(MAX_ID_LEN + 1);
        for bad in [
            "",
            "Tokio",
            "-x",
            ".x",
            "_x",
            "a b",
            "a/b/c",
            "/a",
            "a/",
            "a@b",
            "ä",
            too_long.as_str(),
        ] {
            let err = LibraryId::parse(bad).unwrap_err();
            assert!(
                matches!(err, CoreError::InvalidLibraryId { ref value, .. } if value == bad),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn version_accepts_common_version_strings() {
        for ok in [
            "1",
            "2.9",
            "v1.10.18",
            "latest",
            "1.0.0-rc.1+build_7",
            &"9".repeat(MAX_ID_LEN),
        ] {
            assert_eq!(Version::parse(ok).unwrap().as_str(), ok);
        }
    }

    #[test]
    fn version_is_case_sensitive_and_exact() {
        assert_ne!(Version::parse("V1").unwrap(), Version::parse("v1").unwrap());
    }

    #[test]
    fn version_rejects_invalid_values() {
        let too_long = "9".repeat(MAX_VERSION_LEN + 1);
        for bad in ["", "1 0", "1/0", "1@0", "1\u{1f}0", "ü", too_long.as_str()] {
            let err = Version::parse(bad).unwrap_err();
            assert!(
                matches!(err, CoreError::InvalidVersion { ref value, .. } if value == bad),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn scope_renders_library_at_version() {
        assert_eq!(scope("tokio", "1.40").to_string(), "tokio@1.40");
    }

    #[test]
    fn doc_key_joins_parts_with_unit_separator_and_exposes_path() {
        let key = DocKey::new(&scope("tokio", "1.40"), "tutorial/spawning.md").unwrap();
        assert_eq!(key.as_str(), "tokio\u{1f}1.40\u{1f}tutorial/spawning.md");
        assert_eq!(key.doc_path(), "tutorial/spawning.md");
    }

    #[test]
    fn doc_key_path_may_contain_at_signs_and_spaces() {
        let key = DocKey::new(&scope("a", "1"), "docs/a@b c.md").unwrap();
        assert_eq!(key.doc_path(), "docs/a@b c.md");
    }

    #[test]
    fn doc_key_rejects_empty_long_or_control_paths() {
        let s = scope("a", "1");
        let too_long = "p".repeat(MAX_DOC_PATH_LEN + 1);
        for bad in ["", "a\u{1f}b", "a\nb", "a\0b", too_long.as_str()] {
            let err = DocKey::new(&s, bad).unwrap_err();
            assert!(
                matches!(err, CoreError::InvalidDocPath { ref value, .. } if value == bad),
                "{bad:?}"
            );
        }
        assert!(DocKey::new(&s, &"p".repeat(MAX_DOC_PATH_LEN)).is_ok());
    }

    #[test]
    fn chunk_id_is_stable_lowercase_hex() {
        let id = ChunkId::derive(&scope("tokio", "1.40"), "a.md", 3);
        assert_eq!(id, ChunkId::derive(&scope("tokio", "1.40"), "a.md", 3));
        assert_eq!(id.as_str().len(), 64);
        assert!(
            id.as_str()
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }

    #[test]
    fn chunk_id_matches_the_documented_encoding() {
        let mut hasher = Sha256::new();
        for part in [b"tokio".as_slice(), b"1.40", b"a.md", &3u32.to_le_bytes()] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
        let expected: String = hasher
            .finalize()
            .iter()
            .flat_map(|b| [b >> 4, b & 0xf])
            .map(|nibble| char::from(b"0123456789abcdef"[usize::from(nibble)]))
            .collect();
        assert_eq!(ChunkId::derive(&scope("tokio", "1.40"), "a.md", 3).as_str(), expected);
    }

    #[test]
    fn chunk_id_has_no_concatenation_collisions() {
        let a = ChunkId::derive(&scope("ab", "1"), "c.md", 0);
        let b = ChunkId::derive(&scope("a", "b1"), "c.md", 0);
        let c = ChunkId::derive(&scope("a", "1"), "bc.md", 0);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(
            ChunkId::derive(&scope("a", "1"), "c.md", 0),
            ChunkId::derive(&scope("a", "1"), "c.md", 1)
        );
    }

    #[test]
    fn serde_round_trips_through_strings_and_validates() {
        let id: LibraryId = serde_json_like("\"tokio\"").unwrap();
        assert_eq!(id.as_str(), "tokio");
        assert!(serde_json_like::<LibraryId>("\"Bad\"").is_err());
        let v: Version = serde_json_like("\"1.0\"").unwrap();
        assert_eq!(String::from(v), "1.0");
        assert!(serde_json_like::<Version>("\"1 0\"").is_err());
        assert_eq!(String::from(id), "tokio");
    }

    /// Deserializes a TOML value through a one-key table, to exercise the serde attributes without a JSON crate.
    fn serde_json_like<T: for<'de> Deserialize<'de>>(literal: &str) -> Result<T, toml::de::Error> {
        #[derive(Deserialize)]
        struct Wrapper<T> {
            v: T,
        }
        toml::from_str::<Wrapper<T>>(&format!("v = {literal}")).map(|w| w.v)
    }

    fn canonical(text: &str) -> CanonicalId {
        CanonicalId::parse(text).unwrap()
    }

    #[test]
    fn canonical_ids_of_every_ecosystem_parse() {
        let long_name = format!("npm:{}", "a".repeat(MAX_CANONICAL_NAME_LEN));
        for (text, ecosystem, name) in [
            ("npm:react", Ecosystem::Npm, "react"),
            ("npm:@tanstack/react-query", Ecosystem::Npm, "@tanstack/react-query"),
            ("npm:JSONStream", Ecosystem::Npm, "JSONStream"),
            ("pypi:fastapi", Ecosystem::Pypi, "fastapi"),
            ("pypi:typing-extensions", Ecosystem::Pypi, "typing-extensions"),
            (
                "maven:org.springframework:spring-core",
                Ecosystem::Maven,
                "org.springframework:spring-core",
            ),
            ("nuget:newtonsoft.json", Ecosystem::Nuget, "newtonsoft.json"),
            ("go:github.com/gin-gonic/gin", Ecosystem::Go, "github.com/gin-gonic/gin"),
            (
                "go:github.com/Azure/azure-sdk-for-go/v2",
                Ecosystem::Go,
                "github.com/Azure/azure-sdk-for-go/v2",
            ),
            ("crates:serde_json", Ecosystem::Crates, "serde_json"),
            ("packagist:laravel/framework", Ecosystem::Packagist, "laravel/framework"),
            ("rubygems:rails", Ecosystem::Rubygems, "rails"),
            ("rubygems:ActiveRecord-Thing", Ecosystem::Rubygems, "ActiveRecord-Thing"),
            ("pub:http", Ecosystem::Pub, "http"),
            ("github:tokio-rs/axum", Ecosystem::Github, "tokio-rs/axum"),
            ("runtime:rust", Ecosystem::Runtime, "rust"),
            (long_name.as_str(), Ecosystem::Npm, &long_name[4..]),
        ] {
            let id = canonical(text);
            assert_eq!(id.ecosystem(), ecosystem, "{text}");
            assert_eq!(id.name(), name, "{text}");
            assert_eq!(id.as_str(), text);
            assert_eq!(id.to_string(), text);
        }
    }

    #[test]
    fn swift_ids_and_github_input_parse_to_lowercase_github_ids() {
        for input in [
            "swift:github.com/Apple/swift-nio",
            "swift:github.com/apple/swift-nio",
            "github:Apple/Swift-NIO",
        ] {
            assert_eq!(canonical(input).as_str(), "github:apple/swift-nio", "{input}");
        }
        for bad in [
            "swift:gitlab.com/a/b",
            "swift:github.com/a/b.git",
            "swift:github.com/a",
            "swift:github.com/a/b/c",
        ] {
            assert!(CanonicalId::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn invalid_canonical_ids_are_rejected_with_a_reason() {
        let too_long = format!("npm:{}", "a".repeat(MAX_CANONICAL_NAME_LEN + 1));
        for bad in [
            "npm:",
            too_long.as_str(),
            "npm:re act",
            "npm:a\u{1f}b",
            "npm:a\nb",
            "go:github.com/../x",
            "go:/github.com/x",
            "go:github.com/x/",
            "go:github.com/x+y",
            "cargo:serde",
            ":serde",
            "maven:spring-core",
            "maven:a:b:c",
            "maven::b",
            "pypi:Django",
            "pypi:typing_extensions",
            "pypi:a--b",
            "pypi:-a",
            "nuget:Newtonsoft.Json",
            "crates:Serde",
            "packagist:laravel",
            "packagist:Laravel/framework",
            "pub:Http",
            "pub:http-client",
            "rubygems:a/b",
            "npm:@scope",
            "npm:@/x",
            "npm:a/b",
            "github:a",
            "github:a/b/c",
            "github:a/b c",
            "runtime:cobol",
            "runtime:Rust",
            "react",
        ] {
            let error = CanonicalId::parse(bad).unwrap_err();
            assert!(
                matches!(error, CoreError::InvalidCanonicalId { ref value, .. } if value == bad),
                "{bad:?}: {error}"
            );
        }
    }

    #[test]
    fn exact_matches_compare_bytes() {
        assert_ne!(canonical("npm:JSONStream"), canonical("npm:jsonstream"));
        assert_ne!(canonical("maven:org.Foo:bar"), canonical("maven:org.foo:bar"));
        assert_ne!(canonical("go:github.com/A/b"), canonical("go:github.com/a/b"));
        assert_ne!(canonical("rubygems:Rails"), canonical("rubygems:rails"));
    }

    #[test]
    fn library_refs_are_slugs_or_canonical_ids() {
        let slug = LibraryRef::parse("react").unwrap();
        assert_eq!(slug, LibraryRef::Slug(LibraryId::parse("react").unwrap()));
        assert_eq!(slug.ecosystem(), None);
        let id = LibraryRef::parse("npm:react").unwrap();
        assert_eq!(id, LibraryRef::Canonical(canonical("npm:react")));
        assert_eq!(id.ecosystem(), Some(Ecosystem::Npm));
        for text in ["react", "vercel/next.js", "npm:react", "npm:@types/node", "runtime:jdk"] {
            let parsed = LibraryRef::parse(text).unwrap();
            assert_eq!(parsed.as_str(), text);
            assert_eq!(parsed.to_string(), text);
            assert_eq!(LibraryRef::parse(&parsed.to_string()).unwrap(), parsed);
        }
        assert!(matches!(
            LibraryRef::parse("React"),
            Err(CoreError::InvalidLibraryId { .. })
        ));
        assert!(matches!(
            LibraryRef::parse("npm:"),
            Err(CoreError::InvalidCanonicalId { .. })
        ));
        assert_eq!(LibraryRef::from(canonical("pub:http")).as_str(), "pub:http");
    }

    #[test]
    fn runtime_aliases_with_a_colon_resolve_before_canonical_parsing() {
        assert_eq!(LibraryRef::parse("node:fs").unwrap().as_str(), "runtime:node");
        assert_eq!(LibraryRef::parse("dart:core").unwrap().as_str(), "runtime:dart");
        assert_eq!(LibraryRef::parse("npm:fs").unwrap().as_str(), "npm:fs");
        assert!(LibraryRef::parse("node:").is_err());
    }

    #[test]
    fn version_length_limit_is_separate_from_ids() {
        assert_eq!(MAX_VERSION_LEN, 128);
        assert!(Version::parse(&"9".repeat(MAX_VERSION_LEN)).is_ok());
        assert!(Version::parse(&"9".repeat(MAX_VERSION_LEN + 1)).is_err());
        assert!(LibraryId::parse(&"a".repeat(MAX_ID_LEN + 1)).is_err());
    }

    #[test]
    fn versions_allow_bang_for_pypi_and_slash_for_github_only() {
        assert_eq!(
            Version::parse_in("1!2.0", Some(Ecosystem::Pypi)).unwrap().as_str(),
            "1!2.0"
        );
        assert_eq!(
            Version::parse_in("release/1.x", Some(Ecosystem::Github))
                .unwrap()
                .as_str(),
            "release/1.x"
        );
        for (bad, ecosystem) in [
            ("1!2.0", None),
            ("1!2.0", Some(Ecosystem::Npm)),
            ("release/1.x", None),
            ("release/1.x", Some(Ecosystem::Go)),
            ("a@b", Some(Ecosystem::Github)),
        ] {
            assert!(Version::parse_in(bad, ecosystem).is_err(), "{bad} {ecosystem:?}");
        }
    }

    #[test]
    fn scopes_render_and_parse_on_the_last_at() {
        for text in [
            "react@19.1.0",
            "npm:@scope/name@1.2.3",
            "github:o/r@release/1.x",
            "pypi:x@1!2.0",
            "runtime:rust@1.85.1",
            "runtime:jdk@21.0.5",
            "runtime:dotnet@9",
            "runtime:web@latest",
        ] {
            let scope = Scope::parse(text).unwrap();
            assert_eq!(scope.to_string(), text);
        }
        let scope = Scope::parse("npm:@scope/name@1.2.3").unwrap();
        assert_eq!(scope.library.as_str(), "npm:@scope/name");
        assert_eq!(scope.version.as_str(), "1.2.3");
        let at_limit = format!("react@{}", "9".repeat(MAX_VERSION_LEN));
        assert!(Scope::parse(&at_limit).is_ok());
        let over = format!("react@{}", "9".repeat(MAX_VERSION_LEN + 1));
        for bad in [
            "npm:x@release/1.x",
            "npm:x@1!2.0",
            "react",
            "react@",
            "@1.0",
            "React@1",
            over.as_str(),
        ] {
            assert!(Scope::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn scope_records_keep_the_v1_object_shape() {
        let v1 = r#"{"library":"react","version":"19.1.0"}"#;
        let scope: Scope = serde_json::from_str(v1).unwrap();
        assert_eq!(scope, Scope::parse("react@19.1.0").unwrap());
        assert_eq!(serde_json::to_string(&scope).unwrap(), v1);
        let canonical_scope = Scope::parse("github:o/r@release/1.x").unwrap();
        let text = serde_json::to_string(&canonical_scope).unwrap();
        assert_eq!(text, r#"{"library":"github:o/r","version":"release/1.x"}"#);
        assert_eq!(serde_json::from_str::<Scope>(&text).unwrap(), canonical_scope);
        for bad in [
            r#"{"library":"React","version":"1"}"#,
            r#"{"library":"npm:x","version":"a/b"}"#,
        ] {
            assert!(serde_json::from_str::<Scope>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn slug_scopes_keep_v1_keys_and_chunk_ids() {
        let v1_scope = scope("tokio", "1.40");
        let parsed = Scope::parse("tokio@1.40").unwrap();
        assert_eq!(parsed, v1_scope);
        assert_eq!(
            DocKey::new(&parsed, "a.md").unwrap().as_str(),
            "tokio\u{1f}1.40\u{1f}a.md"
        );
        assert_eq!(
            ChunkId::derive(&parsed, "a.md", 3),
            ChunkId::derive(&v1_scope, "a.md", 3)
        );
        let npm = Scope::parse("npm:@a/b@1.0").unwrap();
        let key = DocKey::new(&npm, "x@y.md").unwrap();
        assert_eq!(key.as_str(), "npm:@a/b\u{1f}1.0\u{1f}x@y.md");
        assert_eq!(key.doc_path(), "x@y.md");
    }

    #[test]
    fn the_scope_hash_is_the_sha256_of_the_scope_text() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        for text in ["tokio@1.40", "npm:@tanstack/react-query@5.0.0"] {
            let scope = Scope::parse(text).unwrap();
            assert_eq!(scope.hash(), sha256_hex(text));
        }
    }

    #[test]
    fn context7_ids_are_github_aliases() {
        let (id, version) = context7_alias("/vercel/next.js").unwrap();
        assert_eq!((id.as_str(), version), ("github:vercel/next.js", None));
        for text in ["/owner/Repo/v1.2", "/owner/repo@v1.2"] {
            let (id, version) = context7_alias(text).unwrap();
            assert_eq!(id.as_str(), "github:owner/repo");
            assert_eq!(version.unwrap().as_str(), "v1.2");
        }
        for not_alias in ["owner/repo", "/owner", "/", "/a/b/c/d", "/a/b@", "/a/b/x y"] {
            assert!(context7_alias(not_alias).is_none(), "{not_alias}");
        }
    }

    #[test]
    fn ids_display_as_their_text() {
        assert_eq!(LibraryId::parse("a/b").unwrap().to_string(), "a/b");
        assert_eq!(Version::parse("1.0").unwrap().to_string(), "1.0");
    }
}

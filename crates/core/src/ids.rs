//! Validated identifiers: libraries, versions, scopes, documents and sections.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::CoreError;

/// Separator between the parts of a [`DocKey`]: the ASCII unit separator, which no valid part contains.
const KEY_SEPARATOR: char = '\u{1f}';
/// Longest accepted library id, version or id segment, in bytes.
const MAX_ID_LEN: usize = 64;
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
/// Allowed characters are ASCII letters, digits, `.`, `_`, `+` and `-`; length 1 to 64 bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version(String);

impl Version {
    /// Validates `value` as a version.
    ///
    /// # Errors
    /// [`CoreError::InvalidVersion`] when the value breaks the rules above.
    pub fn parse(value: &str) -> Result<Self, CoreError> {
        let reject = |reason| CoreError::InvalidVersion {
            value: value.to_owned(),
            reason,
        };
        if value.is_empty() || value.len() > MAX_ID_LEN {
            return Err(reject("must be 1 to 64 bytes long"));
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
        {
            return Err(reject("only ASCII letters, digits, `.`, `_`, `+` and `-` are allowed"));
        }
        Ok(Self(value.to_owned()))
    }

    /// The version as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One library at one version: the unit that is synced, filtered on and reported.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Scope {
    /// The library.
    pub library: LibraryId,
    /// The version.
    pub version: Version,
}

impl Scope {
    /// Creates a scope.
    #[must_use]
    pub fn new(library: LibraryId, version: Version) -> Self {
        Self { library, version }
    }
}

impl fmt::Display for Scope {
    /// Renders `library@version`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.library.as_str(), self.version.as_str())
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
        let too_long = "9".repeat(MAX_ID_LEN + 1);
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
}

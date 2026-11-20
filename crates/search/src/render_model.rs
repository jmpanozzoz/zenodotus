//! The serializable responses of `resolve_library` and `query_docs`, shared by the MCP server and the CLI.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How the version of a query was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VersionResolution {
    /// The request named the version.
    Explicit,
    /// The version the config marks as the default.
    Default,
    /// No version is the default; the first one in the config was used.
    FirstListed,
}

/// Why a library matched a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// The name is the library id.
    Exact,
    /// The name is an alias or the display name.
    Alias,
    /// The name equals the id, display name or an alias after lowercasing and dropping non-alphanumerics.
    Normalized,
    /// The normalized name starts the normalized id, display name or an alias.
    Prefix,
    /// The normalized name is close to the normalized id, display name or an alias (Jaro-Winkler).
    Fuzzy,
}

/// What a result has in common with the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// An identifier of the query is one of the section's symbols.
    ExactSymbol,
    /// A query word is in the title or heading path.
    Title,
    /// A query word is in a code block.
    Code,
    /// The section has code in the requested language.
    Language,
    /// The query's words appear as a phrase.
    Phrase,
}

/// The kind of a notice attached to a query response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    /// No section matched.
    NoResults,
    /// Results exist, but none matches the query well.
    WeakMatch,
}

/// One library that matches a `resolve_library` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LibraryMatch {
    /// The id to pass to `query_docs`.
    pub library_id: String,
    /// Display name.
    pub name: String,
    /// One-line description.
    pub description: String,
    /// Indexed versions, in config order.
    pub versions: Vec<String>,
    /// The version used when a query names none.
    pub default_version: String,
    /// Where the default version's documentation comes from.
    pub source: String,
    /// When any version last synced successfully (RFC 3339).
    pub last_synced: Option<String>,
    /// Why the library matched.
    pub match_kind: MatchKind,
}

/// The response of `resolve_library`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResolveLibraryResult {
    /// Matching libraries, best first.
    pub matches: Vec<LibraryMatch>,
}

/// The sync status of the queried version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncStatus {
    /// Goes up by one with every sync that changed a section.
    pub revision: u64,
    /// When a sync last ended `ok` or `partial` (RFC 3339).
    pub last_success: Option<String>,
    /// How the last sync ended: `ok`, `partial` or `failed`.
    pub outcome: String,
}

/// One section returned by `query_docs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResultItem {
    /// Position in the results, from 1.
    pub rank: u32,
    /// The relevance score; only comparable within one response.
    pub score: f32,
    /// What the section has in common with the query.
    pub signals: Vec<Signal>,
    /// Stable section id.
    pub chunk_id: String,
    /// The document title.
    pub title: String,
    /// The headings from the document root down to the section.
    pub heading_path: Vec<String>,
    /// Where the section comes from: a URL with an anchor, or `local:<path>`.
    pub source_ref: String,
    /// The version the section belongs to.
    pub version: String,
    /// Languages of the section's code blocks.
    pub languages: Vec<String>,
    /// The section's Markdown.
    pub text: String,
}

/// A note on the quality of a response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Notice {
    /// What happened.
    pub kind: NoticeKind,
    /// An explanation for the reader.
    pub message: String,
    /// The library's other indexed versions.
    pub other_versions: Vec<String>,
}

/// The response of `query_docs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QueryDocsResult {
    /// The queried library.
    pub library_id: String,
    /// The queried version.
    pub version: String,
    /// How the version was chosen.
    pub version_resolution: VersionResolution,
    /// The sync status of the version.
    pub sync_status: SyncStatus,
    /// Matching sections, best first.
    pub results: Vec<ResultItem>,
    /// Set when nothing or only weak matches were found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<Notice>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_serialize_in_snake_case() {
        let json = serde_json::to_string(&(
            VersionResolution::FirstListed,
            MatchKind::Normalized,
            Signal::ExactSymbol,
            NoticeKind::WeakMatch,
        ))
        .unwrap();
        assert_eq!(json, r#"["first_listed","normalized","exact_symbol","weak_match"]"#);
    }

    #[test]
    fn a_response_without_notice_omits_it_and_round_trips() {
        let result = QueryDocsResult {
            library_id: "a".to_owned(),
            version: "1".to_owned(),
            version_resolution: VersionResolution::Explicit,
            sync_status: SyncStatus {
                revision: 1,
                last_success: None,
                outcome: "ok".to_owned(),
            },
            results: Vec::new(),
            notice: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("notice"), "{json}");
        assert_eq!(serde_json::from_str::<QueryDocsResult>(&json).unwrap(), result);
    }

    #[test]
    fn the_output_schema_names_every_field() {
        let schema = serde_json::to_string(&schemars::schema_for!(QueryDocsResult)).unwrap();
        for field in [
            "version_resolution",
            "sync_status",
            "results",
            "signals",
            "heading_path",
            "notice",
        ] {
            assert!(schema.contains(field), "{field}");
        }
    }
}

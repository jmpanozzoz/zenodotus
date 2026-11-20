//! From one document's text to a [`Document`], or to nothing when it has not changed. Shared by every source.

use zenodotus_core::ids::{DocKey, Scope};
use zenodotus_core::model::{DocState, Document, content_hash};
use zenodotus_core::time::Timestamp;

use crate::markdown;
use crate::sectioner::{SectionRules, SourceRef, sectionize};

/// What a source knows about one fetched or read document.
#[derive(Debug, Clone, Copy)]
pub struct Fetched<'a> {
    /// The document key.
    pub doc_key: &'a DocKey,
    /// Where sections point back to.
    pub source: SourceRef<'a>,
    /// The Markdown text.
    pub text: &'a str,
    /// The `ETag` response header.
    pub etag: Option<&'a str>,
    /// The `Last-Modified` response header.
    pub last_modified: Option<&'a str>,
    /// When the body was read.
    pub fetched_at: Timestamp,
}

/// Parses and sections `fetched`, unless `previous` already has the same content hash.
#[must_use]
pub fn build(
    scope: &Scope,
    fetched: &Fetched<'_>,
    previous: Option<&DocState>,
    rules: &SectionRules,
) -> Option<Document> {
    let hash = content_hash(fetched.text);
    if previous.is_some_and(|state| state.content_hash == hash) {
        return None;
    }
    let parsed = markdown::parse(fetched.doc_key.doc_path(), fetched.text);
    Some(Document {
        state: DocState {
            doc_key: fetched.doc_key.clone(),
            etag: fetched.etag.map(str::to_owned),
            last_modified: fetched.last_modified.map(str::to_owned),
            content_hash: hash,
            fetched_at: fetched.fetched_at,
        },
        sections: sectionize(scope, fetched.doc_key, fetched.source, &parsed, rules),
    })
}

#[cfg(test)]
mod tests {
    use zenodotus_core::ids::{LibraryId, Version};

    use super::*;

    #[test]
    fn builds_a_document_and_skips_an_unchanged_one() {
        let scope = Scope::new(LibraryId::parse("a").unwrap(), Version::parse("1").unwrap());
        let key = DocKey::new(&scope, "page").unwrap();
        let fetched = Fetched {
            doc_key: &key,
            source: SourceRef::Url("https://x.dev/page"),
            text: "# Page\n\nBody\n",
            etag: Some("\"v1\""),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT"),
            fetched_at: Timestamp::from_unix(9),
        };
        let document = build(&scope, &fetched, None, &SectionRules::default()).unwrap();
        assert_eq!(document.state.etag.as_deref(), Some("\"v1\""));
        assert_eq!(
            document.state.last_modified.as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT")
        );
        assert_eq!(document.state.content_hash, content_hash(fetched.text));
        assert_eq!(document.state.fetched_at, Timestamp::from_unix(9));
        assert_eq!(document.sections.len(), 1);
        assert_eq!(document.sections[0].source_ref, "https://x.dev/page#page");
        assert!(build(&scope, &fetched, Some(&document.state), &SectionRules::default()).is_none());
        let mut stale = document.state;
        stale.content_hash = content_hash("old");
        assert!(build(&scope, &fetched, Some(&stale), &SectionRules::default()).is_some());
    }
}

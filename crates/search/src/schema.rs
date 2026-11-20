//! The index schema and the three kinds of index documents: sections, `doc_state` and `version_state`.

use serde::Serialize;
use tantivy::TantivyDocument;
use tantivy::schema::{FAST, Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions};
use zenodotus_core::config::IndexSettings;
use zenodotus_core::ids::{DocKey, Scope};
use zenodotus_core::model::{DocState, Section, VersionState};

use crate::analyzer::{CODE_IDENT, CODE_WORDS};
use crate::fences::split_code;

/// Version of the schema and of the way documents are built. Stored in every commit payload; a different value at
/// open time means the index must be rebuilt.
pub const SCHEMA_VERSION: u32 = 1;

/// Value of the `kind` field of a section.
pub const KIND_SECTION: &str = "section";
/// Value of the `kind` field of a document's change-detection state.
pub const KIND_DOC_STATE: &str = "doc_state";
/// Value of the `kind` field of a version's sync record.
pub const KIND_VERSION_STATE: &str = "version_state";

/// Separates the prefix of a metadata key from what it names.
const KEY_SEPARATOR: char = '\u{1f}';

/// The fields of the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fields {
    /// `section`, `doc_state` or `version_state`.
    pub kind: Field,
    /// Unique per index document.
    pub key: Field,
    /// `library@version`.
    pub scope: Field,
    /// The source document, on sections and `doc_state` documents.
    pub doc_key: Field,
    /// The section id, used to break score ties.
    pub chunk_id: Field,
    /// Every part of a section, each repeated by its index weight.
    pub combined: Field,
    /// Whole identifiers and their qualified suffixes.
    pub symbols_exact: Field,
    /// Code languages of a section, lowercased.
    pub lang: Field,
    /// The serialized `Section`, `DocState` or `VersionState`.
    pub payload: Field,
}

/// Builds the schema and returns it with its fields. The result is the same on every call.
#[must_use]
pub fn build_schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let words = TextFieldIndexing::default()
        .set_tokenizer(CODE_WORDS)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let identifiers = TextFieldIndexing::default()
        .set_tokenizer(CODE_IDENT)
        .set_index_option(IndexRecordOption::WithFreqs)
        .set_fieldnorms(false);
    let fields = Fields {
        kind: builder.add_text_field("kind", STRING),
        key: builder.add_text_field("key", STRING),
        scope: builder.add_text_field("scope", STRING),
        doc_key: builder.add_text_field("doc_key", STRING),
        chunk_id: builder.add_text_field("chunk_id", STRING | FAST),
        combined: builder.add_text_field("combined", TextOptions::default().set_indexing_options(words)),
        symbols_exact: builder.add_text_field(
            "symbols_exact",
            TextOptions::default().set_indexing_options(identifiers),
        ),
        lang: builder.add_text_field("lang", STRING),
        payload: builder.add_text_field("payload", STORED),
    };
    (builder.build(), fields)
}

/// The `key` of a document's `doc_state` index document.
#[must_use]
pub fn doc_state_key(doc_key: &DocKey) -> String {
    format!("ds{KEY_SEPARATOR}{}", doc_key.as_str())
}

/// The `key` of a scope's `version_state` index document.
#[must_use]
pub fn version_state_key(scope: &Scope) -> String {
    format!("vs{KEY_SEPARATOR}{scope}")
}

/// Serializes a domain value for the `payload` field.
fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("domain types serialize to JSON")
}

/// The texts that make up a section's `combined` field, each with how many times it is repeated: its own heading
/// (`title_weight`), the document title and ancestor headings (`heading_weight`), its symbols (`symbol_weight`),
/// its prose (`body_weight`), its code (`code_weight`) and its document path without extension (`path_weight`).
#[must_use]
pub fn combined_parts(section: &Section, weights: &IndexSettings) -> Vec<(String, u32)> {
    let (own_heading, ancestors) = match section.heading_path.split_last() {
        Some((last, ancestors)) => (last.clone(), ancestors),
        None => (section.title.clone(), &[][..]),
    };
    let mut headings: Vec<&str> = Vec::new();
    for heading in std::iter::once(&section.title).chain(ancestors) {
        if *heading != own_heading && !headings.contains(&heading.as_str()) {
            headings.push(heading);
        }
    }
    let path = section.doc_key.doc_path();
    let path = match path.rsplit_once('.') {
        Some((stem, extension)) if !extension.contains('/') => stem,
        _ => path,
    };
    let split = split_code(&section.text);
    [
        (own_heading, weights.title_weight),
        (headings.join("\n"), weights.heading_weight),
        (section.symbols.join("\n"), weights.symbol_weight),
        (split.prose, weights.body_weight),
        (split.code, weights.code_weight),
        (path.to_owned(), weights.path_weight),
    ]
    .into_iter()
    .filter(|(text, weight)| *weight > 0 && !text.is_empty())
    .collect()
}

impl Fields {
    /// The index document of `section`, which belongs to `scope`.
    #[must_use]
    pub fn section_document(&self, scope: &Scope, section: &Section, weights: &IndexSettings) -> TantivyDocument {
        let mut doc = TantivyDocument::new();
        doc.add_text(self.kind, KIND_SECTION);
        doc.add_text(self.key, section.chunk_id.as_str());
        doc.add_text(self.scope, scope.to_string());
        doc.add_text(self.doc_key, section.doc_key.as_str());
        doc.add_text(self.chunk_id, section.chunk_id.as_str());
        for (text, weight) in combined_parts(section, weights) {
            for _ in 0..weight {
                doc.add_text(self.combined, &text);
            }
        }
        for symbol in &section.symbols {
            doc.add_text(self.symbols_exact, symbol);
        }
        for language in &section.code_languages {
            doc.add_text(self.lang, language.to_lowercase());
        }
        doc.add_text(self.payload, to_json(section));
        doc
    }

    /// The index document of a document's change-detection state.
    #[must_use]
    pub fn doc_state_document(&self, scope: &Scope, state: &DocState) -> TantivyDocument {
        let mut doc = TantivyDocument::new();
        doc.add_text(self.kind, KIND_DOC_STATE);
        doc.add_text(self.key, doc_state_key(&state.doc_key));
        doc.add_text(self.scope, scope.to_string());
        doc.add_text(self.doc_key, state.doc_key.as_str());
        doc.add_text(self.payload, to_json(state));
        doc
    }

    /// The index document of a version's sync record.
    #[must_use]
    pub fn version_state_document(&self, state: &VersionState) -> TantivyDocument {
        let mut doc = TantivyDocument::new();
        doc.add_text(self.kind, KIND_VERSION_STATE);
        doc.add_text(self.key, version_state_key(&state.scope));
        doc.add_text(self.scope, state.scope.to_string());
        doc.add_text(self.payload, to_json(state));
        doc
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use tantivy::schema::Value;
    use zenodotus_core::ids::{ChunkId, LibraryId, Version};
    use zenodotus_core::model::{Outcome, content_hash};
    use zenodotus_core::time::Timestamp;

    use super::*;

    pub(crate) fn scope(library: &str, version: &str) -> Scope {
        Scope::new(LibraryId::parse(library).unwrap(), Version::parse(version).unwrap())
    }

    pub(crate) fn section(scope: &Scope, doc_path: &str, ordinal: u32, headings: &[&str], text: &str) -> Section {
        Section {
            chunk_id: ChunkId::derive(scope, doc_path, ordinal),
            doc_key: DocKey::new(scope, doc_path).unwrap(),
            ordinal,
            title: headings.first().copied().unwrap_or("Untitled").to_owned(),
            heading_path: headings.iter().map(|&h| h.to_owned()).collect(),
            text: text.to_owned(),
            code_languages: Vec::new(),
            symbols: Vec::new(),
            source_ref: format!("local:{doc_path}"),
            content_hash: content_hash(text),
        }
    }

    pub(crate) fn version_state(scope: &Scope) -> VersionState {
        VersionState {
            scope: scope.clone(),
            name: scope.library.to_string(),
            description: String::new(),
            aliases: Vec::new(),
            homepage: None,
            license: None,
            source: "local:docs".to_owned(),
            default: false,
            list_order: 0,
            revision: 0,
            last_success: None,
            last_attempt: Timestamp::from_unix(0),
            last_verified: None,
            outcome: Outcome::Ok,
            last_error: None,
            document_count: 0,
            section_count: 0,
            resolved_commit: None,
        }
    }

    fn texts(doc: &TantivyDocument, field: Field) -> Vec<String> {
        doc.get_all(field)
            .map(|value| value.as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn the_schema_is_stable() {
        let (first, fields) = build_schema();
        let (second, again) = build_schema();
        assert_eq!(first, second);
        assert_eq!(fields, again);
        let names: Vec<&str> = first.fields().map(|(_, entry)| entry.name()).collect();
        assert_eq!(
            names,
            [
                "kind",
                "key",
                "scope",
                "doc_key",
                "chunk_id",
                "combined",
                "symbols_exact",
                "lang",
                "payload"
            ]
        );
        assert!(first.get_field_entry(fields.chunk_id).is_fast());
        assert!(first.get_field_entry(fields.payload).is_stored());
        assert!(!first.get_field_entry(fields.combined).is_stored());
    }

    #[test]
    fn keys_are_prefixed_by_kind() {
        let scope = scope("a", "1");
        assert_eq!(version_state_key(&scope), "vs\u{1f}a@1");
        let key = DocKey::new(&scope, "x.md").unwrap();
        assert_eq!(doc_state_key(&key), "ds\u{1f}a\u{1f}1\u{1f}x.md");
    }

    #[test]
    fn combined_parts_weigh_each_part_of_a_section() {
        let scope = scope("a", "1");
        let mut s = section(
            &scope,
            "guide/setup.md",
            1,
            &["Guide", "Install", "Linux"],
            "## Linux\n\nRun it.\n\n```sh\napt install x\n```\n",
        );
        s.symbols = vec!["apt_get".to_owned(), "x::y".to_owned()];
        let weights = IndexSettings::default();
        assert_eq!(
            combined_parts(&s, &weights),
            [
                ("Linux".to_owned(), 3),
                ("Guide\nInstall".to_owned(), 2),
                ("apt_get\nx::y".to_owned(), 4),
                ("## Linux\n\nRun it.\n\n".to_owned(), 1),
                ("apt install x\n".to_owned(), 1),
                ("guide/setup".to_owned(), 1),
            ]
        );
    }

    #[test]
    fn combined_parts_skip_empty_parts_and_zero_weights() {
        let scope = scope("a", "1");
        let mut s = section(&scope, "notes", 0, &[], "Body\n");
        s.title = "Notes".to_owned();
        let weights = IndexSettings {
            path_weight: 0,
            ..IndexSettings::default()
        };
        assert_eq!(
            combined_parts(&s, &weights),
            [("Notes".to_owned(), 3), ("Body\n".to_owned(), 1)]
        );
    }

    #[test]
    fn combined_parts_list_each_ancestor_heading_once() {
        let scope = scope("a", "1");
        let mut s = section(&scope, "v1.2/page", 0, &["Doc", "Doc", "Part"], "x");
        s.title = "Doc".to_owned();
        let parts = combined_parts(&s, &IndexSettings::default());
        assert_eq!(parts[1], ("Doc".to_owned(), 2));
        assert_eq!(parts[3], ("v1.2/page".to_owned(), 1));
    }

    #[test]
    fn section_documents_carry_every_field() {
        let (_, fields) = build_schema();
        let scope = scope("lib", "2.0");
        let mut s = section(&scope, "a.md", 0, &["A"], "Text\n");
        s.code_languages = vec!["Rust".to_owned(), "sh".to_owned()];
        s.symbols = vec!["tokio::spawn".to_owned()];
        let weights = IndexSettings::default();
        let doc = fields.section_document(&scope, &s, &weights);
        assert_eq!(texts(&doc, fields.kind), ["section"]);
        assert_eq!(texts(&doc, fields.key), [s.chunk_id.as_str()]);
        assert_eq!(texts(&doc, fields.scope), ["lib@2.0"]);
        assert_eq!(texts(&doc, fields.doc_key), [s.doc_key.as_str()]);
        assert_eq!(texts(&doc, fields.chunk_id), [s.chunk_id.as_str()]);
        assert_eq!(texts(&doc, fields.symbols_exact), ["tokio::spawn"]);
        assert_eq!(texts(&doc, fields.lang), ["rust", "sh"]);
        let combined = texts(&doc, fields.combined);
        assert_eq!(combined.iter().filter(|text| *text == "A").count(), 3);
        assert_eq!(combined.iter().filter(|text| *text == "tokio::spawn").count(), 4);
        assert_eq!(combined.len(), 3 + 4 + 1 + 1);
        let payload: Section = serde_json::from_str(&texts(&doc, fields.payload)[0]).unwrap();
        assert_eq!(payload, s);
    }

    #[test]
    fn metadata_documents_carry_their_payload() {
        let (_, fields) = build_schema();
        let scope = scope("lib", "2.0");
        let state = DocState {
            doc_key: DocKey::new(&scope, "a.md").unwrap(),
            etag: Some("e".to_owned()),
            last_modified: None,
            content_hash: content_hash("x"),
            fetched_at: Timestamp::from_unix(5),
        };
        let doc = fields.doc_state_document(&scope, &state);
        assert_eq!(texts(&doc, fields.kind), ["doc_state"]);
        assert_eq!(texts(&doc, fields.key), [doc_state_key(&state.doc_key)]);
        assert_eq!(texts(&doc, fields.scope), ["lib@2.0"]);
        assert_eq!(texts(&doc, fields.doc_key), [state.doc_key.as_str()]);
        let payload: DocState = serde_json::from_str(&texts(&doc, fields.payload)[0]).unwrap();
        assert_eq!(payload, state);

        let record = version_state(&scope);
        let doc = fields.version_state_document(&record);
        assert_eq!(texts(&doc, fields.kind), ["version_state"]);
        assert_eq!(texts(&doc, fields.key), ["vs\u{1f}lib@2.0"]);
        assert_eq!(texts(&doc, fields.scope), ["lib@2.0"]);
        assert_eq!(texts(&doc, fields.doc_key), [] as [std::string::String; 0]);
        let payload: VersionState = serde_json::from_str(&texts(&doc, fields.payload)[0]).unwrap();
        assert_eq!(payload, record);
    }
}

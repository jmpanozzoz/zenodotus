//! Builds a schema-1 index for tests, with scope records written as v1 serialized them (plan §1.5, §3.11).

use std::fs;
use std::path::Path;

use tantivy::{Index, IndexWriter, TantivyDocument};
use zenodotus_core::config::IndexSettings;

use crate::analyzer;
use crate::error::SearchError;
use crate::schema::{KIND_VERSION_STATE, build_schema};
use crate::store::INDEX_DIR;

/// The schema version a v1 binary wrote into every commit.
pub const V1_SCHEMA_VERSION: u32 = 1;

/// Creates the live index under `data_dir` with one `version_state` document per JSON record of `records`, and
/// commits it with v1's commit payload. The field layout is v1's, which v2 kept.
///
/// # Errors
/// An index or I/O error, or a record that is not JSON.
///
/// # Panics
/// When a record has no `scope` object with `library` and `version` strings.
pub fn build(data_dir: &Path, settings: &IndexSettings, records: &[&str]) -> Result<(), SearchError> {
    let path = data_dir.join(INDEX_DIR);
    fs::create_dir_all(&path).map_err(SearchError::io(&path))?;
    let (schema, fields) = build_schema();
    let index = Index::create_in_dir(&path, schema)?;
    analyzer::register(&index);
    let mut writer: IndexWriter = index.writer_with_num_threads(1, 15_000_000)?;
    for record in records {
        let value: serde_json::Value = serde_json::from_str(record)?;
        let part = |name: &str| {
            value["scope"][name]
                .as_str()
                .expect("a v1 record has its scope")
                .to_owned()
        };
        let scope = format!("{}@{}", part("library"), part("version"));
        let mut doc = TantivyDocument::new();
        doc.add_text(fields.kind, KIND_VERSION_STATE);
        doc.add_text(fields.key, format!("vs\u{1f}{scope}"));
        doc.add_text(fields.scope, &scope);
        doc.add_text(fields.payload, record);
        writer.add_document(doc)?;
    }
    let mut prepared = writer.prepare_commit()?;
    prepared.set_payload(&format!(
        r#"{{"schema":{V1_SCHEMA_VERSION},"index_settings":"{}"}}"#,
        settings.hash()
    ));
    prepared.commit()?;
    Ok(writer.wait_merging_threads()?)
}

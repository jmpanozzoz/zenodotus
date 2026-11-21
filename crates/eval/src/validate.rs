//! `validate`: with the real sectioner, every question must match at least one section of its library version, and
//! at most 5% of them, so that it neither asks for something absent nor for something everywhere.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use zenodotus_search::store::Store;

use crate::corpus::Corpus;
use crate::error::EvalError;
use crate::judge::Judge;
use crate::questions::Question;

/// The largest share of a version's sections, in percent, that a question may match.
pub const MAX_SHARE_PERCENT: usize = 5;

/// A question that failed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// The question.
    pub id: String,
    /// Why it was rejected.
    pub reason: String,
}

/// Checks `questions` against the sections of the index synced from the corpus, and returns the rejected ones.
///
/// # Errors
///
/// A search error when the index cannot be read, [`EvalError::DuplicateId`], and [`EvalError::UnknownScope`].
pub fn validate(corpus: &Corpus, questions: &[Question]) -> Result<Vec<Rejection>, EvalError> {
    let store = Store::open(&corpus.data_dir, &corpus.index)?;
    let mut ids = BTreeSet::new();
    let mut texts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut rejections = Vec::new();
    for question in questions {
        if !ids.insert(question.id.as_str()) {
            return Err(EvalError::DuplicateId(question.id.clone()));
        }
        let case = corpus.scope_of(question)?;
        let sections = match texts.entry(case.key.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let sections = store.sections(&case.scope)?;
                entry.insert(sections.into_iter().map(|section| section.text).collect())
            }
        };
        let judge = Judge::new(question);
        let matches = sections.iter().filter(|text| judge.is_hit(text)).count();
        let total = sections.len();
        let reason = if matches == 0 {
            format!("matches none of the {total} sections of {}", case.key)
        } else if matches * 100 > total * MAX_SHARE_PERCENT {
            format!(
                "matches {matches} of the {total} sections of {}, more than {MAX_SHARE_PERCENT}%",
                case.key
            )
        } else {
            continue;
        };
        rejections.push(Rejection {
            id: question.id.clone(),
            reason,
        });
    }
    Ok(rejections)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use zenodotus_core::config::IndexSettings;
    use zenodotus_core::ids::{ChunkId, DocKey, Scope};
    use zenodotus_core::model::{DocState, Document, Outcome, Section, VersionChanges, VersionState, content_hash};
    use zenodotus_core::time::Timestamp;
    use zenodotus_search::error::SearchError;
    use zenodotus_search::store::StoreWriter;

    use super::*;
    use crate::corpus::tests::corpus;
    use crate::questions::tests::question;

    fn version_state(scope: &Scope) -> VersionState {
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

    /// One document per text, each with one section. A text that starts with `# ` gives its first line as the title,
    /// otherwise the title is the document path; the words with `::` are the section's symbols.
    fn documents(scope: &Scope, texts: &[String]) -> Vec<Document> {
        texts
            .iter()
            .enumerate()
            .map(|(n, text)| {
                let doc_path = format!("doc{n}.md");
                Document {
                    state: DocState {
                        doc_key: DocKey::new(scope, &doc_path).unwrap(),
                        etag: None,
                        last_modified: None,
                        content_hash: content_hash(text),
                        fetched_at: Timestamp::from_unix(1),
                    },
                    sections: vec![Section {
                        chunk_id: ChunkId::derive(scope, &doc_path, 0),
                        doc_key: DocKey::new(scope, &doc_path).unwrap(),
                        ordinal: 0,
                        title: text
                            .lines()
                            .next()
                            .and_then(|line| line.strip_prefix("# "))
                            .unwrap_or(&doc_path)
                            .to_owned(),
                        heading_path: Vec::new(),
                        text: text.clone(),
                        code_languages: Vec::new(),
                        symbols: text
                            .split_whitespace()
                            .filter(|word| word.contains("::"))
                            .map(str::to_owned)
                            .collect(),
                        source_ref: format!("local:{doc_path}"),
                        content_hash: content_hash(text),
                    }],
                }
            })
            .collect()
    }

    /// Indexes `texts` as the sections of `scope` under `data_dir`.
    pub(crate) fn index(data_dir: &Path, settings: &IndexSettings, scope: &Scope, texts: &[String]) {
        let mut writer = StoreWriter::open(data_dir, settings).unwrap();
        let changes = VersionChanges {
            upserts: documents(scope, texts),
            ..VersionChanges::default()
        };
        writer.apply(&version_state(scope), &changes).unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn questions_must_match_some_but_not_many_sections() {
        let (_root, corpus) = corpus();
        let mut texts: Vec<String> = (0..40).map(|n| format!("Section {n} about runtimes.")).collect();
        texts.push("Use tokio::spawn to start a task.".to_owned());
        texts.push("Use tokio::spawn and JoinHandle.".to_owned());
        texts.push("JoinHandle alone.".to_owned());
        let tokio = corpus.scopes[0].scope.clone();
        index(&corpus.data_dir, &corpus.index, &tokio, &texts);
        let questions = [
            question("rare", "tokio", None, &["tokio::spawn"]),
            question("absent", "tokio", None, &["tokio::select!"]),
            question("common", "tokio", None, &["about runtimes"]),
            question("edge", "tokio", None, &["Use tokio::spawn", "JoinHandle"]),
            question("empty", "pydantic", Some("1.10"), &["BaseModel"]),
        ];
        let rejections = validate(&corpus, &questions).unwrap();
        let found: Vec<(&str, &str)> = rejections
            .iter()
            .map(|rejection| (rejection.id.as_str(), rejection.reason.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                ("absent", "matches none of the 43 sections of tokio"),
                ("common", "matches 40 of the 43 sections of tokio, more than 5%"),
                ("empty", "matches none of the 0 sections of pydantic@1.10"),
            ]
        );
    }

    #[test]
    fn duplicates_unknown_versions_and_a_missing_index_stop_validation() {
        let (_root, corpus) = corpus();
        let error = validate(&corpus, &[]).unwrap_err();
        assert!(matches!(error, EvalError::Search(SearchError::IndexEmpty)), "{error}");
        index(&corpus.data_dir, &corpus.index, &corpus.scopes[0].scope, &[]);
        let twice = [
            question("q", "tokio", None, &["x"]),
            question("q", "tokio", None, &["x"]),
        ];
        let error = validate(&corpus, &twice).unwrap_err();
        assert!(matches!(&error, EvalError::DuplicateId(id) if id == "q"), "{error}");
        let error = validate(&corpus, &[question("q", "vite", None, &["x"])]).unwrap_err();
        assert!(matches!(error, EvalError::UnknownScope { .. }), "{error}");
    }
}

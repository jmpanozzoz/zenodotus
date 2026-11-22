//! The document sink of a job: it streams what a collection run finds straight into the store's writer, one document
//! at a time, and commits the scope once (design §12.4). The holder's loop of §12.1 builds on it in later phases.

use zenodotus_core::ids::{DocKey, Scope};
use zenodotus_core::model::{DocError, Document, Outcome, VersionState};
use zenodotus_ingest::sink::{DocumentSink, Flow};
use zenodotus_search::SearchError;
use zenodotus_search::store::StoreWriter;

use crate::JobsError;

/// A [`DocumentSink`] that writes into an open stream of [`StoreWriter`]. It keeps only the document errors' count,
/// the first of them and the resolved commit, so memory holds one document at a time.
///
/// The first store error is held and every later call answers [`Flow::Stop`]; [`StoreSink::commit`] then rolls the
/// stream back and returns that error.
pub struct StoreSink<'w> {
    writer: &'w mut StoreWriter,
    previous: Option<VersionState>,
    failed_documents: usize,
    first_error: Option<DocError>,
    resolved_commit: Option<String>,
    failure: Option<SearchError>,
}

impl<'w> StoreSink<'w> {
    /// Reads the stored record of `scope` and opens a stream of it on `writer`.
    ///
    /// # Errors
    ///
    /// The stored record cannot be read; no stream is opened then.
    ///
    /// # Panics
    ///
    /// When `writer` already has an open stream.
    pub fn begin(writer: &'w mut StoreWriter, scope: &Scope) -> Result<Self, JobsError> {
        let previous = writer.store().version_state(scope)?;
        writer.begin(scope);
        Ok(Self {
            writer,
            previous,
            failed_documents: 0,
            first_error: None,
            resolved_commit: None,
            failure: None,
        })
    }

    /// Commits the stream with the scope record of `snapshot`, whose metadata and `last_attempt` it keeps, and
    /// returns the stored record. The status follows v1's `apply`: `partial` with the first document error when any
    /// document failed, `last_verified` only when none did, and the previous resolved commit when none was found.
    ///
    /// # Errors
    ///
    /// The held store error, or an index error of the commit; the stream is rolled back then.
    pub fn commit(self, snapshot: &VersionState) -> Result<VersionState, JobsError> {
        if let Some(failure) = self.failure {
            self.writer.rollback()?;
            return Err(failure.into());
        }
        let previous = self.previous.as_ref();
        let mut state = snapshot.clone();
        state.last_success = Some(snapshot.last_attempt);
        if let Some(first) = &self.first_error {
            state.outcome = Outcome::Partial;
            state.last_verified = previous.and_then(|p| p.last_verified);
            state.last_error = Some(format!("{} document(s) failed; first: {first}", self.failed_documents));
        } else {
            state.outcome = Outcome::Ok;
            state.last_verified = Some(snapshot.last_attempt);
            state.last_error = None;
        }
        state.resolved_commit = self
            .resolved_commit
            .or_else(|| previous.and_then(|p| p.resolved_commit.clone()));
        Ok(self.writer.commit_scope(&state)?)
    }

    /// Runs `write` unless a store error is held, and holds the error it returns.
    fn write(&mut self, write: impl FnOnce(&mut StoreWriter) -> Result<(), SearchError>) -> Flow {
        if self.failure.is_some() {
            return Flow::Stop;
        }
        match write(self.writer) {
            Ok(()) => Flow::Continue,
            Err(error) => {
                self.failure = Some(error);
                Flow::Stop
            }
        }
    }
}

impl DocumentSink for StoreSink<'_> {
    fn upsert(&mut self, document: Document) -> Flow {
        self.write(|writer| writer.add(&document))
    }

    fn unchanged(&mut self, key: DocKey) -> Flow {
        self.write(|writer| writer.keep(&key))
    }

    fn delete(&mut self, key: DocKey) -> Flow {
        self.write(|writer| writer.delete(&key))
    }

    fn error(&mut self, error: DocError) -> Flow {
        self.failed_documents += 1;
        self.first_error.get_or_insert(error);
        Flow::Continue
    }

    fn resolved_commit(&mut self, commit: String) -> Flow {
        self.resolved_commit = Some(commit);
        Flow::Continue
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use zenodotus_core::config::IndexSettings;
    use zenodotus_core::ids::{ChunkId, LibraryId, Version};
    use zenodotus_core::model::{DocState, Origin, Section, VersionChanges, content_hash};
    use zenodotus_core::time::Timestamp;
    use zenodotus_ingest::sink::replay;
    use zenodotus_search::store::Store;

    use super::*;

    fn scope(library: &str) -> Scope {
        Scope::new(LibraryId::parse(library).unwrap(), Version::parse("1").unwrap())
    }

    fn document(scope: &Scope, doc_path: &str, texts: &[&str]) -> Document {
        let sections = texts
            .iter()
            .zip(0..)
            .map(|(text, ordinal)| Section {
                chunk_id: ChunkId::derive(scope, doc_path, ordinal),
                doc_key: DocKey::new(scope, doc_path).unwrap(),
                ordinal,
                title: "Doc".to_owned(),
                heading_path: vec!["Doc".to_owned()],
                text: (*text).to_owned(),
                code_languages: Vec::new(),
                symbols: Vec::new(),
                source_ref: format!("local:{doc_path}"),
                content_hash: content_hash(text),
            })
            .collect();
        Document {
            state: DocState {
                doc_key: DocKey::new(scope, doc_path).unwrap(),
                etag: None,
                last_modified: None,
                content_hash: content_hash(&texts.concat()),
                fetched_at: Timestamp::from_unix(1),
                source: None,
            },
            sections,
        }
    }

    fn snapshot(scope: &Scope, at: i64) -> VersionState {
        VersionState {
            scope: scope.clone(),
            name: scope.library.to_string(),
            description: String::new(),
            aliases: Vec::new(),
            homepage: None,
            license: None,
            source: "local:docs".to_owned(),
            default: true,
            list_order: 0,
            revision: 0,
            last_success: None,
            last_attempt: Timestamp::from_unix(at),
            last_verified: None,
            outcome: Outcome::Ok,
            last_error: None,
            document_count: 0,
            section_count: 0,
            resolved_commit: None,
            origin: Origin::Config,
            plan: None,
            bytes: 0,
        }
    }

    fn doc_error(doc_path: &str) -> DocError {
        DocError {
            doc_path: doc_path.to_owned(),
            kind: "status".to_owned(),
            message: "HTTP 503".to_owned(),
        }
    }

    fn texts(store: &Store, scope: &Scope) -> Vec<String> {
        let mut texts: Vec<String> = store
            .sections(scope)
            .unwrap()
            .into_iter()
            .map(|section| section.text)
            .collect();
        texts.sort();
        texts
    }

    /// Streams `changes` through a [`StoreSink`].
    fn stream(writer: &mut StoreWriter, snapshot: &VersionState, changes: VersionChanges) -> VersionState {
        let mut sink = StoreSink::begin(writer, &snapshot.scope).unwrap();
        assert_eq!(replay(changes, &mut sink), Flow::Continue);
        sink.commit(snapshot).unwrap()
    }

    #[test]
    fn streamed_runs_store_what_apply_stores() {
        let a = scope("lib");
        let runs = [
            VersionChanges {
                upserts: vec![document(&a, "x.md", &["one", "two"]), document(&a, "y.md", &["three"])],
                resolved_commit: Some("c1".to_owned()),
                ..VersionChanges::default()
            },
            VersionChanges {
                upserts: vec![document(&a, "z.md", &["four"])],
                unchanged: vec![DocKey::new(&a, "x.md").unwrap()],
                deletions: vec![DocKey::new(&a, "y.md").unwrap()],
                errors: vec![doc_error("w.md"), doc_error("v.md")],
                resolved_commit: None,
            },
            VersionChanges::default(),
        ];
        let (applied_dir, streamed_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let mut applied = StoreWriter::open(applied_dir.path(), &IndexSettings::default()).unwrap();
        let mut streamed = StoreWriter::open(streamed_dir.path(), &IndexSettings::default()).unwrap();
        let mut states = Vec::new();
        for (at, changes) in (1..).zip(runs) {
            let expected = applied.apply(&snapshot(&a, at), &changes).unwrap();
            let actual = stream(&mut streamed, &snapshot(&a, at), changes);
            assert_eq!(actual, expected);
            assert_eq!(streamed.store().version_state(&a).unwrap(), Some(expected));
            assert_eq!(texts(streamed.store(), &a), texts(applied.store(), &a));
            states.push(actual);
        }
        assert_eq!(states[1].outcome, Outcome::Partial);
        assert_eq!(
            states[1].last_error.as_deref(),
            Some("2 document(s) failed; first: w.md: HTTP 503 (status)")
        );
        assert_eq!(states[1].resolved_commit.as_deref(), Some("c1"));
        assert_eq!(
            (states[2].outcome, states[2].last_error.as_deref()),
            (Outcome::Ok, None)
        );
    }

    #[test]
    fn a_store_error_stops_the_run_and_the_commit_returns_it() {
        let (a, b) = (scope("lib"), scope("other"));
        let dir = TempDir::new().unwrap();
        let mut writer = StoreWriter::open(dir.path(), &IndexSettings::default()).unwrap();
        let before = stream(
            &mut writer,
            &snapshot(&a, 1),
            VersionChanges {
                upserts: vec![document(&a, "x.md", &["original"])],
                ..VersionChanges::default()
            },
        );
        let foreign = DocKey::new(&b, "y.md").unwrap();
        let calls: [fn(&mut StoreSink<'_>, &Scope, &DocKey) -> Flow; 3] = [
            |sink, b, _| sink.upsert(document(b, "y.md", &["stray"])),
            |sink, _, key| sink.unchanged(key.clone()),
            |sink, _, key| sink.delete(key.clone()),
        ];
        for call in calls {
            let mut sink = StoreSink::begin(&mut writer, &a).unwrap();
            assert_eq!(sink.upsert(document(&a, "x.md", &["replaced"])), Flow::Continue);
            assert_eq!(call(&mut sink, &b, &foreign), Flow::Stop);
            assert_eq!(sink.delete(DocKey::new(&a, "x.md").unwrap()), Flow::Stop);
            let error = sink.commit(&snapshot(&a, 2)).unwrap_err();
            assert!(
                matches!(&error, JobsError::Search(SearchError::ForeignDocument { .. })),
                "{error}"
            );
            assert_eq!(error.to_string(), "document other/1/y.md does not belong to lib@1");
        }
        assert_eq!(writer.store().version_state(&a).unwrap(), Some(before));
        assert_eq!(texts(writer.store(), &a), ["original"]);
        let after = stream(&mut writer, &snapshot(&a, 3), VersionChanges::default());
        assert_eq!((after.revision, after.document_count), (1, 1));
    }
}

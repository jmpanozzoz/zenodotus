//! The Tantivy store: one index for every library version, holding sections and the state needed for incremental
//! syncs. [`Store`] only reads; [`StoreWriter`] is the single writer, and applies one version per commit.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tantivy::collector::{Count, DocSetCollector};
use tantivy::indexer::LogMergePolicy;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery, TermSetQuery};
use tantivy::schema::{Field, IndexRecordOption, Value};
use tantivy::{
    DocAddress, Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, TantivyError, Term,
};
use zenodotus_core::config::IndexSettings;
use zenodotus_core::ids::{DocKey, Scope};
use zenodotus_core::model::{DocState, Outcome, Section, VersionChanges, VersionState};

use crate::analyzer;
use crate::error::SearchError;
use crate::schema::{
    Fields, KIND_DOC_STATE, KIND_SECTION, KIND_VERSION_STATE, SCHEMA_VERSION, build_schema, version_state_key,
};

/// The live index, under the data directory.
pub const INDEX_DIR: &str = "index";
/// Where a rebuild writes before it replaces the live index.
pub const NEW_INDEX_DIR: &str = "index.new";
/// Where the live index waits while a rebuilt one takes its place.
pub const OLD_INDEX_DIR: &str = "index.old";

/// Memory the index writer may use before it flushes a segment.
const WRITER_MEMORY_BYTES: usize = 64 << 20;
/// Fraction of deleted documents above which a segment is merged.
const DELETED_RATIO_BEFORE_MERGE: f32 = 0.1;

/// What every commit records, and what an index must record to be opened.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CommitPayload {
    schema: u32,
    index_settings: String,
}

impl CommitPayload {
    fn current(settings: &IndexSettings) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            index_settings: settings.hash(),
        }
    }

    fn describe(&self) -> String {
        format!("schema {} and index settings {}", self.schema, self.index_settings)
    }
}

/// Read access to the index. Each call takes one searcher, so it reads one consistent snapshot.
pub struct Store {
    index: Index,
    fields: Fields,
    reader: IndexReader,
}

impl Store {
    /// Opens the index under `data_dir` for reading. New commits are picked up after a short delay.
    ///
    /// # Errors
    ///
    /// [`SearchError::IndexEmpty`] when nothing was ever committed, [`SearchError::SchemaMismatch`] when the index
    /// was built by another schema version or other `[settings.index]` values, or an index or I/O error.
    pub fn open(data_dir: &Path, settings: &IndexSettings) -> Result<Self, SearchError> {
        // Readers never move directories: after a crash between a swap's renames, they read `index.old/`, and only
        // the writer, which holds the sync lock, repairs the layout.
        let path = [INDEX_DIR, OLD_INDEX_DIR]
            .map(|dir| data_dir.join(dir))
            .into_iter()
            .find(|path| path.join("meta.json").is_file())
            .ok_or(SearchError::IndexEmpty)?;
        let index = Index::open_in_dir(&path)?;
        let payload = check(&index, settings)?;
        if payload.is_none() {
            return Err(SearchError::IndexEmpty);
        }
        Self::with_reader(index, ReloadPolicy::OnCommitWithDelay)
    }

    fn with_reader(index: Index, policy: ReloadPolicy) -> Result<Self, SearchError> {
        analyzer::register(&index);
        let reader = index.reader_builder().reload_policy(policy).try_into()?;
        Ok(Self {
            fields: build_schema().1,
            index,
            reader,
        })
    }

    /// The fields of the index.
    #[must_use]
    pub const fn fields(&self) -> &Fields {
        &self.fields
    }

    /// A searcher over the latest loaded commit.
    #[must_use]
    pub fn searcher(&self) -> Searcher {
        self.reader.searcher()
    }

    /// Loads the latest commit now instead of waiting for the reload policy.
    ///
    /// # Errors
    ///
    /// An index error when the new commit cannot be loaded; the previous one keeps serving.
    pub fn reload(&self) -> Result<(), SearchError> {
        Ok(self.reader.reload()?)
    }

    /// Every synced version, ordered by scope.
    ///
    /// # Errors
    ///
    /// An index error, or a corrupt payload.
    pub fn version_states(&self) -> Result<Vec<VersionState>, SearchError> {
        self.version_states_at(&self.searcher())
    }

    /// Every synced version in the snapshot of `searcher`, ordered by scope.
    pub(crate) fn version_states_at(&self, searcher: &Searcher) -> Result<Vec<VersionState>, SearchError> {
        let query = TermQuery::new(term(self.fields.kind, KIND_VERSION_STATE), IndexRecordOption::Basic);
        let mut states: Vec<VersionState> = self.payloads(searcher, &query)?;
        states.sort_by(|a, b| a.scope.cmp(&b.scope));
        Ok(states)
    }

    /// The sections of one document in the snapshot of `searcher`, in index order.
    pub(crate) fn sections_of(&self, searcher: &Searcher, doc_key: &DocKey) -> Result<Vec<Section>, SearchError> {
        let query = BooleanQuery::new(vec![
            (Occur::Must, term_query(self.fields.kind, KIND_SECTION)),
            (Occur::Must, term_query(self.fields.doc_key, doc_key.as_str())),
        ]);
        self.payloads(searcher, &query)
    }

    /// The sync record of one version, if it was ever synced.
    ///
    /// # Errors
    ///
    /// An index error, or a corrupt payload.
    pub fn version_state(&self, scope: &Scope) -> Result<Option<VersionState>, SearchError> {
        let query = TermQuery::new(
            term(self.fields.key, &version_state_key(scope)),
            IndexRecordOption::Basic,
        );
        Ok(self.payloads(&self.searcher(), &query)?.pop())
    }

    /// The change-detection state of every document of one version, by document key.
    ///
    /// # Errors
    ///
    /// An index error, or a corrupt payload.
    pub fn doc_states(&self, scope: &Scope) -> Result<BTreeMap<DocKey, DocState>, SearchError> {
        let query = self.kind_in_scope(KIND_DOC_STATE, scope);
        let states: Vec<DocState> = self.payloads(&self.searcher(), &query)?;
        Ok(states.into_iter().map(|state| (state.doc_key.clone(), state)).collect())
    }

    /// Every section of one version, in index order.
    ///
    /// # Errors
    ///
    /// An index error, or a corrupt payload.
    pub fn sections(&self, scope: &Scope) -> Result<Vec<Section>, SearchError> {
        let query = self.kind_in_scope(KIND_SECTION, scope);
        self.payloads(&self.searcher(), &query)
    }

    /// Documents of `kind` in `scope`.
    pub(crate) fn kind_in_scope(&self, kind: &str, scope: &Scope) -> BooleanQuery {
        BooleanQuery::new(vec![
            (Occur::Must, term_query(self.fields.kind, kind)),
            (Occur::Must, term_query(self.fields.scope, &scope.to_string())),
        ])
    }

    /// The payloads of every document matching `query`, in index order.
    pub(crate) fn payloads<T: DeserializeOwned>(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
    ) -> Result<Vec<T>, SearchError> {
        let mut addresses: Vec<DocAddress> = searcher.search(query, &DocSetCollector)?.into_iter().collect();
        addresses.sort_unstable();
        addresses
            .into_iter()
            .map(|address| self.payload(searcher, address))
            .collect()
    }

    /// The payload of one document.
    pub(crate) fn payload<T: DeserializeOwned>(
        &self,
        searcher: &Searcher,
        address: DocAddress,
    ) -> Result<T, SearchError> {
        let doc: TantivyDocument = searcher.doc(address)?;
        let text = doc
            .get_first(self.fields.payload)
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        Ok(serde_json::from_str(text)?)
    }

    /// How many documents of `kind` exist with one of `doc_keys`.
    fn count_for_keys(&self, kind: &str, doc_keys: &[&DocKey]) -> Result<u64, SearchError> {
        let keys = doc_keys.iter().map(|key| term(self.fields.doc_key, key.as_str()));
        let query = BooleanQuery::new(vec![
            (Occur::Must, term_query(self.fields.kind, kind)),
            (Occur::Must, Box::new(TermSetQuery::new(keys))),
        ]);
        Ok(self.searcher().search(&query, &Count)? as u64)
    }
}

/// The term of `text` in `field`.
pub(crate) fn term(field: Field, text: &str) -> Term {
    Term::from_field_text(field, text)
}

/// A boxed query for the term of `text` in `field`.
fn term_query(field: Field, text: &str) -> Box<dyn Query> {
    Box::new(TermQuery::new(term(field, text), IndexRecordOption::Basic))
}

/// Repairs a swap that a crash interrupted: renames `index.old/` back to `index/` when only the old index exists,
/// and deletes `index.old/` when the live index exists beside it. Only the writer calls it.
fn recover(data_dir: &Path) -> Result<(), SearchError> {
    let live = data_dir.join(INDEX_DIR);
    let old = data_dir.join(OLD_INDEX_DIR);
    if live.exists() {
        return remove_dir(&old);
    }
    if old.exists() {
        fs::rename(&old, &live).map_err(SearchError::io(&old))?;
    }
    Ok(())
}

/// Checks the schema and the commit payload of `index`, and returns the payload, or `None` before the first commit.
fn check(index: &Index, settings: &IndexSettings) -> Result<Option<CommitPayload>, SearchError> {
    let expected = CommitPayload::current(settings);
    let mismatch = |found: String| SearchError::SchemaMismatch {
        found,
        expected: expected.describe(),
    };
    if index.schema() != build_schema().0 {
        return Err(mismatch("another field layout".to_owned()));
    }
    let Some(text) = index.load_metas()?.payload else {
        return Ok(None);
    };
    match serde_json::from_str::<CommitPayload>(&text) {
        Ok(found) if found == expected => Ok(Some(found)),
        Ok(found) => Err(mismatch(found.describe())),
        Err(_) => Err(mismatch(format!("an unknown commit payload {text:?}"))),
    }
}

/// Maps a failure to create the index writer: a held lock means another sync is running.
fn writer_error(error: TantivyError) -> SearchError {
    match error {
        TantivyError::LockFailure(..) => SearchError::Locked,
        other => SearchError::Index(other),
    }
}

/// The single writer of the index. Every method that changes the index commits at most once.
pub struct StoreWriter {
    store: Store,
    writer: IndexWriter,
    payload: String,
    weights: IndexSettings,
    pending_prune: Vec<Scope>,
}

impl StoreWriter {
    /// Opens the live index under `data_dir` for writing, creating it when it does not exist.
    ///
    /// # Errors
    ///
    /// [`SearchError::Locked`] when another writer holds the index, [`SearchError::SchemaMismatch`] when the index
    /// was built by another schema version or other `[settings.index]` values, or an index or I/O error.
    pub fn open(data_dir: &Path, settings: &IndexSettings) -> Result<Self, SearchError> {
        recover(data_dir)?;
        Self::open_at(&data_dir.join(INDEX_DIR), settings)
    }

    /// Creates an empty index in `index.new/` under `data_dir`, replacing any left over from an abandoned rebuild.
    /// Finish with [`StoreWriter::finish`], then [`swap_rebuilt`] or [`discard_rebuilt`].
    ///
    /// # Errors
    ///
    /// An index or I/O error.
    pub fn rebuild(data_dir: &Path, settings: &IndexSettings) -> Result<Self, SearchError> {
        discard_rebuilt(data_dir)?;
        Self::open_at(&data_dir.join(NEW_INDEX_DIR), settings)
    }

    fn open_at(path: &Path, settings: &IndexSettings) -> Result<Self, SearchError> {
        let index = if path.join("meta.json").is_file() {
            let index = Index::open_in_dir(path)?;
            check(&index, settings)?;
            index
        } else {
            fs::create_dir_all(path).map_err(SearchError::io(path))?;
            Index::create_in_dir(path, build_schema().0)?
        };
        let store = Store::with_reader(index, ReloadPolicy::Manual)?;
        let writer: IndexWriter = store.index.writer(WRITER_MEMORY_BYTES).map_err(writer_error)?;
        let mut policy = LogMergePolicy::default();
        policy.set_del_docs_ratio_before_merge(DELETED_RATIO_BEFORE_MERGE);
        writer.set_merge_policy(Box::new(policy));
        Ok(Self {
            store,
            writer,
            payload: serde_json::to_string(&CommitPayload::current(settings))?,
            weights: settings.clone(),
            pending_prune: Vec::new(),
        })
    }

    /// Read access to what this writer has committed.
    #[must_use]
    pub const fn store(&self) -> &Store {
        &self.store
    }

    /// Applies one ingestion run of one version in a single commit, and returns the stored sync record.
    ///
    /// `snapshot` gives the library metadata and `last_attempt`; the sync status is derived from the previous
    /// record and from `changes`. `revision` goes up by one when any section changed.
    ///
    /// # Errors
    ///
    /// [`SearchError::ForeignDocument`] when a document belongs to another scope, or an index error. Nothing is
    /// committed then.
    pub fn apply(&mut self, snapshot: &VersionState, changes: &VersionChanges) -> Result<VersionState, SearchError> {
        let scope = &snapshot.scope;
        let mut state = self.carried_state(snapshot)?;
        let touched: Vec<&DocKey> = changes
            .upserts
            .iter()
            .map(|document| &document.state.doc_key)
            .chain(&changes.deletions)
            .collect();
        let removed_documents = self.store.count_for_keys(KIND_DOC_STATE, &touched)?;
        let removed_sections = self.store.count_for_keys(KIND_SECTION, &touched)?;
        let added_sections: usize = changes.upserts.iter().map(|document| document.sections.len()).sum();
        state.document_count = state.document_count.saturating_sub(removed_documents) + changes.upserts.len() as u64;
        state.section_count = state.section_count.saturating_sub(removed_sections) + added_sections as u64;
        state.revision += u64::from(changes.changes_sections());
        state.outcome = changes.outcome();
        state.last_success = Some(snapshot.last_attempt);
        if changes.errors.is_empty() {
            state.last_verified = Some(snapshot.last_attempt);
            state.last_error = None;
        } else {
            state.last_error = Some(format!(
                "{} document(s) failed; first: {}",
                changes.errors.len(),
                changes.errors[0]
            ));
        }
        state.resolved_commit = changes.resolved_commit.clone().or(state.resolved_commit);
        if let Err(error) = self.stage_changes(scope, changes) {
            self.writer.rollback()?;
            return Err(error);
        }
        self.commit_with_state(&state)?;
        Ok(state)
    }

    /// Records a sync of one version that changed nothing, with `outcome: failed` and `error`. Only the version's
    /// sync record is replaced; its sections, counts and revision stay as they were.
    ///
    /// # Errors
    ///
    /// An index error, or a corrupt payload.
    pub fn record_failure(&mut self, snapshot: &VersionState, error: &str) -> Result<VersionState, SearchError> {
        let mut state = self.carried_state(snapshot)?;
        state.outcome = Outcome::Failed;
        state.last_error = Some(error.to_owned());
        self.commit_with_state(&state)?;
        Ok(state)
    }

    /// Marks every synced scope that is not in `keep` for deletion with the next commit, and returns them.
    ///
    /// # Errors
    ///
    /// An index error, or a corrupt payload.
    pub fn prune(&mut self, keep: &[Scope]) -> Result<Vec<Scope>, SearchError> {
        let removed: Vec<Scope> = self
            .store
            .version_states()?
            .into_iter()
            .map(|state| state.scope)
            .filter(|scope| !keep.contains(scope))
            .collect();
        self.pending_prune.extend(removed.iter().cloned());
        Ok(removed)
    }

    /// Commits what is pending, such as scopes marked by [`StoreWriter::prune`], when no version is applied after.
    ///
    /// # Errors
    ///
    /// An index error.
    pub fn commit(&mut self) -> Result<(), SearchError> {
        self.stage_prune();
        self.commit_staged()
    }

    /// Stages a raw index document, to build corrupt indexes in tests.
    #[cfg(test)]
    pub(crate) fn add_raw_for_test(&self, doc: TantivyDocument) {
        self.writer.add_document(doc).unwrap();
    }

    /// Waits for background merges and releases the writer.
    ///
    /// # Errors
    ///
    /// An index error from a merge.
    pub fn finish(self) -> Result<(), SearchError> {
        Ok(self.writer.wait_merging_threads()?)
    }

    /// The previous sync record's status combined with the metadata and `last_attempt` of `snapshot`.
    fn carried_state(&self, snapshot: &VersionState) -> Result<VersionState, SearchError> {
        let mut state = snapshot.clone();
        let previous = self.store.version_state(&snapshot.scope)?;
        state.revision = previous.as_ref().map_or(0, |p| p.revision);
        state.last_success = previous.as_ref().and_then(|p| p.last_success);
        state.last_verified = previous.as_ref().and_then(|p| p.last_verified);
        state.last_error = previous.as_ref().and_then(|p| p.last_error.clone());
        state.document_count = previous.as_ref().map_or(0, |p| p.document_count);
        state.section_count = previous.as_ref().map_or(0, |p| p.section_count);
        state.resolved_commit = previous.and_then(|p| p.resolved_commit);
        Ok(state)
    }

    /// Stages the deletion of every changed or deleted document and the addition of the new ones.
    fn stage_changes(&self, scope: &Scope, changes: &VersionChanges) -> Result<(), SearchError> {
        let fields = self.store.fields;
        let keys = changes
            .upserts
            .iter()
            .map(|document| &document.state.doc_key)
            .chain(&changes.deletions);
        for key in keys {
            if DocKey::new(scope, key.doc_path()).ok().as_ref() != Some(key) {
                return Err(SearchError::ForeignDocument {
                    doc_key: key.as_str().replace('\u{1f}', "/"),
                    scope: scope.to_string(),
                });
            }
            self.writer.delete_term(term(fields.doc_key, key.as_str()));
        }
        for document in &changes.upserts {
            for section in &document.sections {
                self.writer
                    .add_document(fields.section_document(scope, section, &self.weights))?;
            }
            self.writer
                .add_document(fields.doc_state_document(scope, &document.state))?;
        }
        Ok(())
    }

    /// Stages the deletion of every scope marked by [`StoreWriter::prune`].
    fn stage_prune(&self) {
        for scope in &self.pending_prune {
            self.writer
                .delete_term(term(self.store.fields.scope, &scope.to_string()));
        }
    }

    /// Replaces the sync record of `state`'s scope, then commits everything staged.
    fn commit_with_state(&mut self, state: &VersionState) -> Result<(), SearchError> {
        let fields = self.store.fields;
        self.stage_prune();
        self.writer
            .delete_term(term(fields.key, &version_state_key(&state.scope)));
        self.writer.add_document(fields.version_state_document(state))?;
        self.commit_staged()
    }

    /// Commits with the payload and makes the commit visible to [`StoreWriter::store`].
    fn commit_staged(&mut self) -> Result<(), SearchError> {
        let mut prepared = self.writer.prepare_commit()?;
        prepared.set_payload(&self.payload);
        prepared.commit()?;
        self.pending_prune.clear();
        self.store.reload()
    }
}

/// Replaces the live index with the rebuilt one: `index/` becomes `index.old/`, `index.new/` becomes `index/`, and
/// `index.old/` is deleted. A crash between the renames is repaired by the next open.
///
/// # Errors
///
/// An I/O error.
pub fn swap_rebuilt(data_dir: &Path) -> Result<(), SearchError> {
    let live = data_dir.join(INDEX_DIR);
    let old = data_dir.join(OLD_INDEX_DIR);
    let new = data_dir.join(NEW_INDEX_DIR);
    if live.exists() {
        remove_dir(&old)?;
        fs::rename(&live, &old).map_err(SearchError::io(&live))?;
    }
    fs::rename(&new, &live).map_err(SearchError::io(&new))?;
    remove_dir(&old)
}

/// Deletes `index.new/` under `data_dir`, if it exists.
///
/// # Errors
///
/// An I/O error.
pub fn discard_rebuilt(data_dir: &Path) -> Result<(), SearchError> {
    remove_dir(&data_dir.join(NEW_INDEX_DIR))
}

fn remove_dir(path: &Path) -> Result<(), SearchError> {
    if path.exists() {
        fs::remove_dir_all(path).map_err(SearchError::io(path))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use zenodotus_core::model::{DocError, Document, content_hash};
    use zenodotus_core::time::Timestamp;

    use super::*;
    use crate::schema::tests::{scope, section, version_state};

    fn document(scope: &Scope, doc_path: &str, texts: &[&str]) -> Document {
        let sections = texts
            .iter()
            .zip(0..)
            .map(|(text, ordinal)| section(scope, doc_path, ordinal, &["Doc"], text))
            .collect();
        Document {
            state: DocState {
                doc_key: DocKey::new(scope, doc_path).unwrap(),
                etag: None,
                last_modified: None,
                content_hash: content_hash(&texts.concat()),
                fetched_at: Timestamp::from_unix(1),
            },
            sections,
        }
    }

    fn upserts(documents: Vec<Document>) -> VersionChanges {
        VersionChanges {
            upserts: documents,
            listing_complete: true,
            ..VersionChanges::default()
        }
    }

    fn snapshot(scope: &Scope, at: i64) -> VersionState {
        let mut state = version_state(scope);
        state.last_attempt = Timestamp::from_unix(at);
        state
    }

    fn section_texts(store: &Store, scope: &Scope) -> Vec<String> {
        let mut texts: Vec<String> = store
            .sections(scope)
            .unwrap()
            .into_iter()
            .map(|section| section.text)
            .collect();
        texts.sort();
        texts
    }

    fn settings() -> IndexSettings {
        IndexSettings::default()
    }

    #[test]
    fn reading_before_any_commit_reports_an_empty_index() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            Store::open(dir.path(), &settings()),
            Err(SearchError::IndexEmpty)
        ));
        let writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer.finish().unwrap();
        assert!(matches!(
            Store::open(dir.path(), &settings()),
            Err(SearchError::IndexEmpty)
        ));
    }

    #[test]
    fn applied_versions_persist_after_reopening() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        let stored = writer
            .apply(
                &snapshot(&a, 10),
                &upserts(vec![
                    document(&a, "x.md", &["one", "two"]),
                    document(&a, "y.md", &["three"]),
                ]),
            )
            .unwrap();
        assert_eq!(
            (
                stored.revision,
                stored.document_count,
                stored.section_count,
                stored.outcome
            ),
            (1, 2, 3, Outcome::Ok)
        );
        assert_eq!(stored.last_success, Some(Timestamp::from_unix(10)));
        assert_eq!(stored.last_verified, Some(Timestamp::from_unix(10)));
        writer.finish().unwrap();

        let store = Store::open(dir.path(), &settings()).unwrap();
        assert_eq!(store.version_states().unwrap(), std::slice::from_ref(&stored));
        assert_eq!(store.version_state(&a).unwrap(), Some(stored));
        assert_eq!(store.version_state(&scope("lib", "2")).unwrap(), None);
        let states = store.doc_states(&a).unwrap();
        let paths: Vec<&str> = states.keys().map(DocKey::doc_path).collect();
        assert_eq!(paths, ["x.md", "y.md"]);
        assert_eq!(section_texts(&store, &a), ["one", "three", "two"]);
    }

    #[test]
    fn upserts_replace_and_deletions_remove_whole_documents() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer
            .apply(
                &snapshot(&a, 1),
                &upserts(vec![
                    document(&a, "x.md", &["old 1", "old 2"]),
                    document(&a, "y.md", &["gone"]),
                ]),
            )
            .unwrap();
        let changes = VersionChanges {
            deletions: vec![DocKey::new(&a, "y.md").unwrap()],
            ..upserts(vec![document(&a, "x.md", &["new"])])
        };
        let stored = writer.apply(&snapshot(&a, 2), &changes).unwrap();
        assert_eq!(
            (stored.revision, stored.document_count, stored.section_count),
            (2, 1, 1)
        );
        assert_eq!(section_texts(writer.store(), &a), ["new"]);
        assert_eq!(writer.store().doc_states(&a).unwrap().len(), 1);
    }

    #[test]
    fn unchanged_versions_keep_their_revision() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer
            .apply(&snapshot(&a, 1), &upserts(vec![document(&a, "x.md", &["one"])]))
            .unwrap();
        let unchanged = VersionChanges {
            unchanged: vec![DocKey::new(&a, "x.md").unwrap()],
            resolved_commit: Some("abc".to_owned()),
            ..VersionChanges::default()
        };
        let stored = writer.apply(&snapshot(&a, 5), &unchanged).unwrap();
        assert_eq!(
            (stored.revision, stored.document_count, stored.section_count),
            (1, 1, 1)
        );
        assert_eq!(stored.last_verified, Some(Timestamp::from_unix(5)));
        assert_eq!(stored.resolved_commit.as_deref(), Some("abc"));
        let stored = writer.apply(&snapshot(&a, 6), &VersionChanges::default()).unwrap();
        assert_eq!(stored.resolved_commit.as_deref(), Some("abc"));
    }

    #[test]
    fn versions_are_isolated() {
        let dir = TempDir::new().unwrap();
        let (one, two) = (scope("lib", "1"), scope("lib", "2"));
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer
            .apply(&snapshot(&one, 1), &upserts(vec![document(&one, "x.md", &["v1"])]))
            .unwrap();
        writer
            .apply(&snapshot(&two, 1), &upserts(vec![document(&two, "x.md", &["v2"])]))
            .unwrap();
        let changes = VersionChanges {
            deletions: vec![DocKey::new(&one, "x.md").unwrap()],
            ..VersionChanges::default()
        };
        writer.apply(&snapshot(&one, 2), &changes).unwrap();
        assert_eq!(section_texts(writer.store(), &one), [] as [std::string::String; 0]);
        assert_eq!(section_texts(writer.store(), &two), ["v2"]);
        let scopes: Vec<String> = writer
            .store()
            .version_states()
            .unwrap()
            .iter()
            .map(|s| s.scope.to_string())
            .collect();
        assert_eq!(scopes, ["lib@1", "lib@2"]);
    }

    #[test]
    fn partial_syncs_record_the_first_error_and_keep_last_verified() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer
            .apply(&snapshot(&a, 1), &upserts(vec![document(&a, "x.md", &["one"])]))
            .unwrap();
        let error = |path: &str| DocError {
            doc_path: path.to_owned(),
            kind: "status".to_owned(),
            message: "HTTP 500".to_owned(),
        };
        let changes = VersionChanges {
            errors: vec![error("y.md"), error("z.md")],
            ..upserts(vec![document(&a, "w.md", &["w"])])
        };
        let stored = writer.apply(&snapshot(&a, 9), &changes).unwrap();
        assert_eq!(stored.outcome, Outcome::Partial);
        assert_eq!(
            stored.last_error.as_deref(),
            Some("2 document(s) failed; first: y.md: HTTP 500 (status)")
        );
        assert_eq!(stored.last_success, Some(Timestamp::from_unix(9)));
        assert_eq!(stored.last_verified, Some(Timestamp::from_unix(1)));
        let stored = writer.apply(&snapshot(&a, 10), &VersionChanges::default()).unwrap();
        assert_eq!((stored.outcome, stored.last_error), (Outcome::Ok, None));
    }

    #[test]
    fn a_failed_apply_changes_no_section() {
        let dir = TempDir::new().unwrap();
        let (a, b) = (scope("lib", "1"), scope("other", "1"));
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer
            .apply(&snapshot(&a, 1), &upserts(vec![document(&a, "x.md", &["original"])]))
            .unwrap();
        let foreign = upserts(vec![
            document(&a, "x.md", &["replaced"]),
            document(&b, "y.md", &["stray"]),
        ]);
        let error = writer.apply(&snapshot(&a, 2), &foreign).unwrap_err();
        assert!(matches!(&error, SearchError::ForeignDocument { .. }), "{error}");
        assert_eq!(error.to_string(), "document other/1/y.md does not belong to lib@1");
        assert_eq!(section_texts(writer.store(), &a), ["original"]);
        writer
            .apply(&snapshot(&a, 3), &upserts(vec![document(&a, "z.md", &["more"])]))
            .unwrap();
        assert_eq!(section_texts(writer.store(), &a), ["more", "original"]);
        let deletion = VersionChanges {
            deletions: vec![DocKey::new(&b, "y.md").unwrap()],
            ..VersionChanges::default()
        };
        assert!(writer.apply(&snapshot(&a, 4), &deletion).is_err());
    }

    #[test]
    fn a_recorded_failure_only_replaces_the_sync_record() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        let first = writer.record_failure(&snapshot(&a, 1), "listing unavailable").unwrap();
        assert_eq!(
            (first.revision, first.section_count, first.outcome),
            (0, 0, Outcome::Failed)
        );
        assert_eq!(first.last_success, None);
        let ok = writer
            .apply(&snapshot(&a, 2), &upserts(vec![document(&a, "x.md", &["one"])]))
            .unwrap();
        let mut renamed = snapshot(&a, 3);
        renamed.name = "Renamed".to_owned();
        let failed = writer.record_failure(&renamed, "timeout").unwrap();
        assert_eq!(failed.name, "Renamed");
        assert_eq!(failed.last_attempt, Timestamp::from_unix(3));
        assert_eq!(failed.last_error.as_deref(), Some("timeout"));
        assert_eq!(
            (
                failed.revision,
                failed.document_count,
                failed.section_count,
                failed.last_success
            ),
            (ok.revision, ok.document_count, ok.section_count, ok.last_success)
        );
        assert_eq!(section_texts(writer.store(), &a), ["one"]);
    }

    #[test]
    fn pruned_scopes_go_with_the_next_commit() {
        let dir = TempDir::new().unwrap();
        let (a, b, c) = (scope("a", "1"), scope("b", "1"), scope("c", "1"));
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        for s in [&a, &b, &c] {
            writer
                .apply(&snapshot(s, 1), &upserts(vec![document(s, "x.md", &["text"])]))
                .unwrap();
        }
        assert_eq!(writer.prune(&[a.clone(), c.clone()]).unwrap(), std::slice::from_ref(&b));
        assert_eq!(writer.store().version_states().unwrap().len(), 3);
        writer.apply(&snapshot(&a, 2), &VersionChanges::default()).unwrap();
        assert_eq!(section_texts(writer.store(), &b), [] as [std::string::String; 0]);
        assert!(writer.store().doc_states(&b).unwrap().is_empty());
        assert_eq!(writer.store().version_states().unwrap().len(), 2);

        assert_eq!(writer.prune(&[]).unwrap(), [a.clone(), c.clone()]);
        writer.commit().unwrap();
        assert_eq!(
            writer.store().version_states().unwrap(),
            [] as [zenodotus_core::model::VersionState; 0]
        );

        writer
            .apply(&snapshot(&a, 3), &upserts(vec![document(&a, "x.md", &["back"])]))
            .unwrap();
        writer.prune(&[]).unwrap();
        writer
            .apply(&snapshot(&a, 4), &upserts(vec![document(&b, "x.md", &["stray"])]))
            .unwrap_err();
        writer.record_failure(&snapshot(&c, 4), "boom").unwrap();
        assert_eq!(section_texts(writer.store(), &a), [] as [std::string::String; 0]);
    }

    #[test]
    fn a_reader_sees_new_commits_after_a_reload() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer
            .apply(&snapshot(&a, 1), &upserts(vec![document(&a, "x.md", &["one"])]))
            .unwrap();
        let reader = Store::open(dir.path(), &settings()).unwrap();
        writer
            .apply(&snapshot(&a, 2), &upserts(vec![document(&a, "y.md", &["two"])]))
            .unwrap();
        reader.reload().unwrap();
        assert_eq!(section_texts(&reader, &a), ["one", "two"]);
        assert_eq!(reader.fields(), writer.store().fields());
    }

    #[test]
    fn a_second_writer_is_locked_out() {
        let dir = TempDir::new().unwrap();
        let _writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        assert!(matches!(
            StoreWriter::open(dir.path(), &settings()),
            Err(SearchError::Locked)
        ));
        let other = writer_error(TantivyError::InvalidArgument("x".to_owned()));
        assert!(matches!(other, SearchError::Index(_)));
    }

    #[test]
    fn other_index_settings_require_a_rebuild() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer.apply(&snapshot(&a, 1), &VersionChanges::default()).unwrap();
        writer.finish().unwrap();
        let heavier = IndexSettings {
            title_weight: 9,
            ..settings()
        };
        for error in [
            Store::open(dir.path(), &heavier).err().unwrap(),
            StoreWriter::open(dir.path(), &heavier).err().unwrap(),
        ] {
            let message = error.to_string();
            assert!(matches!(error, SearchError::SchemaMismatch { .. }), "{message}");
            assert!(
                message.contains(&settings().hash()) && message.contains(&heavier.hash()),
                "{message}"
            );
        }
    }

    #[test]
    fn foreign_layouts_and_payloads_require_a_rebuild() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(INDEX_DIR);
        fs::create_dir_all(&path).unwrap();
        let index = Index::create_in_dir(&path, build_schema().0).unwrap();
        let mut writer: IndexWriter = index.writer(WRITER_MEMORY_BYTES).unwrap();
        let mut prepared = writer.prepare_commit().unwrap();
        prepared.set_payload("not json");
        prepared.commit().unwrap();
        drop(writer);
        let error = Store::open(dir.path(), &settings()).err().unwrap();
        assert!(
            error.to_string().contains("an unknown commit payload \"not json\""),
            "{error}"
        );

        let other = TempDir::new().unwrap();
        let path = other.path().join(INDEX_DIR);
        fs::create_dir_all(&path).unwrap();
        let mut builder = tantivy::schema::Schema::builder();
        builder.add_text_field("kind", tantivy::schema::STRING);
        Index::create_in_dir(&path, builder.build()).unwrap();
        let error = StoreWriter::open(other.path(), &settings()).err().unwrap();
        assert!(error.to_string().contains("another field layout"), "{error}");
    }

    #[test]
    fn a_rebuild_replaces_the_live_index_only_when_swapped() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer
            .apply(&snapshot(&a, 1), &upserts(vec![document(&a, "x.md", &["live"])]))
            .unwrap();
        writer.finish().unwrap();

        let mut rebuild = StoreWriter::rebuild(dir.path(), &settings()).unwrap();
        rebuild
            .apply(&snapshot(&a, 2), &upserts(vec![document(&a, "x.md", &["abandoned"])]))
            .unwrap();
        rebuild.finish().unwrap();
        discard_rebuilt(dir.path()).unwrap();
        assert!(!dir.path().join(NEW_INDEX_DIR).exists());
        assert_eq!(
            section_texts(&Store::open(dir.path(), &settings()).unwrap(), &a),
            ["live"]
        );

        let mut rebuild = StoreWriter::rebuild(dir.path(), &settings()).unwrap();
        rebuild
            .apply(&snapshot(&a, 3), &upserts(vec![document(&a, "x.md", &["rebuilt"])]))
            .unwrap();
        rebuild.finish().unwrap();
        let mut stale = StoreWriter::rebuild(dir.path(), &settings()).unwrap();
        stale
            .apply(&snapshot(&a, 4), &upserts(vec![document(&a, "x.md", &["latest"])]))
            .unwrap();
        stale.finish().unwrap();
        swap_rebuilt(dir.path()).unwrap();
        assert!(!dir.path().join(OLD_INDEX_DIR).exists());
        assert_eq!(
            section_texts(&Store::open(dir.path(), &settings()).unwrap(), &a),
            ["latest"]
        );
    }

    #[test]
    fn a_swap_into_an_empty_data_directory_needs_no_old_index() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut rebuild = StoreWriter::rebuild(dir.path(), &settings()).unwrap();
        rebuild.apply(&snapshot(&a, 1), &VersionChanges::default()).unwrap();
        rebuild.finish().unwrap();
        swap_rebuilt(dir.path()).unwrap();
        assert_eq!(
            Store::open(dir.path(), &settings())
                .unwrap()
                .version_states()
                .unwrap()
                .len(),
            1
        );
        let error = swap_rebuilt(dir.path()).unwrap_err();
        assert!(matches!(error, SearchError::Io { .. }), "{error}");
    }

    #[test]
    fn a_crash_between_the_renames_is_read_by_readers_and_repaired_by_the_writer() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer.apply(&snapshot(&a, 1), &VersionChanges::default()).unwrap();
        writer.finish().unwrap();
        fs::rename(dir.path().join(INDEX_DIR), dir.path().join(OLD_INDEX_DIR)).unwrap();
        let reader = Store::open(dir.path(), &settings()).unwrap();
        assert_eq!(reader.version_states().unwrap().len(), 1);
        // A reader never moves directories, so it cannot race a swap in progress.
        assert!(!dir.path().join(INDEX_DIR).exists());
        StoreWriter::open(dir.path(), &settings()).unwrap().finish().unwrap();
        assert!(dir.path().join(INDEX_DIR).exists());
        assert!(!dir.path().join(OLD_INDEX_DIR).exists());
    }

    #[test]
    fn an_old_index_left_beside_the_live_one_does_not_block_the_next_swap() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer.apply(&snapshot(&a, 1), &VersionChanges::default()).unwrap();
        writer.finish().unwrap();
        fs::create_dir(dir.path().join(OLD_INDEX_DIR)).unwrap();
        fs::write(dir.path().join(OLD_INDEX_DIR).join("leftover"), "x").unwrap();
        let mut rebuild = StoreWriter::rebuild(dir.path(), &settings()).unwrap();
        rebuild.apply(&snapshot(&a, 2), &VersionChanges::default()).unwrap();
        rebuild.finish().unwrap();
        swap_rebuilt(dir.path()).unwrap();
        assert!(!dir.path().join(OLD_INDEX_DIR).exists());
        fs::create_dir(dir.path().join(OLD_INDEX_DIR)).unwrap();
        StoreWriter::open(dir.path(), &settings()).unwrap().finish().unwrap();
        assert!(!dir.path().join(OLD_INDEX_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn file_system_failures_name_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::create_dir(locked.join(OLD_INDEX_DIR)).unwrap();
        fs::create_dir(locked.join(NEW_INDEX_DIR)).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        let errors = [
            StoreWriter::open(&locked, &settings()).err().unwrap(),
            StoreWriter::rebuild(&locked, &settings()).err().unwrap(),
            StoreWriter::open(&locked.join("sub"), &settings()).err().unwrap(),
        ];
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        for error in errors {
            assert!(matches!(error, SearchError::Io { .. }), "{error}");
        }
    }
}

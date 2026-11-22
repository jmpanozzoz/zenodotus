//! The Tantivy store: one index for every library version, holding sections and the state needed for incremental
//! syncs. [`Store`] only reads; [`StoreWriter`] is the single writer, and applies one version per commit.

use std::collections::{BTreeMap, BTreeSet};
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
use zenodotus_core::model::{DocState, Document, Outcome, Section, VersionChanges, VersionState};

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

/// Memory the index writer may use before it flushes a segment (design §12.8).
const WRITER_MEMORY_BYTES: usize = 32 << 20;
/// Indexing threads of the writer: one, so that its whole heap is one arena (design §12.8).
const WRITER_THREADS: usize = 1;
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
        let path = readable_index(data_dir).ok_or(SearchError::IndexEmpty)?;
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

    /// Every scope record of the index under `data_dir`, read without checking the schema or the index settings, so
    /// that a rebuild can read the records of an index an older version wrote. Records of schema 1 get the defaults
    /// of design §12.5. Empty when there is no index.
    ///
    /// # Errors
    ///
    /// An index error, or a payload that does not decode.
    pub fn read_states_raw(data_dir: &Path) -> Result<Vec<VersionState>, SearchError> {
        let Some(path) = readable_index(data_dir) else {
            return Ok(Vec::new());
        };
        let store = Self::with_reader(Index::open_in_dir(&path)?, ReloadPolicy::Manual)?;
        store.version_states()
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

/// The index a reader opens under `data_dir`: `index/`, or `index.old/` after a crash between a swap's renames.
fn readable_index(data_dir: &Path) -> Option<std::path::PathBuf> {
    [INDEX_DIR, OLD_INDEX_DIR]
        .map(|dir| data_dir.join(dir))
        .into_iter()
        .find(|path| path.join("meta.json").is_file())
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
    stream: Option<Stream>,
}

/// What a stream opened by [`StoreWriter::begin`] has staged so far: only keys and section counts, never documents.
struct Stream {
    scope: Scope,
    /// The section count of every document added and not deleted after.
    added: BTreeMap<DocKey, usize>,
    /// Every key added or deleted, whose stored documents the commit replaces.
    touched: BTreeSet<DocKey>,
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
        let writer: IndexWriter = store
            .index
            .writer_with_num_threads(WRITER_THREADS, WRITER_MEMORY_BYTES)
            .map_err(writer_error)?;
        let mut policy = LogMergePolicy::default();
        policy.set_del_docs_ratio_before_merge(DELETED_RATIO_BEFORE_MERGE);
        writer.set_merge_policy(Box::new(policy));
        Ok(Self {
            store,
            writer,
            payload: serde_json::to_string(&CommitPayload::current(settings))?,
            weights: settings.clone(),
            pending_prune: Vec::new(),
            stream: None,
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
    /// [`SearchError::ForeignDocument`] when a document belongs to another scope, or an index error. Everything
    /// staged is rolled back then, so a later commit cannot carry part of this run. Nothing is committed, unless the
    /// error comes from reloading the readers after a durable commit: the changes are then stored although the call
    /// fails.
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
        let committed = self
            .stage_changes(scope, changes)
            .and_then(|()| self.commit_with_state(&state));
        self.rolled_back(committed)?;
        Ok(state)
    }

    /// Opens a stream of changes to `scope`, staged one document at a time by [`StoreWriter::add`],
    /// [`StoreWriter::keep`] and [`StoreWriter::delete`], and committed once by [`StoreWriter::commit_scope`] or
    /// discarded by [`StoreWriter::rollback`].
    ///
    /// # Panics
    ///
    /// When a stream is already open.
    pub fn begin(&mut self, scope: &Scope) {
        assert!(self.stream.is_none(), "a stream is already open");
        self.stream = Some(Stream {
            scope: scope.clone(),
            added: BTreeMap::new(),
            touched: BTreeSet::new(),
        });
    }

    /// Stages `document` in the open stream, replacing what is stored under its key.
    ///
    /// # Errors
    ///
    /// [`SearchError::ForeignDocument`] when the document belongs to another scope, which stages nothing, or an index
    /// error.
    ///
    /// # Panics
    ///
    /// When no stream is open.
    pub fn add(&mut self, document: &Document) -> Result<(), SearchError> {
        let fields = self.store.fields;
        let stream = open_stream(self.stream.as_mut());
        let key = &document.state.doc_key;
        owned(&stream.scope, key)?;
        self.writer.delete_term(term(fields.doc_key, key.as_str()));
        for section in &document.sections {
            self.writer
                .add_document(fields.section_document(&stream.scope, section, &self.weights))?;
        }
        self.writer
            .add_document(fields.doc_state_document(&stream.scope, &document.state))?;
        stream.added.insert(key.clone(), document.sections.len());
        stream.touched.insert(key.clone());
        Ok(())
    }

    /// Keeps what is stored under `key` in the open stream. Nothing is staged; the key is only checked.
    ///
    /// # Errors
    ///
    /// [`SearchError::ForeignDocument`] when the key belongs to another scope.
    ///
    /// # Panics
    ///
    /// When no stream is open.
    pub fn keep(&mut self, key: &DocKey) -> Result<(), SearchError> {
        owned(&open_stream(self.stream.as_mut()).scope, key)
    }

    /// Stages the deletion of `key` in the open stream, with what an earlier [`StoreWriter::add`] staged under it.
    ///
    /// # Errors
    ///
    /// [`SearchError::ForeignDocument`] when the key belongs to another scope, which stages nothing.
    ///
    /// # Panics
    ///
    /// When no stream is open.
    pub fn delete(&mut self, key: &DocKey) -> Result<(), SearchError> {
        let stream = open_stream(self.stream.as_mut());
        owned(&stream.scope, key)?;
        self.writer.delete_term(term(self.store.fields.doc_key, key.as_str()));
        stream.added.remove(key);
        stream.touched.insert(key.clone());
        Ok(())
    }

    /// Commits the open stream with the scope record `state`, in one commit, closes the stream and returns the
    /// stored record.
    ///
    /// The caller decides the status fields of `state`; the writer derives the counts from what is stored and
    /// streamed, and raises the previous `revision` by one when any document was added or deleted.
    ///
    /// # Errors
    ///
    /// An index error or a corrupt payload. The stream is rolled back then, as in [`StoreWriter::apply`].
    ///
    /// # Panics
    ///
    /// When no stream is open, or when `state` belongs to another scope than the stream.
    pub fn commit_scope(&mut self, state: &VersionState) -> Result<VersionState, SearchError> {
        let stream = open_stream(self.stream.take());
        assert_eq!(
            stream.scope, state.scope,
            "commit_scope for {} in a stream of {}",
            state.scope, stream.scope
        );
        let committed = self
            .counted_state(&stream, state)
            .and_then(|counted| self.commit_with_state(&counted).map(|()| counted));
        if committed.is_err() {
            self.writer.rollback()?;
        }
        committed
    }

    /// Discards the open stream and everything staged since the last commit. Scopes marked by
    /// [`StoreWriter::prune`] stay marked.
    ///
    /// # Errors
    ///
    /// An index error.
    pub fn rollback(&mut self) -> Result<(), SearchError> {
        self.stream = None;
        self.writer.rollback()?;
        Ok(())
    }

    /// `state` with the counts and revision of the stored record changed by `stream`.
    fn counted_state(&self, stream: &Stream, state: &VersionState) -> Result<VersionState, SearchError> {
        let previous = self.store.version_state(&stream.scope)?;
        let touched: Vec<&DocKey> = stream.touched.iter().collect();
        let removed_documents = self.store.count_for_keys(KIND_DOC_STATE, &touched)?;
        let removed_sections = self.store.count_for_keys(KIND_SECTION, &touched)?;
        let added_sections: usize = stream.added.values().sum();
        let mut counted = state.clone();
        counted.revision = previous.as_ref().map_or(0, |p| p.revision) + u64::from(!touched.is_empty());
        counted.document_count = previous
            .as_ref()
            .map_or(0, |p| p.document_count)
            .saturating_sub(removed_documents)
            + stream.added.len() as u64;
        counted.section_count =
            previous.map_or(0, |p| p.section_count).saturating_sub(removed_sections) + added_sections as u64;
        Ok(counted)
    }

    /// Records a sync of one version that changed nothing, with `outcome: failed` and `error`. Only the version's
    /// sync record is replaced; its sections, counts and revision stay as they were.
    ///
    /// # Errors
    ///
    /// An index error, or a corrupt payload. What was staged is rolled back, as in [`StoreWriter::apply`].
    pub fn record_failure(&mut self, snapshot: &VersionState, error: &str) -> Result<VersionState, SearchError> {
        // A failed `apply` may have committed before its reader reload failed; the record must carry what is stored.
        self.store.reload()?;
        let mut state = self.carried_state(snapshot)?;
        state.outcome = Outcome::Failed;
        state.last_error = Some(error.to_owned());
        let committed = self.commit_with_state(&state);
        self.rolled_back(committed)?;
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
        let committed = self.commit_staged();
        self.rolled_back(committed)
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
            owned(scope, key)?;
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

    /// Discards everything staged when `result` is an error, so that a later commit cannot carry part of a failed
    /// one. Marks left by [`StoreWriter::prune`] stay and are staged again by the next commit.
    fn rolled_back(&mut self, result: Result<(), SearchError>) -> Result<(), SearchError> {
        if result.is_err() {
            self.writer.rollback()?;
        }
        result
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

/// The open stream.
///
/// # Panics
///
/// When no stream is open.
fn open_stream<T>(stream: Option<T>) -> T {
    stream.expect("no stream is open; call begin first")
}

/// Checks that `key` belongs to `scope`.
fn owned(scope: &Scope, key: &DocKey) -> Result<(), SearchError> {
    if DocKey::new(scope, key.doc_path()).ok().as_ref() == Some(key) {
        Ok(())
    } else {
        Err(SearchError::ForeignDocument {
            doc_key: key.as_str().replace('\u{1f}', "/"),
            scope: scope.to_string(),
        })
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
    use zenodotus_core::model::{DocError, content_hash};
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
                source: None,
            },
            sections,
        }
    }

    fn upserts(documents: Vec<Document>) -> VersionChanges {
        VersionChanges {
            upserts: documents,
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

    /// A read-only index directory makes the commit fail. Root ignores the permission, so these tests need a
    /// regular user.
    #[cfg(unix)]
    fn read_only(dir: &TempDir, read_only: bool) {
        use std::os::unix::fs::PermissionsExt;
        let mode = if read_only { 0o555 } else { 0o755 };
        fs::set_permissions(dir.path().join(INDEX_DIR), fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_commit_leaves_nothing_staged_for_the_failure_record() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer
            .apply(&snapshot(&a, 1), &upserts(vec![document(&a, "x.md", &["kept"])]))
            .unwrap();
        read_only(&dir, true);
        let changes = VersionChanges {
            deletions: vec![DocKey::new(&a, "x.md").unwrap()],
            ..upserts(vec![document(&a, "y.md", &["lost"])])
        };
        let failed = writer.apply(&snapshot(&a, 2), &changes);
        read_only(&dir, false);
        assert!(failed.is_err(), "{failed:?}");
        let recorded = writer.record_failure(&snapshot(&a, 2), "disk full").unwrap();
        assert_eq!((recorded.section_count, recorded.outcome), (1, Outcome::Failed));
        assert_eq!(section_texts(writer.store(), &a), ["kept"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_failure_record_that_cannot_be_committed_leaves_the_previous_record() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer
            .apply(&snapshot(&a, 1), &upserts(vec![document(&a, "x.md", &["kept"])]))
            .unwrap();
        read_only(&dir, true);
        let failed = writer.record_failure(&snapshot(&a, 2), "disk full");
        read_only(&dir, false);
        assert!(failed.is_err(), "{failed:?}");
        writer.commit().unwrap();
        let stored = writer.store().version_state(&a).unwrap().unwrap();
        assert_eq!((stored.outcome, stored.section_count), (Outcome::Ok, 1));
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_commit_keeps_the_pruned_scopes_for_the_next_one() {
        let dir = TempDir::new().unwrap();
        let (a, b) = (scope("lib", "1"), scope("lib", "2"));
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        for version in [&a, &b] {
            writer
                .apply(
                    &snapshot(version, 1),
                    &upserts(vec![document(version, "x.md", &["text"])]),
                )
                .unwrap();
        }
        assert_eq!(
            writer.prune(std::slice::from_ref(&a)).unwrap(),
            std::slice::from_ref(&b)
        );
        read_only(&dir, true);
        let failed = writer.commit();
        read_only(&dir, false);
        assert!(failed.is_err(), "{failed:?}");
        assert!(writer.store().version_state(&b).unwrap().is_some());
        writer.commit().unwrap();
        assert!(writer.store().version_state(&b).unwrap().is_none());
        assert_eq!(section_texts(writer.store(), &a), ["text"]);
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

    /// The keys in `scope` whose documents the writer's store holds.
    fn doc_paths(store: &Store, scope: &Scope) -> Vec<String> {
        store
            .doc_states(scope)
            .unwrap()
            .keys()
            .map(|key| key.doc_path().to_owned())
            .collect()
    }

    #[test]
    fn a_streamed_scope_is_committed_once_by_commit_scope() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer.begin(&a);
        writer.add(&document(&a, "x.md", &["one", "two"])).unwrap();
        writer.add(&document(&a, "y.md", &["three"])).unwrap();
        writer.add(&document(&a, "y.md", &["three again"])).unwrap();
        writer.store().reload().unwrap();
        assert_eq!(writer.store().version_state(&a).unwrap(), None);
        assert_eq!(section_texts(writer.store(), &a), Vec::<String>::new());

        let mut state = snapshot(&a, 10);
        state.outcome = Outcome::Partial;
        state.last_error = Some("caller's error".to_owned());
        state.revision = 40;
        state.document_count = 40;
        state.section_count = 40;
        let stored = writer.commit_scope(&state).unwrap();
        assert_eq!(
            (stored.revision, stored.document_count, stored.section_count),
            (1, 2, 3)
        );
        assert_eq!(stored.outcome, Outcome::Partial);
        assert_eq!(stored.last_error.as_deref(), Some("caller's error"));
        assert_eq!(writer.store().version_state(&a).unwrap(), Some(stored));
        assert_eq!(section_texts(writer.store(), &a), ["one", "three again", "two"]);
        assert_eq!(doc_paths(writer.store(), &a), ["x.md", "y.md"]);
    }

    #[test]
    fn a_second_stream_keeps_deletes_and_replaces_documents() {
        let dir = TempDir::new().unwrap();
        let (a, b) = (scope("lib", "1"), scope("lib", "2"));
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer
            .apply(
                &snapshot(&a, 1),
                &upserts(vec![
                    document(&a, "x.md", &["kept"]),
                    document(&a, "y.md", &["old 1", "old 2"]),
                    document(&a, "z.md", &["gone"]),
                ]),
            )
            .unwrap();
        writer
            .apply(&snapshot(&b, 1), &upserts(vec![document(&b, "x.md", &["other"])]))
            .unwrap();
        writer.begin(&a);
        writer.keep(&DocKey::new(&a, "x.md").unwrap()).unwrap();
        writer.add(&document(&a, "y.md", &["new"])).unwrap();
        writer.delete(&DocKey::new(&a, "z.md").unwrap()).unwrap();
        writer.add(&document(&a, "w.md", &["added", "then"])).unwrap();
        writer.delete(&DocKey::new(&a, "w.md").unwrap()).unwrap();
        let stored = writer.commit_scope(&snapshot(&a, 2)).unwrap();
        assert_eq!(
            (stored.revision, stored.document_count, stored.section_count),
            (2, 2, 2)
        );
        assert_eq!(section_texts(writer.store(), &a), ["kept", "new"]);
        assert_eq!(doc_paths(writer.store(), &a), ["x.md", "y.md"]);
        assert_eq!(section_texts(writer.store(), &b), ["other"]);
    }

    #[test]
    fn a_stream_that_only_keeps_leaves_the_revision() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer
            .apply(&snapshot(&a, 1), &upserts(vec![document(&a, "x.md", &["one"])]))
            .unwrap();
        writer.begin(&a);
        writer.keep(&DocKey::new(&a, "x.md").unwrap()).unwrap();
        let stored = writer.commit_scope(&snapshot(&a, 2)).unwrap();
        assert_eq!(
            (stored.revision, stored.document_count, stored.section_count),
            (1, 1, 1)
        );
        assert_eq!(stored.last_attempt, Timestamp::from_unix(2));
    }

    #[test]
    fn a_rolled_back_stream_leaves_the_index_as_it_was() {
        let dir = TempDir::new().unwrap();
        let (a, b) = (scope("lib", "1"), scope("lib", "2"));
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        let before = writer
            .apply(&snapshot(&a, 1), &upserts(vec![document(&a, "x.md", &["original"])]))
            .unwrap();
        writer.begin(&a);
        writer.add(&document(&a, "x.md", &["replaced"])).unwrap();
        writer.add(&document(&a, "y.md", &["stray"])).unwrap();
        writer.delete(&DocKey::new(&a, "x.md").unwrap()).unwrap();
        writer.rollback().unwrap();
        writer
            .apply(&snapshot(&b, 2), &upserts(vec![document(&b, "x.md", &["later"])]))
            .unwrap();
        assert_eq!(writer.store().version_state(&a).unwrap(), Some(before));
        assert_eq!(section_texts(writer.store(), &a), ["original"]);

        writer.begin(&a);
        let stored = writer.commit_scope(&snapshot(&a, 3)).unwrap();
        assert_eq!((stored.revision, stored.document_count), (1, 1));
        assert_eq!(section_texts(writer.store(), &a), ["original"]);
    }

    #[test]
    fn a_stream_rejects_documents_of_another_scope() {
        let dir = TempDir::new().unwrap();
        let (a, b) = (scope("lib", "1"), scope("other", "1"));
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer.begin(&a);
        let foreign = DocKey::new(&b, "y.md").unwrap();
        let errors = [
            writer.add(&document(&b, "y.md", &["stray"])).unwrap_err(),
            writer.keep(&foreign).unwrap_err(),
            writer.delete(&foreign).unwrap_err(),
        ];
        for error in errors {
            assert_eq!(error.to_string(), "document other/1/y.md does not belong to lib@1");
        }
        writer.add(&document(&a, "x.md", &["mine"])).unwrap();
        let stored = writer.commit_scope(&snapshot(&a, 1)).unwrap();
        assert_eq!((stored.document_count, stored.section_count), (1, 1));
        assert_eq!(section_texts(writer.store(), &b), Vec::<String>::new());
    }

    #[test]
    fn a_streamed_commit_carries_the_pruned_scopes() {
        let dir = TempDir::new().unwrap();
        let (a, b) = (scope("a", "1"), scope("b", "1"));
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer
            .apply(&snapshot(&b, 1), &upserts(vec![document(&b, "x.md", &["text"])]))
            .unwrap();
        assert_eq!(
            writer.prune(std::slice::from_ref(&a)).unwrap(),
            std::slice::from_ref(&b)
        );
        writer.begin(&a);
        writer.add(&document(&a, "x.md", &["text"])).unwrap();
        writer.commit_scope(&snapshot(&a, 2)).unwrap();
        assert_eq!(writer.store().version_state(&b).unwrap(), None);
        assert_eq!(section_texts(writer.store(), &b), Vec::<String>::new());
        assert_eq!(section_texts(writer.store(), &a), ["text"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_streamed_commit_rolls_the_stream_back() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer
            .apply(&snapshot(&a, 1), &upserts(vec![document(&a, "x.md", &["original"])]))
            .unwrap();
        writer.begin(&a);
        writer.add(&document(&a, "x.md", &["replaced"])).unwrap();
        read_only(&dir, true);
        let failed = writer.commit_scope(&snapshot(&a, 2));
        read_only(&dir, false);
        assert!(failed.is_err(), "{failed:?}");
        writer.begin(&a);
        let stored = writer.commit_scope(&snapshot(&a, 3)).unwrap();
        assert_eq!((stored.revision, stored.document_count), (1, 1));
        assert_eq!(section_texts(writer.store(), &a), ["original"]);
    }

    #[test]
    #[should_panic(expected = "a stream is already open")]
    fn a_second_begin_without_a_commit_panics() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer.begin(&a);
        writer.begin(&a);
    }

    #[test]
    #[should_panic(expected = "commit_scope for lib@2 in a stream of lib@1")]
    fn a_commit_for_another_scope_panics() {
        let dir = TempDir::new().unwrap();
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        writer.begin(&scope("lib", "1"));
        let _ = writer.commit_scope(&snapshot(&scope("lib", "2"), 1));
    }

    #[test]
    #[should_panic(expected = "no stream is open; call begin first")]
    fn streaming_without_begin_panics() {
        let dir = TempDir::new().unwrap();
        let a = scope("lib", "1");
        let mut writer = StoreWriter::open(dir.path(), &settings()).unwrap();
        let _ = writer.add(&document(&a, "x.md", &["one"]));
    }
}

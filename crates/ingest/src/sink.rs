//! Where a collection run puts what it finds, one document at a time (design §12.4).

use zenodotus_core::ids::DocKey;
use zenodotus_core::model::{DocError, Document, VersionChanges};

/// Whether a collection run goes on after a sink call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Go on.
    Continue,
    /// End the run at this document boundary: the job was cut (design §12.1) or the sink failed.
    Stop,
}

/// Receives the findings of one collection run, mirroring [`VersionChanges`].
pub trait DocumentSink {
    /// A new or changed document.
    fn upsert(&mut self, document: Document) -> Flow;
    /// A document confirmed current.
    fn unchanged(&mut self, key: DocKey) -> Flow;
    /// A document proven gone.
    fn delete(&mut self, key: DocKey) -> Flow;
    /// A document that failed and keeps its previous content.
    fn error(&mut self, error: DocError) -> Flow;
    /// The commit a git reference resolved to.
    fn resolved_commit(&mut self, commit: String) -> Flow;
}

/// Collects everything, as v1's `collect` returns it.
impl DocumentSink for VersionChanges {
    fn upsert(&mut self, document: Document) -> Flow {
        self.upserts.push(document);
        Flow::Continue
    }

    fn unchanged(&mut self, key: DocKey) -> Flow {
        self.unchanged.push(key);
        Flow::Continue
    }

    fn delete(&mut self, key: DocKey) -> Flow {
        self.deletions.push(key);
        Flow::Continue
    }

    fn error(&mut self, error: DocError) -> Flow {
        self.errors.push(error);
        Flow::Continue
    }

    fn resolved_commit(&mut self, commit: String) -> Flow {
        self.resolved_commit = Some(commit);
        Flow::Continue
    }
}

/// Sends `changes` to `sink`: the resolved commit, then upserts, unchanged documents, deletions and errors, each in
/// order. Ends at the first [`Flow::Stop`] and returns it.
pub fn replay<S: DocumentSink + ?Sized>(changes: VersionChanges, sink: &mut S) -> Flow {
    let VersionChanges {
        upserts,
        unchanged,
        deletions,
        errors,
        resolved_commit,
    } = changes;
    let calls = resolved_commit
        .into_iter()
        .map(Call::Commit)
        .chain(upserts.into_iter().map(Call::Upsert))
        .chain(unchanged.into_iter().map(Call::Unchanged))
        .chain(deletions.into_iter().map(Call::Delete))
        .chain(errors.into_iter().map(Call::Error));
    for call in calls {
        let flow = match call {
            Call::Commit(commit) => sink.resolved_commit(commit),
            Call::Upsert(document) => sink.upsert(document),
            Call::Unchanged(key) => sink.unchanged(key),
            Call::Delete(key) => sink.delete(key),
            Call::Error(error) => sink.error(error),
        };
        if flow == Flow::Stop {
            return Flow::Stop;
        }
    }
    Flow::Continue
}

/// One sink call of [`replay`].
enum Call {
    Commit(String),
    Upsert(Document),
    Unchanged(DocKey),
    Delete(DocKey),
    Error(DocError),
}

#[cfg(test)]
pub(crate) mod tests {
    use zenodotus_core::ids::{DocKey, LibraryId, Scope, Version};
    use zenodotus_core::model::{DocError, DocState, Document, VersionChanges};
    use zenodotus_core::time::Timestamp;

    use super::*;

    /// Records every call as one line and stops after `limit` calls.
    #[derive(Debug, Default)]
    pub(crate) struct Recorder {
        pub calls: Vec<String>,
        pub limit: Option<usize>,
    }

    impl Recorder {
        fn record(&mut self, call: String) -> Flow {
            self.calls.push(call);
            if self.limit.is_some_and(|limit| self.calls.len() >= limit) {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }

    impl DocumentSink for Recorder {
        fn upsert(&mut self, document: Document) -> Flow {
            self.record(format!("upsert {}", document.state.doc_key.doc_path()))
        }
        fn unchanged(&mut self, key: DocKey) -> Flow {
            self.record(format!("unchanged {}", key.doc_path()))
        }
        fn delete(&mut self, key: DocKey) -> Flow {
            self.record(format!("delete {}", key.doc_path()))
        }
        fn error(&mut self, error: DocError) -> Flow {
            self.record(format!("error {}", error.doc_path))
        }
        fn resolved_commit(&mut self, commit: String) -> Flow {
            self.record(format!("commit {commit}"))
        }
    }

    fn key(path: &str) -> DocKey {
        let scope = Scope::new(LibraryId::parse("lib").unwrap(), Version::parse("1").unwrap());
        DocKey::new(&scope, path).unwrap()
    }

    fn changes() -> VersionChanges {
        VersionChanges {
            upserts: vec![Document {
                state: DocState {
                    doc_key: key("new.md"),
                    etag: None,
                    last_modified: None,
                    content_hash: "0".repeat(64),
                    fetched_at: Timestamp::from_unix(1),
                    source: None,
                },
                sections: Vec::new(),
            }],
            unchanged: vec![key("same.md")],
            deletions: vec![key("gone.md")],
            errors: vec![DocError {
                doc_path: "broken.md".to_owned(),
                kind: "read".to_owned(),
                message: "unreadable".to_owned(),
            }],
            resolved_commit: Some("c1".to_owned()),
        }
    }

    #[test]
    fn version_changes_collect_every_call() {
        let source = changes();
        let mut sink = VersionChanges::default();
        assert_eq!(replay(source.clone(), &mut sink), Flow::Continue);
        assert_eq!(sink, source);
    }

    #[test]
    fn a_replay_forwards_in_order_and_ends_at_the_first_stop() {
        let mut sink = Recorder::default();
        assert_eq!(replay(changes(), &mut sink), Flow::Continue);
        assert_eq!(
            sink.calls,
            [
                "commit c1",
                "upsert new.md",
                "unchanged same.md",
                "delete gone.md",
                "error broken.md"
            ]
        );
        for limit in 1..=5 {
            let mut sink = Recorder {
                limit: Some(limit),
                ..Recorder::default()
            };
            assert_eq!(replay(changes(), &mut sink), Flow::Stop, "{limit}");
            assert_eq!(sink.calls.len(), limit);
        }
    }
}

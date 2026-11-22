//! Schema-1 decode (plan §3.11): a v1 index's scope records are read raw with the §12.5 defaults, while `Store::open`
//! refuses the old schema.

use tempfile::TempDir;
use zenodotus_core::config::IndexSettings;
use zenodotus_core::model::{Origin, Outcome};
use zenodotus_search::SearchError;
use zenodotus_search::store::{Store, StoreWriter};
use zenodotus_search::v1_index;

/// Two v1 records, written by hand as v1 serialized them.
const V1_RECORDS: [&str; 2] = [
    r#"{"scope":{"library":"widgets","version":"2.0"},"name":"Widgets","description":"A widget toolkit","aliases":["w"],
       "homepage":"https://widgets.example/","license":"MIT","source":"github:example/widgets@main","default":true,
       "list_order":0,"revision":4,"last_success":100,"last_attempt":100,"last_verified":100,"outcome":"ok",
       "last_error":null,"document_count":3,"section_count":12,"resolved_commit":"0123456789abcdef0123456789abcdef01234567"}"#,
    r#"{"scope":{"library":"gadgets","version":"1"},"name":"Gadgets","description":"Gadget helpers","aliases":[],
       "homepage":null,"license":null,"source":"local:/srv/gadgets","default":false,"list_order":1,"revision":1,
       "last_success":null,"last_attempt":50,"last_verified":null,"outcome":"failed","last_error":"unreachable",
       "document_count":0,"section_count":0,"resolved_commit":null}"#,
];

#[test]
fn a_schema_1_index_is_read_raw_with_the_new_fields_defaulted() {
    let dir = TempDir::new().unwrap();
    let settings = IndexSettings::default();
    v1_index::build(dir.path(), &settings, &V1_RECORDS).unwrap();

    let states = Store::read_states_raw(dir.path()).unwrap();
    let scopes: Vec<String> = states.iter().map(|state| state.scope.to_string()).collect();
    assert_eq!(scopes, ["gadgets@1", "widgets@2.0"]);
    for state in &states {
        assert_eq!((state.origin, &state.plan, state.bytes), (Origin::Config, &None, 0));
    }
    assert_eq!(states[0].outcome, Outcome::Failed);
    assert_eq!(states[1].section_count, 12);

    for opened in [
        Store::open(dir.path(), &settings).err(),
        StoreWriter::open(dir.path(), &settings).err(),
    ] {
        match opened {
            Some(SearchError::SchemaMismatch { found, expected }) => {
                assert!(found.starts_with("schema 1 "), "{found}");
                assert!(expected.starts_with("schema 2 "), "{expected}");
            }
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }
}

#[test]
fn reading_raw_states_needs_no_index_and_reads_a_current_one() {
    let dir = TempDir::new().unwrap();
    assert_eq!(Store::read_states_raw(dir.path()).unwrap(), []);
    let mut writer = StoreWriter::open(dir.path(), &IndexSettings::default()).unwrap();
    writer.commit().unwrap();
    writer.finish().unwrap();
    assert_eq!(Store::read_states_raw(dir.path()).unwrap(), []);
}

//! Writing the config file: the `init` template and `add`, which edits the file in place and keeps its comments.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write as _};
use std::path::Path;

use serde::Serialize as _;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::config::{Config, VersionSpec};
use crate::error::CoreError;
use crate::ids::LibraryId;

/// The commented config file that `init` writes. It parses to the default [`Config`].
#[must_use]
pub fn init_template() -> String {
    "\
# zenodotus configuration. Every setting is optional; the values shown are the defaults.
# Add libraries with `zenodotus add`, then index them with `zenodotus sync`.

[settings]
# data_dir = \"~/.local/share/zenodotus\"

[settings.fetch]
# allow_http = false
# allow_private_networks = false
# page_max_bytes = 10_485_760
# retries = 3

[settings.http]
# listen = \"127.0.0.1:8080\"
# allowed_hosts = []
# allowed_origins = []

[settings.ranking]
# symbol_boost = 0.5
# phrase_boost = 1.0
# quoted_phrase_boost = 2.0
# synonyms = { env = [\"environment\"] }

[settings.index]
# Changing a weight requires `zenodotus sync --rebuild`.
# title_weight = 4
# heading_weight = 2
# symbol_weight = 5

# [[library]]
# id = \"tokio\"
# name = \"Tokio\"
# [[library.version]]
# version = \"latest\"
# default = true
# source = { kind = \"github\", repo = \"tokio-rs/website\", paths = [\"content\"] }
"
    .to_owned()
}

/// Creates `path` with `text`, creating parent directories, and never overwrites.
///
/// # Errors
/// [`CoreError::ConfigExists`] when the file exists; [`CoreError::ConfigIo`] for any other I/O failure.
pub fn write_new(path: &Path, text: &str) -> Result<(), CoreError> {
    let io = |source| CoreError::ConfigIo {
        path: path.to_owned(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| match source.kind() {
            ErrorKind::AlreadyExists => CoreError::ConfigExists { path: path.to_owned() },
            _ => io(source),
        })?;
    file.write_all(text.as_bytes()).map_err(io)
}

/// Metadata for a library that `add` creates. Ignored when the library already exists, except fields given here,
/// which replace the existing ones.
#[derive(Debug, Clone, Copy, Default)]
pub struct LibraryMeta<'a> {
    /// Display name.
    pub name: Option<&'a str>,
    /// One-line description.
    pub description: Option<&'a str>,
}

/// Adds `spec` as a version of `library` to the config file at `path`, creating the library when needed, and returns
/// the resulting configuration. The file is replaced atomically and keeps its comments and layout.
///
/// # Errors
/// [`CoreError::ConfigIo`] and [`CoreError::ConfigParse`] for the file; [`CoreError::DuplicateVersion`] when the
/// version exists; otherwise any validation error of the edited file, which is then left unchanged.
pub fn add_version(
    path: &Path,
    library: &LibraryId,
    meta: LibraryMeta<'_>,
    spec: &VersionSpec,
) -> Result<Config, CoreError> {
    let io = |source| CoreError::ConfigIo {
        path: path.to_owned(),
        source,
    };
    let text = std::fs::read_to_string(path).map_err(io)?;
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|error: toml_edit::TomlError| CoreError::ConfigParse {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    add_to_document(&mut doc, path, library, meta, spec)?;
    let edited = doc.to_string();
    let config = Config::parse(path, &edited)?;
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, edited).map_err(io)?;
    std::fs::rename(&temporary, path).map_err(io)?;
    Ok(config)
}

/// The edit itself; `path` only labels errors.
fn add_to_document(
    doc: &mut DocumentMut,
    path: &Path,
    library: &LibraryId,
    meta: LibraryMeta<'_>,
    spec: &VersionSpec,
) -> Result<(), CoreError> {
    let version_table = version_table(library, spec)?;
    let trailing = doc.trailing().as_str().unwrap_or_default().to_owned();
    let libraries = array_of_tables(doc.as_table_mut(), "library", path)?;
    let found = libraries
        .iter_mut()
        .find(|table| table.get("id").and_then(Item::as_str) == Some(library.as_str()));
    let Some(entry) = found else {
        // Comments that end the file follow the last table; a new library goes after them.
        let mut table = Table::new();
        table.decor_mut().set_prefix(format!("{trailing}\n"));
        table.insert("id", value(library.as_str()));
        apply_meta(&mut table, meta);
        let mut versions = ArrayOfTables::new();
        versions.push(version_table);
        table.insert("version", Item::ArrayOfTables(versions));
        libraries.push(table);
        doc.set_trailing("");
        return Ok(());
    };
    apply_meta(entry, meta);
    let versions = array_of_tables(entry, "version", path)?;
    for existing in versions.iter_mut() {
        if existing.get("version").and_then(Item::as_str) == Some(spec.version.as_str()) {
            return Err(CoreError::DuplicateVersion {
                library: library.to_string(),
                version: spec.version.to_string(),
            });
        }
        if spec.default {
            existing.remove("default");
        }
    }
    versions.push(version_table);
    Ok(())
}

/// Writes the metadata fields that were given.
fn apply_meta(table: &mut Table, meta: LibraryMeta<'_>) {
    if let Some(name) = meta.name {
        table.insert("name", value(name));
    }
    if let Some(description) = meta.description {
        table.insert("description", value(description));
    }
}

/// The `[[library.version]]` table for `spec`.
fn version_table(library: &LibraryId, spec: &VersionSpec) -> Result<Table, CoreError> {
    let source = spec
        .source
        .serialize(toml_edit::ser::ValueSerializer::new())
        .map_err(|_| CoreError::InvalidSource {
            library: library.to_string(),
            version: spec.version.to_string(),
            reason: "`path` is not valid UTF-8",
        })?;
    let mut table = Table::new();
    table.insert("version", value(spec.version.as_str()));
    if spec.default {
        table.insert("default", value(true));
    }
    table.insert("source", value(source));
    if !spec.extra_hosts.is_empty() {
        table.insert(
            "extra_hosts",
            value(spec.extra_hosts.iter().collect::<toml_edit::Array>()),
        );
    }
    Ok(table)
}

/// The array of tables at `key`, created when missing.
fn array_of_tables<'t>(table: &'t mut Table, key: &str, path: &Path) -> Result<&'t mut ArrayOfTables, CoreError> {
    table
        .entry(key)
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or_else(|| CoreError::ConfigParse {
            path: path.to_owned(),
            message: format!("`{key}` must be an array of tables (`[[{key}]]`)"),
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ids::Version;
    use crate::model::SourceSpec;

    fn spec(version: &str, default: bool) -> VersionSpec {
        VersionSpec {
            version: Version::parse(version).unwrap(),
            default,
            source: SourceSpec::Local {
                path: PathBuf::from("/srv/docs"),
                include: vec![],
                exclude: vec![],
            },
            extra_hosts: vec![],
        }
    }

    fn id(text: &str) -> LibraryId {
        LibraryId::parse(text).unwrap()
    }

    fn config_file(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zenodotus.toml");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn the_template_parses_to_the_default_config() {
        assert_eq!(
            Config::parse(Path::new("t.toml"), &init_template()).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn write_new_creates_parents_and_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/zenodotus.toml");
        write_new(&path, "x = 1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x = 1\n");
        assert!(matches!(
            write_new(&path, "y = 2\n"),
            Err(CoreError::ConfigExists { path: ref existing }) if existing == &path
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x = 1\n");
    }

    #[test]
    fn write_new_reports_other_io_errors() {
        let (_dir, file) = config_file("");
        assert!(matches!(
            write_new(&file.join("child.toml"), ""),
            Err(CoreError::ConfigIo { .. })
        ));
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(write_new(dir.path(), ""), Err(CoreError::ConfigExists { .. })));
        assert!(matches!(write_new(Path::new(""), ""), Err(CoreError::ConfigIo { .. })));
    }

    #[test]
    fn adds_a_new_library_and_keeps_comments() {
        let (_dir, path) = config_file("# my docs\n[settings] # inline\ndata_dir = \"d\"\n");
        let meta = LibraryMeta {
            name: Some("Tokio"),
            description: Some("Async runtime"),
        };
        let mut first = spec("1.40", true);
        first.extra_hosts = vec!["cdn.tokio.rs".into()];
        let config = add_version(&path, &id("tokio"), meta, &first).unwrap();
        assert_eq!(config.libraries[0].versions, [first]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# my docs\n[settings] # inline\ndata_dir = \"d\"\n\n\
             [[library]]\nid = \"tokio\"\nname = \"Tokio\"\ndescription = \"Async runtime\"\n\n\
             [[library.version]]\nversion = \"1.40\"\ndefault = true\n\
             source = { kind = \"local\", path = \"/srv/docs\" }\nextra_hosts = [\"cdn.tokio.rs\"]\n"
        );
    }

    #[test]
    fn comments_that_end_the_file_stay_above_a_new_library() {
        let (_dir, path) = config_file(&init_template());
        add_version(&path, &id("tokio"), LibraryMeta::default(), &spec("1", true)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[settings.index]\n# Changing a weight requires"),
            "{text}"
        );
        assert!(
            text.ends_with("source = { kind = \"local\", path = \"/srv/docs\" }\n"),
            "{text}"
        );
        let weighted = text.replace("# title_weight = 4", "title_weight = 5");
        assert_eq!(Config::parse(&path, &weighted).unwrap().settings.index.title_weight, 5);
    }

    #[test]
    fn adds_a_version_to_an_existing_library_and_moves_the_default() {
        let (_dir, path) = config_file(&init_template());
        add_version(&path, &id("a"), LibraryMeta::default(), &spec("1", true)).unwrap();
        add_version(&path, &id("b"), LibraryMeta::default(), &spec("1", false)).unwrap();
        let config = add_version(&path, &id("a"), LibraryMeta::default(), &spec("2", true)).unwrap();
        let a = &config.libraries[0];
        assert_eq!(a.versions.len(), 2);
        assert!(!a.versions[0].default);
        assert!(a.versions[1].default);
        assert_eq!(config.libraries[1].id, id("b"));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("# zenodotus configuration.")
        );
        let config = add_version(
            &path,
            &id("a"),
            LibraryMeta {
                name: Some("A"),
                description: None,
            },
            &spec("3", false),
        )
        .unwrap();
        assert_eq!(config.libraries[0].display_name(), "A");
        assert!(config.libraries[0].versions[1].default);
    }

    #[test]
    fn refuses_a_duplicate_version_and_leaves_the_file_unchanged() {
        let (_dir, path) = config_file("");
        add_version(&path, &id("a"), LibraryMeta::default(), &spec("1", false)).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(matches!(
            add_version(&path, &id("a"), LibraryMeta::default(), &spec("1", true)),
            Err(CoreError::DuplicateVersion { library, version }) if library == "a" && version == "1"
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_result_that_fails_validation_is_not_written() {
        let (_dir, path) = config_file("");
        let mut bad = spec("1", false);
        bad.source = SourceSpec::GitHub {
            repo: "nope".into(),
            reference: None,
            paths: vec![],
        };
        assert!(matches!(
            add_version(&path, &id("a"), LibraryMeta::default(), &bad),
            Err(CoreError::InvalidSource { .. })
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn a_non_utf8_path_is_an_invalid_source() {
        use std::os::unix::ffi::OsStrExt as _;
        let (_dir, path) = config_file("");
        let mut bad = spec("1", false);
        bad.source = SourceSpec::Local {
            path: PathBuf::from(std::ffi::OsStr::from_bytes(b"/srv/\xff")),
            include: vec![],
            exclude: vec![],
        };
        assert!(matches!(
            add_version(&path, &id("a"), LibraryMeta::default(), &bad),
            Err(CoreError::InvalidSource {
                reason: "`path` is not valid UTF-8",
                ..
            })
        ));
    }

    #[test]
    fn file_errors_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.toml");
        assert!(matches!(
            add_version(&missing, &id("a"), LibraryMeta::default(), &spec("1", false)),
            Err(CoreError::ConfigIo { .. })
        ));
        for text in ["[settings", "library = 1\n", "[[library]]\nid = \"a\"\nversion = 2\n"] {
            let (_dir, path) = config_file(text);
            assert!(
                matches!(
                    add_version(&path, &id("a"), LibraryMeta::default(), &spec("1", false)),
                    Err(CoreError::ConfigParse { .. })
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn a_failed_write_is_an_io_error() {
        let (dir, path) = config_file("");
        std::fs::create_dir(dir.path().join("zenodotus.toml.tmp")).unwrap();
        assert!(matches!(
            add_version(&path, &id("a"), LibraryMeta::default(), &spec("1", false)),
            Err(CoreError::ConfigIo { .. })
        ));
    }
}

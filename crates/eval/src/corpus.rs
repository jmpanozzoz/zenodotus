//! The evaluation corpus: `eval/corpus.toml`, a regular zenodotus config whose versions are GitHub sources pinned to
//! a commit, and `eval/context7.toml`, the Context7 library id of each version.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use zenodotus_core::config::{Config, IndexSettings};
use zenodotus_core::ids::Scope;
use zenodotus_core::model::SourceSpec;

use crate::error::EvalError;
use crate::questions::Question;

/// One library version of the corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeCase {
    /// The library id.
    pub library: String,
    /// The library's display name.
    pub name: String,
    /// The version label.
    pub version: String,
    /// Whether the config marks this version as its library's default.
    pub default: bool,
    /// The library and version.
    pub scope: Scope,
    /// `library` when the library has one version, else `library@version`; the key of `context7.toml`.
    pub key: String,
    /// The local copy of the source at its pinned commit: `<sources>/<key with @ replaced by ->`, absolute.
    pub checkout: PathBuf,
    /// The documentation folder inside the checkout.
    pub path: String,
    /// The pinned Context7 library id, if Context7 has this version.
    pub context7: Option<String>,
}

impl ScopeCase {
    /// The documentation folder of the local copy.
    #[must_use]
    pub fn docs(&self) -> PathBuf {
        self.checkout.join(&self.path)
    }
}

/// The corpus, loaded from a repository root.
#[derive(Debug, Clone)]
pub struct Corpus {
    /// `eval/corpus.toml`.
    pub config_path: PathBuf,
    /// The data directory its index is synced into.
    pub data_dir: PathBuf,
    /// Its `[settings.index]`.
    pub index: IndexSettings,
    /// Every library version, in config order.
    pub scopes: Vec<ScopeCase>,
}

/// `eval/context7.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Context7File {
    libraries: BTreeMap<String, String>,
}

impl Corpus {
    /// Loads the corpus under the repository `root`. The local copies of the sources live under `sources`.
    ///
    /// # Errors
    ///
    /// A config error, [`EvalError::Io`] or [`EvalError::Parse`] for `context7.toml`, and
    /// [`EvalError::UnsupportedSource`] for a version that is not a GitHub source with one path.
    pub fn load(root: &Path, sources: &Path) -> Result<Self, EvalError> {
        // Competitors get the copies as absolute paths and `file://` URLs; `absolute` fails only on an empty path.
        let sources = std::path::absolute(sources).unwrap_or(sources.to_path_buf());
        let config_path = root.join("eval").join("corpus.toml");
        let config = Config::load(&config_path)?;
        let data_dir = config.settings.data_dir(&config_path, &|_| None)?;
        let context7_path = root.join("eval").join("context7.toml");
        let text = std::fs::read_to_string(&context7_path).map_err(EvalError::io(&context7_path))?;
        let context7: Context7File = toml::from_str(&text).map_err(|error| EvalError::Parse {
            path: context7_path.clone(),
            line: 0,
            message: error.message().to_owned(),
        })?;
        let mut scopes = Vec::new();
        for library in &config.libraries {
            for version in &library.versions {
                let id = library.id.as_str();
                let label = version.version.as_str();
                let key = if library.versions.len() == 1 {
                    id.to_owned()
                } else {
                    format!("{id}@{label}")
                };
                let path = match &version.source {
                    SourceSpec::GitHub { paths, .. } if paths.len() == 1 => paths[0].clone(),
                    _ => return Err(EvalError::UnsupportedSource(format!("{id}@{label}"))),
                };
                scopes.push(ScopeCase {
                    library: id.to_owned(),
                    name: library.display_name().to_owned(),
                    version: label.to_owned(),
                    default: version.default,
                    scope: Scope::new(library.id.clone(), version.version.clone()),
                    checkout: sources.join(key.replace('@', "-")),
                    path,
                    context7: context7.libraries.get(&key).cloned(),
                    key,
                });
            }
        }
        Ok(Self {
            config_path,
            data_dir,
            index: config.settings.index,
            scopes,
        })
    }

    /// The library version `question` asks about: its `version`, or the library's default version (the one marked
    /// `default`, else the first listed).
    ///
    /// # Errors
    ///
    /// [`EvalError::UnknownScope`] when the corpus has no such library version.
    pub fn scope_of(&self, question: &Question) -> Result<&ScopeCase, EvalError> {
        let versions: Vec<&ScopeCase> = self
            .scopes
            .iter()
            .filter(|case| case.library == question.library)
            .collect();
        let found = match &question.version {
            Some(version) => versions.iter().find(|case| case.version == *version),
            None => versions.iter().find(|case| case.default).or(versions.first()),
        };
        found.copied().ok_or_else(|| EvalError::UnknownScope {
            id: question.id.clone(),
            scope: question.version.as_ref().map_or_else(
                || question.library.clone(),
                |version| format!("{}@{version}", question.library),
            ),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::questions::tests::question;

    /// A corpus config with `tokio` (one version) and `pydantic` (2.12, the default, and 1.10).
    pub(crate) const CONFIG: &str = r#"
[settings]
data_dir = "data"

[[library]]
id = "tokio"
name = "Tokio"
[[library.version]]
version = "latest"
source = { kind = "github", repo = "tokio-rs/website", reference = "abc", paths = ["content"] }

[[library]]
id = "pydantic"
[[library.version]]
version = "1.10"
source = { kind = "github", repo = "pydantic/pydantic", reference = "def", paths = ["docs"] }
[[library.version]]
version = "2.12"
default = true
source = { kind = "github", repo = "pydantic/pydantic", reference = "ghi", paths = ["docs"] }
"#;

    /// Writes `eval/corpus.toml` and `eval/context7.toml` under a new root.
    pub(crate) fn root_with(config: &str, context7: &str) -> TempDir {
        let root = TempDir::new().unwrap();
        let eval = root.path().join("eval");
        std::fs::create_dir_all(&eval).unwrap();
        std::fs::write(eval.join("corpus.toml"), config).unwrap();
        std::fs::write(eval.join("context7.toml"), context7).unwrap();
        root
    }

    /// The test corpus, with Context7 ids for `tokio` and `pydantic@2.12`, and sources under `<root>/sources`.
    pub(crate) fn corpus() -> (TempDir, Corpus) {
        let root = root_with(
            CONFIG,
            "[libraries]\ntokio = \"/websites/rs_tokio\"\n\"pydantic@2.12\" = \"/pydantic/pydantic/v2.12.5\"\n",
        );
        let corpus = Corpus::load(root.path(), &root.path().join("sources")).unwrap();
        (root, corpus)
    }

    #[test]
    fn every_version_becomes_a_scope_with_its_key_checkout_and_context7_id() {
        let (root, corpus) = corpus();
        assert_eq!(corpus.config_path, root.path().join("eval/corpus.toml"));
        assert_eq!(corpus.data_dir, root.path().join("eval/data"));
        let keys: Vec<&str> = corpus.scopes.iter().map(|case| case.key.as_str()).collect();
        assert_eq!(keys, ["tokio", "pydantic@1.10", "pydantic@2.12"]);
        let tokio = &corpus.scopes[0];
        assert_eq!((tokio.name.as_str(), tokio.version.as_str()), ("Tokio", "latest"));
        assert_eq!(tokio.scope.to_string(), "tokio@latest");
        assert_eq!(tokio.docs(), root.path().join("sources/tokio/content"));
        assert_eq!(tokio.context7.as_deref(), Some("/websites/rs_tokio"));
        let old = &corpus.scopes[1];
        assert_eq!(old.name, "pydantic");
        assert_eq!(old.checkout, root.path().join("sources/pydantic-1.10"));
        assert_eq!(old.context7, None);
    }

    #[test]
    fn a_question_maps_to_its_version_or_the_default_one() {
        let (_root, corpus) = corpus();
        let pick = |library: &str, version: Option<&str>| {
            corpus
                .scope_of(&question("q", library, version, &["x"]))
                .map(|case| case.key.clone())
        };
        assert_eq!(pick("tokio", None).unwrap(), "tokio");
        assert_eq!(pick("pydantic", None).unwrap(), "pydantic@2.12");
        assert_eq!(pick("pydantic", Some("1.10")).unwrap(), "pydantic@1.10");
        let error = pick("pydantic", Some("3")).unwrap_err();
        assert!(
            matches!(&error, EvalError::UnknownScope { id, scope } if id == "q" && scope == "pydantic@3"),
            "{error}"
        );
        assert_eq!(
            pick("vite", None).unwrap_err().to_string(),
            "question q: vite is not a library version of the corpus"
        );
    }

    #[test]
    fn without_a_default_the_first_listed_version_answers() {
        let config = CONFIG.replace("default = true\n", "");
        let root = root_with(&config, "[libraries]\n");
        let corpus = Corpus::load(root.path(), root.path()).unwrap();
        let case = corpus.scope_of(&question("q", "pydantic", None, &["x"])).unwrap();
        assert_eq!(case.key, "pydantic@1.10");
    }

    #[test]
    fn broken_corpus_files_are_reported() {
        let root = root_with("[[library]]\nid = \"Bad Id\"\n", "[libraries]\n");
        let error = Corpus::load(root.path(), root.path()).unwrap_err();
        assert!(matches!(error, EvalError::Config(_)), "{error}");

        let root = root_with(CONFIG, "[libraries]\ntokio = 1\n");
        let error = Corpus::load(root.path(), root.path()).unwrap_err();
        assert!(
            matches!(&error, EvalError::Parse { line: 0, message, .. } if message.contains("string")),
            "{error}"
        );

        let root = root_with(CONFIG, "");
        std::fs::remove_file(root.path().join("eval/context7.toml")).unwrap();
        let error = Corpus::load(root.path(), root.path()).unwrap_err();
        assert!(matches!(error, EvalError::Io { .. }), "{error}");

        let two_paths = CONFIG.replace(r#"paths = ["content"]"#, r#"paths = ["content", "blog"]"#);
        let root = root_with(&two_paths, "[libraries]\n");
        let error = Corpus::load(root.path(), root.path()).unwrap_err();
        assert!(
            matches!(&error, EvalError::UnsupportedSource(scope) if scope == "tokio@latest"),
            "{error}"
        );

        let local = CONFIG.replace(
            r#"{ kind = "github", repo = "tokio-rs/website", reference = "abc", paths = ["content"] }"#,
            r#"{ kind = "local", path = "docs" }"#,
        );
        let root = root_with(&local, "[libraries]\n");
        let error = Corpus::load(root.path(), root.path()).unwrap_err();
        assert!(matches!(error, EvalError::UnsupportedSource(_)), "{error}");
    }

    #[test]
    fn the_committed_corpus_loads() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let corpus = Corpus::load(&root, &root.join("target/eval-sources")).unwrap();
        assert!(corpus.scopes.len() >= 6);
        assert!(corpus.scopes.iter().all(|case| case.context7.is_some()));
    }

    #[test]
    fn a_relative_sources_path_becomes_absolute() {
        // Tests run in the crate directory.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let corpus = Corpus::load(&root, Path::new("sources")).unwrap();
        let expected = std::env::current_dir().unwrap().join("sources/fastapi");
        assert_eq!(corpus.scopes[0].checkout, expected);
    }
}

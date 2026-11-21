//! `Pipfile.lock`: JSON `default` and `develop` maps of `{version: "==x", index}`, with the indexes in
//! `_meta.sources`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use zenodotus_core::Ecosystem;

use super::json;
use crate::error::LockfileError;
use crate::private::{PYPI_HOSTS, registry_source};
use crate::{Entry, Source};

#[derive(Deserialize)]
struct Lockfile {
    #[serde(default, rename = "_meta")]
    meta: Meta,
    #[serde(default)]
    default: BTreeMap<String, Package>,
    #[serde(default)]
    develop: BTreeMap<String, Package>,
}

#[derive(Default, Deserialize)]
struct Meta {
    #[serde(default)]
    sources: Vec<Index>,
}

#[derive(Deserialize)]
struct Index {
    name: String,
    url: String,
}

#[derive(Deserialize)]
struct Package {
    version: Option<String>,
    index: Option<String>,
    git: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    path: Option<String>,
}

/// The URL of `index`, or the first source's when it is `None`; an index missing from `sources` keeps its name.
fn index_url<'a>(sources: &'a [Index], index: Option<&'a str>) -> Option<&'a str> {
    match index {
        None => sources.first().map(|source| source.url.as_str()),
        Some(name) => Some(
            sources
                .iter()
                .find(|source| source.name == name)
                .map_or(name, |source| source.url.as_str()),
        ),
    }
}

/// Every package of `default`, then of `develop`; `path` packages are local code. A package without `index` comes
/// from the first source, as pipenv installs it.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let lockfile: Lockfile = json(text, file)?;
    Ok(lockfile
        .default
        .iter()
        .chain(&lockfile.develop)
        .filter(|(_, package)| package.path.is_none())
        .map(|(name, package)| {
            let source = match &package.git {
                Some(url) => Source::Git {
                    url: url.clone(),
                    commit: package.reference.clone().unwrap_or_default(),
                },
                None => index_url(&lockfile.meta.sources, package.index.as_deref())
                    .map_or(Source::Registry, |url| registry_source(url, PYPI_HOSTS)),
            };
            let version = package
                .version
                .as_deref()
                .and_then(|version| version.strip_prefix("=="));
            Entry::from_source(Ecosystem::Pypi, name.clone(), version.map(str::to_owned), source)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Entry, Source};
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/pipfile/Pipfile.lock");

    #[test]
    fn default_then_develop_packages_map_to_entries() {
        let entries = parse(SAMPLE, Path::new("Pipfile.lock")).unwrap();
        let registry = |name, version| Entry::registry(Ecosystem::Pypi, name, Some(version));
        assert_eq!(
            entries,
            [
                Entry::git(
                    Ecosystem::Pypi,
                    "glab-lib",
                    "https://gitlab.com/acme/glab-lib.git",
                    "89abcdef0123456789abcdef0123456789abcdef"
                ),
                Entry {
                    source: Source::Private {
                        url: "https://pypi.acme.dev/simple".to_owned()
                    },
                    ..registry("internal-tools", "2.0.0")
                },
                registry("requests", "2.32.3"),
                Entry::registry(Ecosystem::Pypi, "unpinned", None),
                Entry::git(
                    Ecosystem::Pypi,
                    "widget",
                    "https://github.com/acme/widget.git",
                    "0123456789abcdef0123456789abcdef01234567"
                ),
                registry("pytest", "8.2.2"),
                registry("requests", "2.32.3"),
            ]
        );
    }

    #[test]
    fn the_first_source_is_the_default_index_and_an_unknown_index_is_private() {
        let text = r#"{"_meta": {"sources": [{"name": "corp", "url": "https://pypi.corp.example/simple"}]},
            "default": {"a": {"version": "==1"}, "b": {"index": "gone", "version": "==2"}}}"#;
        let entries = parse(text, Path::new("Pipfile.lock")).unwrap();
        let private = |name: &str, version: &str, url: &str| Entry {
            source: Source::Private { url: url.to_owned() },
            ..Entry::registry(Ecosystem::Pypi, name, Some(version))
        };
        assert_eq!(
            entries,
            [
                private("a", "1", "https://pypi.corp.example/simple"),
                private("b", "2", "gone")
            ]
        );
        let bare = parse(r#"{"default": {"a": {"version": "==1"}}}"#, Path::new("Pipfile.lock")).unwrap();
        assert_eq!(bare, [Entry::registry(Ecosystem::Pypi, "a", Some("1"))]);
    }

    #[test]
    fn malformed_json_is_a_parse_error_with_a_line() {
        let error = parse("{\n\"default\": {\n\"a\": }\n}", Path::new("Pipfile.lock")).err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(3), .. })),
            "{error:?}"
        );
    }

    #[test]
    fn every_pipfile_lock_entry_is_direct() {
        let root = crate::testing::root_with(&[(
            "Pipfile.lock",
            include_str!("../../../../fixtures/lockfiles/pipfile/Pipfile.lock"),
        )]);
        crate::testing::assert_all_direct(root.path());
    }
}

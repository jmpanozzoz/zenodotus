//! `gradle/libs.versions.toml`: `[libraries]` entries as `"g:a:v"` strings or `{ module | group + name, version |
//! version.ref }` tables, with the versions of `[versions]`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use zenodotus_core::Ecosystem;

use super::gradle_lockfile::coordinate;
use super::toml;
use crate::Entry;
use crate::error::LockfileError;

#[derive(Deserialize)]
struct Catalog {
    #[serde(default)]
    versions: BTreeMap<String, Version>,
    #[serde(default)]
    libraries: BTreeMap<String, Library>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Version {
    Plain(String),
    Rich(Rich),
}

#[derive(Deserialize)]
struct Rich {
    strictly: Option<String>,
    require: Option<String>,
    prefer: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Library {
    Notation(String),
    Table {
        module: Option<String>,
        group: Option<String>,
        name: Option<String>,
        version: Option<Version>,
    },
}

/// Whether `version` is a range or a dynamic version rather than one version.
fn is_range(version: &str) -> bool {
    version.contains(['[', ']', '(', ')', ',', '+'])
}

/// The one version `version` names: a plain string, or the first of `strictly`, `require` and `prefer` that is not a
/// range; `ref` is looked up in `[versions]`.
fn resolve<'a>(version: &'a Version, versions: &'a BTreeMap<String, Version>) -> Option<&'a str> {
    match version {
        Version::Plain(plain) => Some(plain.as_str()),
        Version::Rich(Rich {
            reference: Some(name), ..
        }) => match versions.get(name)? {
            Version::Plain(plain) => Some(plain.as_str()),
            Version::Rich(rich) => pick(rich),
        },
        Version::Rich(rich) => pick(rich),
    }
    .filter(|version| !is_range(version))
}

/// The first of `strictly`, `require` and `prefer` that is not a range.
fn pick(rich: &Rich) -> Option<&str> {
    [&rich.strictly, &rich.require, &rich.prefer]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .find(|version| !is_range(version))
}

/// Every library of `[libraries]`, in alias order; plugins and bundles are not libraries.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let catalog: Catalog = toml(text, file)?;
    Ok(catalog
        .libraries
        .values()
        .filter_map(|library| {
            let (name, version) = match library {
                Library::Notation(notation) => {
                    let (name, version) = coordinate(notation)?;
                    (name, Some(version))
                }
                Library::Table {
                    module,
                    group,
                    name,
                    version,
                } => {
                    let name = match (module, group, name) {
                        (Some(module), _, _) => module.clone(),
                        (None, Some(group), Some(name)) => format!("{group}:{name}"),
                        _ => return None,
                    };
                    (
                        name,
                        version.as_ref().and_then(|version| resolve(version, &catalog.versions)),
                    )
                }
            };
            Some(Entry::registry(Ecosystem::Maven, &name, version))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entry;
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/gradle/gradle/libs.versions.toml");

    #[test]
    fn libraries_map_to_entries_with_their_versions_resolved() {
        let maven = |name, version| Entry::registry(Ecosystem::Maven, name, version);
        assert_eq!(
            parse(SAMPLE, Path::new("gradle/libs.versions.toml")).unwrap(),
            [
                maven("com.google.guava:guava", Some("33.2.1-jre")),
                maven("com.fasterxml.jackson:jackson-bom", Some("2.17.1")),
                maven("org.junit.jupiter:junit-jupiter-api", Some("5.10.2")),
                maven("org.jetbrains.kotlin:kotlin-stdlib", Some("2.0.0")),
                maven("com.example:missing", None),
                maven("com.squareup.okhttp3:okhttp", Some("4.12.0")),
                maven("com.example:ranged", Some("1.5")),
                maven("com.squareup.retrofit2:retrofit", Some("2.11.0")),
                maven("org.slf4j:slf4j-api", None),
            ]
        );
    }

    #[test]
    fn malformed_toml_is_a_parse_error_with_a_line() {
        let error = parse("[libraries]\na = \"x:y:1\"\nb = {\n", Path::new("libs.versions.toml")).err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(3), .. })),
            "{error:?}"
        );
    }
}

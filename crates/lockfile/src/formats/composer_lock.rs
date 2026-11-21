//! `composer.lock`: JSON `packages[]` and `packages-dev[]` with `name`, `version` and `source`.

use std::path::Path;

use serde::Deserialize;
use zenodotus_core::Ecosystem;

use super::json;
use crate::error::LockfileError;
use crate::{Entry, Source};

#[derive(Deserialize)]
struct Lockfile {
    #[serde(default)]
    packages: Vec<Package>,
    #[serde(default, rename = "packages-dev")]
    packages_dev: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    version: Option<String>,
    source: Option<Reference>,
    dist: Option<Reference>,
}

#[derive(Deserialize)]
struct Reference {
    #[serde(rename = "type")]
    kind: String,
    url: Option<String>,
    #[serde(rename = "reference")]
    commit: Option<String>,
}

/// Every package, then every dev package. Path packages are local code; a `dev-<branch>` version is a git commit
/// when the source is git, and no version otherwise.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let lockfile: Lockfile = json(text, file)?;
    Ok(lockfile
        .packages
        .into_iter()
        .chain(lockfile.packages_dev)
        .filter(|package| package.dist.as_ref().is_none_or(|dist| dist.kind != "path"))
        .map(|package| {
            let branch = package
                .version
                .as_deref()
                .is_some_and(|version| version.starts_with("dev-"));
            match package.source {
                Some(Reference {
                    kind,
                    url: Some(url),
                    commit: Some(commit),
                }) if branch && kind == "git" => Entry::git(Ecosystem::Packagist, &package.name, &url, &commit),
                _ => {
                    let version = package.version.filter(|_| !branch);
                    Entry::from_source(Ecosystem::Packagist, package.name, version, Source::Registry)
                }
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entry;
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/composer/composer.lock");

    #[test]
    fn packages_then_dev_packages_map_to_entries() {
        let packagist = |name, version| Entry::registry(Ecosystem::Packagist, name, version);
        assert_eq!(
            parse(SAMPLE, Path::new("composer.lock")).unwrap(),
            [
                packagist("laravel/framework", Some("v11.10.0")),
                Entry::git(
                    Ecosystem::Packagist,
                    "acme/widget",
                    "https://github.com/acme/widget.git",
                    "0123456789abcdef0123456789abcdef01234567"
                ),
                packagist("acme/dev-no-source", None),
                packagist("phpunit/phpunit", Some("11.2.1")),
            ]
        );
    }

    #[test]
    fn malformed_json_is_a_parse_error_with_a_line() {
        let error = parse("{\n\"packages\": [\n{\"name\": 1}]}", Path::new("composer.lock")).err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(3), .. })),
            "{error:?}"
        );
    }
}

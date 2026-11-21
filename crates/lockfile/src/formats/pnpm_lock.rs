//! `pnpm-lock.yaml`, lockfile versions 6 to 9: every importer's versions, then `packages["<name>@<version>"]`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use zenodotus_core::Ecosystem;

use super::npm_lock::npm_resolved;
use super::yaml;
use crate::error::LockfileError;
use crate::{Entry, Source};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lockfile {
    #[serde(default)]
    importers: BTreeMap<String, Importer>,
    #[serde(default)]
    dependencies: BTreeMap<String, Dependency>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, Dependency>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, Dependency>,
    #[serde(default)]
    packages: BTreeMap<String, Package>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Importer {
    #[serde(default)]
    dependencies: BTreeMap<String, Dependency>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, Dependency>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    version: String,
}

#[derive(Deserialize)]
struct Package {
    #[serde(default)]
    resolution: Resolution,
    version: Option<String>,
}

#[derive(Deserialize, Default)]
struct Resolution {
    tarball: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    repo: Option<String>,
    commit: Option<String>,
}

/// The importers' packages, the root's first (version 9's `importers["."]`, or version 6's top-level maps), then
/// each workspace member's in path order, then every package. A package an importer names is listed before the
/// `packages` map's versions of it, so its version wins.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let mut lockfile: Lockfile = yaml(text, file)?;
    let root = lockfile.importers.remove(".").unwrap_or_else(|| Importer {
        dependencies: std::mem::take(&mut lockfile.dependencies),
        dev_dependencies: std::mem::take(&mut lockfile.dev_dependencies),
        optional_dependencies: std::mem::take(&mut lockfile.optional_dependencies),
    });
    let importer = std::iter::once(root)
        .chain(lockfile.importers.into_values())
        .flat_map(|importer| {
            [
                importer.dependencies,
                importer.dev_dependencies,
                importer.optional_dependencies,
            ]
        })
        .flatten()
        .filter_map(|(name, dependency)| importer_entry(&name, &dependency.version));
    let packages = lockfile
        .packages
        .iter()
        .filter_map(|(key, package)| package_entry(key, package));
    Ok(importer.chain(packages).collect())
}

/// An importer dependency; `link:` and other non-registry versions are left to the `packages` map.
fn importer_entry(name: &str, version: &str) -> Option<Entry> {
    let version = version.split('(').next().unwrap_or_default();
    if version.contains(':') {
        return None;
    }
    // An alias (`npm:renamed@^2`) is locked as `renamed@2.0.1`.
    let (name, version) = version.rsplit_once('@').unwrap_or((name, version));
    Some(Entry::registry(Ecosystem::Npm, name, Some(version)))
}

/// One `packages` entry; local directories and files are skipped.
fn package_entry(key: &str, package: &Package) -> Option<Entry> {
    let (name, key_version) = split_key(key)?;
    let resolution = &package.resolution;
    let source = if resolution.kind.as_deref() == Some("git") {
        Source::Git {
            url: resolution.repo.clone().unwrap_or_default(),
            commit: resolution.commit.clone().unwrap_or_default(),
        }
    } else if let Some(source) = resolution.tarball.as_deref().and_then(npm_resolved) {
        source
    } else if key_version.contains(':') {
        return None;
    } else {
        Source::Registry
    };
    Some(Entry {
        ecosystem: Ecosystem::Npm,
        name: name.to_owned(),
        version: Some(package.version.as_deref().unwrap_or(key_version).to_owned()),
        source,
        direct: false,
    })
}

/// Splits `[/]<name>@<version>[(<peers>)]` into the name and the version.
fn split_key(key: &str) -> Option<(&str, &str)> {
    let key = key.strip_prefix('/').unwrap_or(key);
    let at = key.get(1..)?.find('@')? + 1;
    let version = key[at + 1..].split('(').next().unwrap_or_default();
    Some((&key[..at], version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Source;

    fn registry(name: &str, version: &str) -> Entry {
        Entry::registry(Ecosystem::Npm, name, Some(version))
    }

    fn with_source(name: &str, version: &str, source: Source) -> Entry {
        Entry {
            source,
            ..registry(name, version)
        }
    }

    #[test]
    fn version_9_reads_every_importer_then_the_packages() {
        let text = include_str!("../../../../fixtures/lockfiles/pnpm/pnpm-lock.yaml");
        let entries = parse(text, Path::new("pnpm-lock.yaml")).unwrap();
        assert_eq!(
            entries,
            [
                registry("renamed", "2.0.1"),
                registry("react", "18.2.0"),
                registry("styled", "6.1.0"),
                registry("typescript", "5.4.5"),
                registry("react", "17.0.2"),
                with_source(
                    "@acme/secret",
                    "1.0.0",
                    Source::Private {
                        url: "https://npm.acme.dev/@acme/secret/-/secret-1.0.0.tgz".to_owned()
                    }
                ),
                registry("@babel/core", "7.24.0"),
                registry("react", "17.0.2"),
                registry("styled", "6.1.0"),
                with_source(
                    "tarball-dep",
                    "0.2.0",
                    Source::Git {
                        url: "https://github.com/acme/tarball-dep".to_owned(),
                        commit: "89abcdef0123456789abcdef0123456789abcdef".to_owned()
                    }
                ),
                with_source(
                    "widget",
                    "0.4.0",
                    Source::Git {
                        url: "https://github.com/acme/widget.git".to_owned(),
                        commit: "0123456789abcdef0123456789abcdef01234567".to_owned()
                    }
                ),
            ]
        );
    }

    #[test]
    fn version_6_reads_top_level_dependencies_and_slash_keys() {
        let text = include_str!("../../../../fixtures/lockfiles/pnpm/pnpm-lock-v6.yaml");
        assert_eq!(
            parse(text, Path::new("pnpm-lock.yaml")).unwrap(),
            [
                registry("lodash", "4.17.21"),
                registry("@types/node", "20.11.0"),
                registry("lodash", "4.17.21")
            ]
        );
    }

    #[test]
    fn keys_split_at_the_version_at_sign() {
        assert_eq!(split_key("/@scope/a@1.0.0(peer@2)"), Some(("@scope/a", "1.0.0")));
        assert_eq!(split_key("a@1"), Some(("a", "1")));
        assert_eq!(split_key("nothing"), None);
        assert_eq!(split_key("@scope/only"), None);
    }

    #[test]
    fn malformed_yaml_is_a_parse_error() {
        assert!(matches!(
            parse("packages: [\n", Path::new("pnpm-lock.yaml")),
            Err(LockfileError::Parse { line: Some(_), .. })
        ));
    }

    #[test]
    fn every_importer_is_read_root_first_so_a_member_version_wins_over_the_packages_map() {
        let text = "lockfileVersion: '9.0'\nimporters:\n  .:\n    devDependencies:\n      typescript: {specifier: ^5.4.5, version: 5.4.5}\n  packages/legacy:\n    dependencies:\n      react: {specifier: ^17.0.2, version: 17.0.2}\n  packages/app:\n    dependencies:\n      react: {specifier: ^18.3.1, version: 18.3.1}\npackages:\n  react@17.0.2: {resolution: {integrity: x}}\n  react@18.3.1: {resolution: {integrity: y}}\n";
        assert_eq!(
            parse(text, Path::new("pnpm-lock.yaml")).unwrap(),
            [
                registry("typescript", "5.4.5"),
                registry("react", "18.3.1"),
                registry("react", "17.0.2"),
                registry("react", "17.0.2"),
                registry("react", "18.3.1"),
            ]
        );
    }

    #[test]
    fn a_member_dependency_is_direct_and_a_transitive_one_is_not() {
        let root = crate::testing::fixture_root("pnpm/direct");
        assert_eq!(
            crate::testing::direct_flags(root.path()),
            [
                ("npm:typescript@5.4.5".to_owned(), true),
                ("npm:react@18.3.1".to_owned(), true),
                ("npm:js-tokens@4.0.0".to_owned(), false),
                ("npm:loose-envify@1.4.0".to_owned(), false),
            ]
        );
    }
}

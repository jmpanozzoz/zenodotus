//! `uv.lock`: TOML `[[package]]` tables with `name`, `version` and a `source` table.

use std::path::Path;

use serde::Deserialize;
use zenodotus_core::Ecosystem;

use super::{git_fragment, toml};
use crate::error::LockfileError;
use crate::private::{PYPI_HOSTS, registry_source};
use crate::{Entry, Source};

#[derive(Deserialize)]
struct Lockfile {
    #[serde(default, rename = "package")]
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    version: Option<String>,
    source: Option<PackageSource>,
}

#[derive(Deserialize)]
struct PackageSource {
    registry: Option<String>,
    git: Option<String>,
    url: Option<String>,
}

/// Every package from a registry, a URL or git; editable, directory, path and virtual packages are local code.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let lockfile: Lockfile = toml(text, file)?;
    Ok(lockfile
        .packages
        .into_iter()
        .filter_map(|package| {
            let source = match package.source {
                None => Source::Registry,
                Some(PackageSource { git: Some(git), .. }) => git_fragment(&git),
                Some(
                    PackageSource {
                        registry: Some(url), ..
                    }
                    | PackageSource { url: Some(url), .. },
                ) => registry_source(&url, PYPI_HOSTS),
                Some(_) => return None,
            };
            Some(Entry::from_source(
                Ecosystem::Pypi,
                package.name,
                package.version,
                source,
            ))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Entry, Source};
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/uv/uv.lock");

    #[test]
    fn packages_map_to_entries() {
        let entries = parse(SAMPLE, Path::new("uv.lock")).unwrap();
        let registry = |name, version| Entry::registry(Ecosystem::Pypi, name, Some(version));
        assert_eq!(
            entries,
            [
                registry("anyio", "4.4.0"),
                Entry::git(
                    Ecosystem::Pypi,
                    "glab-lib",
                    "https://gitlab.com/acme/glab-lib?rev=main",
                    "89abcdef0123456789abcdef0123456789abcdef"
                ),
                registry("Idna", "3.7"),
                Entry {
                    source: Source::Private {
                        url: "https://pypi.acme.dev/simple".to_owned()
                    },
                    ..registry("internal-tools", "2.0.0")
                },
                Entry::git(
                    Ecosystem::Pypi,
                    "widget",
                    "https://github.com/acme/widget?tag=v0.4.0",
                    "0123456789abcdef0123456789abcdef01234567"
                ),
                registry("wheel-only", "1.0.0"),
            ]
        );
    }

    #[test]
    fn a_package_without_a_source_is_from_the_registry_and_malformed_toml_has_a_line() {
        let entries = parse("[[package]]\nname = \"a\"\nversion = \"1\"\n", Path::new("uv.lock")).unwrap();
        assert_eq!(entries, [Entry::registry(Ecosystem::Pypi, "a", Some("1"))]);
        let error = parse("version = 1\n[[package]]\nname = \n", Path::new("uv.lock")).err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(3), .. })),
            "{error:?}"
        );
    }

    #[test]
    fn a_declared_dependency_is_direct_and_a_transitive_one_is_not() {
        let root = crate::testing::fixture_root("uv/direct");
        assert_eq!(
            crate::testing::direct_flags(root.path()),
            [
                ("pypi:pytest@8.2.2".to_owned(), true),
                ("pypi:requests@2.32.3".to_owned(), true),
                ("pypi:urllib3@2.2.2".to_owned(), false),
                (
                    "github:acme/widget@0123456789abcdef0123456789abcdef01234567".to_owned(),
                    true
                ),
            ]
        );
    }
}

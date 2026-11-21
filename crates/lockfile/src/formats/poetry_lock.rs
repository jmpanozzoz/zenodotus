//! `poetry.lock`: TOML `[[package]]` tables; `[package.source]` marks git, legacy-index and local packages.

use std::path::Path;

use serde::Deserialize;
use zenodotus_core::Ecosystem;

use super::toml;
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
    #[serde(rename = "type")]
    kind: String,
    url: String,
    resolved_reference: Option<String>,
}

/// Every package from the Python Package Index, another index or git; directory, file and URL packages are local code.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let lockfile: Lockfile = toml(text, file)?;
    Ok(lockfile
        .packages
        .into_iter()
        .filter_map(|package| {
            let source = match package.source {
                None => Source::Registry,
                Some(source) => match source.kind.as_str() {
                    "git" => Source::Git {
                        url: source.url,
                        commit: source.resolved_reference.unwrap_or_default(),
                    },
                    "legacy" | "explicit" | "supplemental" | "primary" => registry_source(&source.url, PYPI_HOSTS),
                    _ => return None,
                },
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

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/poetry/poetry.lock");

    #[test]
    fn packages_map_to_entries() {
        let entries = parse(SAMPLE, Path::new("poetry.lock")).unwrap();
        let registry = |name, version| Entry::registry(Ecosystem::Pypi, name, Some(version));
        assert_eq!(
            entries,
            [
                registry("certifi", "2024.7.4"),
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
                Entry::git(
                    Ecosystem::Pypi,
                    "widget",
                    "https://github.com/acme/widget.git",
                    "0123456789abcdef0123456789abcdef01234567"
                ),
                registry("pypi-mirror-pkg", "1.1.0"),
            ]
        );
    }

    #[test]
    fn malformed_toml_is_a_parse_error_with_a_line() {
        let error = parse("[[package]]\nname = \"a\"\nversion = [\n", Path::new("poetry.lock")).err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(3), .. })),
            "{error:?}"
        );
    }
}

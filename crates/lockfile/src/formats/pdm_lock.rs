//! `pdm.lock`: TOML `[[package]]` tables; `git` with `revision`, `url` and `path` keys mark non-index packages.

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
    git: Option<String>,
    revision: Option<String>,
    url: Option<String>,
    path: Option<String>,
}

/// Every package from an index, a URL or git; `path` packages are local code. The lockfile names no index, so index
/// packages count as public unless `private_scopes` says otherwise.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let lockfile: Lockfile = toml(text, file)?;
    Ok(lockfile
        .packages
        .into_iter()
        .filter(|package| package.path.is_none())
        .map(|package| {
            let source = match (package.git, package.url) {
                (Some(url), _) => Source::Git {
                    url,
                    commit: package.revision.unwrap_or_default(),
                },
                (None, Some(url)) => registry_source(&url, PYPI_HOSTS),
                (None, None) => Source::Registry,
            };
            Entry::from_source(Ecosystem::Pypi, package.name, package.version, source)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Entry, Source};
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/pdm/pdm.lock");

    #[test]
    fn packages_map_to_entries() {
        let entries = parse(SAMPLE, Path::new("pdm.lock")).unwrap();
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
                        url: "https://files.acme.dev/private_wheel-3.0.0-py3-none-any.whl".to_owned()
                    },
                    ..registry("private-wheel", "3.0.0")
                },
                registry("pytest", "8.2.2"),
                registry("requests", "2.32.3"),
                Entry::git(
                    Ecosystem::Pypi,
                    "widget",
                    "https://github.com/acme/widget.git",
                    "0123456789abcdef0123456789abcdef01234567"
                ),
            ]
        );
    }

    #[test]
    fn malformed_toml_is_a_parse_error_with_a_line() {
        let error = parse("[metadata]\n\n[[package]\n", Path::new("pdm.lock")).err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(3), .. })),
            "{error:?}"
        );
    }
}

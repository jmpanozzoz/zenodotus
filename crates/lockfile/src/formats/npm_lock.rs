//! `package-lock.json` and `npm-shrinkwrap.json`, lockfile versions 2 and 3: `packages["node_modules/<name>"]`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use zenodotus_core::Ecosystem;

use super::{json, parse_error};
use crate::error::LockfileError;
use crate::private::{NPM_REGISTRIES, registry_source};
use crate::{Entry, Source};

#[derive(Deserialize)]
struct Lockfile {
    packages: Option<BTreeMap<String, Package>>,
}

#[derive(Deserialize)]
struct Package {
    name: Option<String>,
    version: Option<String>,
    resolved: Option<String>,
    #[serde(default)]
    link: bool,
}

/// The source of an npm `resolved` value: a registry tarball, a git URL with its commit after `#`, a GitHub
/// `codeload` tarball, or `None` for a local path.
pub(crate) fn npm_resolved(resolved: &str) -> Option<Source> {
    let git = |url: &str| {
        let (url, commit) = url.split_once('#').unwrap_or((url, ""));
        Source::Git {
            url: url.to_owned(),
            commit: commit.to_owned(),
        }
    };
    if let Some(repository) = resolved.strip_prefix("github:") {
        Some(git(&format!("https://github.com/{repository}")))
    } else if let Some(path) = resolved.strip_prefix("https://codeload.github.com/") {
        // `<owner>/<repo>/tar.gz/<commit>`
        let mut parts = path.split('/');
        let (owner, repository, commit) = (parts.next()?, parts.next()?, parts.nth(1)?);
        Some(git(&format!("https://github.com/{owner}/{repository}#{commit}")))
    } else if resolved.starts_with("git+") || resolved.starts_with("git:") {
        Some(git(resolved))
    } else if resolved.starts_with("https://") || resolved.starts_with("http://") {
        Some(registry_source(resolved, NPM_REGISTRIES))
    } else {
        None
    }
}

/// Every installed package; root-level `node_modules/<name>` entries come before nested ones, so they win.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let lockfile: Lockfile = json(text, file)?;
    let packages = lockfile
        .packages
        .ok_or_else(|| parse_error(file, None, "no `packages` map: only lockfile versions 2 and 3 are read"))?;
    let (root_level, nested): (Vec<_>, Vec<_>) = packages
        .iter()
        .filter(|(key, _)| key.contains("node_modules/"))
        .partition(|(key, _)| key.matches("node_modules/").count() == 1);
    Ok(root_level
        .into_iter()
        .chain(nested)
        .filter_map(|(key, package)| entry(key, package))
        .collect())
}

/// The entry of one `packages` key, skipping links and local paths.
fn entry(key: &str, package: &Package) -> Option<Entry> {
    if package.link {
        return None;
    }
    let name = package
        .name
        .as_deref()
        .unwrap_or_else(|| key.rsplit_once("node_modules/").map_or(key, |(_, name)| name));
    let source = match &package.resolved {
        None => Source::Registry,
        Some(resolved) => npm_resolved(resolved)?,
    };
    Some(Entry {
        ecosystem: Ecosystem::Npm,
        name: name.to_owned(),
        version: package.version.clone(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Source;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/npm/package-lock.json");
    const WIDGET: &str = "0123456789abcdef0123456789abcdef01234567";

    fn registry(name: &str, version: &str) -> Entry {
        Entry::registry(Ecosystem::Npm, name, Some(version))
    }

    fn with_version(mut entry: Entry, version: &str) -> Entry {
        entry.version = Some(version.to_owned());
        entry
    }

    #[test]
    fn root_level_entries_come_first_then_nested_ones() {
        let entries = parse(SAMPLE, Path::new("package-lock.json")).unwrap();
        let mut internal = registry("@acme/internal", "2.1.0");
        internal.source = Source::Private {
            url: "https://npm.acme.dev/@acme/internal/-/internal-2.1.0.tgz".to_owned(),
        };
        assert_eq!(
            entries,
            [
                internal,
                registry("ansi-regex", "6.0.1"),
                registry("bundled", "1.0.0"),
                with_version(
                    Entry::git(Ecosystem::Npm, "glab-lib", "git+https://gitlab.com/acme/glab-lib.git",
                        "89abcdef0123456789abcdef0123456789abcdef"),
                    "1.0.0"
                ),
                registry("left-pad", "1.3.0"),
                registry("real-package", "2.0.0"),
                registry("string-width", "4.2.3"),
                with_version(Entry::git(Ecosystem::Npm, "widget", "git+ssh://git@github.com/acme/widget.git", WIDGET), "0.4.0"),
                registry("ansi-regex", "5.0.1"),
            ]
        );
    }

    #[test]
    fn a_version_1_lockfile_is_a_parse_error() {
        let error = parse("{\"lockfileVersion\": 1, \"dependencies\": {}}", Path::new("package-lock.json")).err();
        assert!(matches!(error, Some(LockfileError::Parse { line: None, .. })), "{error:?}");
        assert!(parse("[]", Path::new("package-lock.json")).is_err());
    }

    #[test]
    fn npm_resolved_forms_map_to_sources() {
        assert_eq!(npm_resolved("https://registry.npmjs.org/a/-/a-1.tgz"), Some(Source::Registry));
        assert_eq!(
            npm_resolved("github:acme/widget#abc"),
            Some(Source::Git { url: "https://github.com/acme/widget".to_owned(), commit: "abc".to_owned() })
        );
        assert_eq!(
            npm_resolved("git://example.com/a.git#abc"),
            Some(Source::Git { url: "git://example.com/a.git".to_owned(), commit: "abc".to_owned() })
        );
        assert_eq!(
            npm_resolved("git+https://github.com/a/b.git"),
            Some(Source::Git { url: "git+https://github.com/a/b.git".to_owned(), commit: String::new() })
        );
        for local in ["file:../x", "link:../x", "workspace:*", "packages/x"] {
            assert_eq!(npm_resolved(local), None, "{local}");
        }
    }
}

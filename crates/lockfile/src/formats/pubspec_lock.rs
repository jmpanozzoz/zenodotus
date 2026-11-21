//! `pubspec.lock`: YAML `packages.<name>` with `source` (`hosted`, `git`, `path`, `sdk`), `description` and
//! `version`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use zenodotus_core::Ecosystem;

use super::yaml;
use crate::error::LockfileError;
use crate::private::{PUB_HOSTS, registry_source};
use crate::{Entry, Source};

#[derive(Deserialize)]
struct Lockfile {
    #[serde(default)]
    packages: BTreeMap<String, Package>,
}

#[derive(Deserialize)]
struct Package {
    /// `direct main`, `direct dev`, `direct overridden` or `transitive`.
    #[serde(default)]
    dependency: String,
    source: String,
    description: Description,
    version: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Description {
    Detailed {
        url: Option<String>,
        #[serde(rename = "resolved-ref")]
        resolved_ref: Option<String>,
    },
    Plain(serde::de::IgnoredAny),
}

/// Every hosted and git package, direct when its `dependency` says so; path and SDK packages are not from pub.dev.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let lockfile: Lockfile = yaml(text, file)?;
    Ok(lockfile
        .packages
        .into_iter()
        .filter_map(|(name, package)| {
            let (url, commit) = match package.description {
                Description::Detailed { url, resolved_ref } => (url, resolved_ref),
                Description::Plain(_) => (None, None),
            };
            let source = match package.source.as_str() {
                "hosted" => url.map_or(Source::Registry, |url| registry_source(&url, PUB_HOSTS)),
                "git" => Source::Git {
                    url: url.unwrap_or_default(),
                    commit: commit.unwrap_or_default(),
                },
                _ => return None,
            };
            let entry = Entry::from_source(Ecosystem::Pub, name, package.version, source);
            Some(entry.with_direct(package.dependency.starts_with("direct ")))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Entry, Source};
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/pubspec/pubspec.lock");

    #[test]
    fn hosted_and_git_packages_map_to_entries() {
        let package = |name, version| Entry::registry(Ecosystem::Pub, name, Some(version));
        assert_eq!(
            parse(SAMPLE, Path::new("pubspec.lock")).unwrap(),
            [
                Entry {
                    source: Source::Private {
                        url: "https://pub.acme.dev".to_owned()
                    },
                    ..package("acme_internal", "2.0.0").with_direct(true)
                },
                package("collection", "1.18.0"),
                Entry::git(
                    Ecosystem::Pub,
                    "glab_lib",
                    "https://gitlab.com/acme/glab_lib.git",
                    "89abcdef0123456789abcdef0123456789abcdef"
                )
                .with_direct(true),
                package("http", "1.2.1").with_direct(true),
                Entry::git(
                    Ecosystem::Pub,
                    "widget",
                    "https://github.com/acme/widget.git",
                    "0123456789abcdef0123456789abcdef01234567"
                )
                .with_direct(true),
            ]
        );
    }

    #[test]
    fn a_hosted_package_without_a_url_is_public_and_malformed_yaml_has_a_line() {
        let text = "packages:\n  a:\n    source: hosted\n    description: a\n    version: \"1.0.0\"\n";
        assert_eq!(
            parse(text, Path::new("pubspec.lock")).unwrap(),
            [Entry::registry(Ecosystem::Pub, "a", Some("1.0.0"))]
        );
        let error = parse(
            "packages:\n  a:\n    source: [1]\n    description: a\n",
            Path::new("pubspec.lock"),
        )
        .err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(3), .. })),
            "{error:?}"
        );
    }

    #[test]
    fn direct_main_dev_and_overridden_packages_are_direct() {
        let root = crate::testing::root_with(&[(
            "pubspec.lock",
            include_str!("../../../../fixtures/lockfiles/pubspec/pubspec.lock"),
        )]);
        assert_eq!(
            crate::testing::direct_flags(root.path()),
            [
                ("pub:collection@1.18.0".to_owned(), false),
                ("pub:http@1.2.1".to_owned(), true),
                (
                    "github:acme/widget@0123456789abcdef0123456789abcdef01234567".to_owned(),
                    true
                ),
            ]
        );
        let overridden = "packages:\n  meta:\n    dependency: \"direct overridden\"\n    description: {name: meta, url: \"https://pub.dev\"}\n    source: hosted\n    version: \"1.15.0\"\n";
        let entries = parse(overridden, Path::new("pubspec.lock")).unwrap();
        assert!(entries[0].direct, "{entries:?}");
    }
}

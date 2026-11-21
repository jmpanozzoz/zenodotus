//! `yarn.lock`: classic v1 (indented text blocks) and Berry (YAML with `__metadata`).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use zenodotus_core::Ecosystem;

use super::npm_lock::npm_resolved;
use super::{parse_error, yaml};
use crate::error::LockfileError;
use crate::{Entry, Source};

/// Every resolved package, in file order for v1 and key order for Berry.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    if text.lines().any(|line| line == "__metadata:") {
        berry(text, file)
    } else {
        classic(text, file)
    }
}

/// The package name of a v1 spec such as `"@scope/name@^1"` or the alias `old@npm:new@^1`.
fn spec_name(spec: &str) -> Option<&str> {
    let spec = spec.trim_matches('"');
    let at = spec.get(1..)?.find('@')? + 1;
    match spec[at + 1..].strip_prefix("npm:") {
        Some(aliased) if aliased.get(1..).is_some_and(|rest| rest.contains('@')) => spec_name(aliased),
        _ => Some(&spec[..at]),
    }
}

/// The `commit=` parameter of a Berry git resolution's fragment.
fn berry_commit(url: &str) -> Option<&str> {
    let fragment = url.split_once('#')?.1;
    fragment
        .split('&')
        .find_map(|parameter| parameter.strip_prefix("commit="))
}

/// One v1 block: its first spec and the fields that matter.
struct Block {
    spec: String,
    version: Option<String>,
    resolved: Option<String>,
}

impl Block {
    /// The entry of a block with a registry or git `resolved`; local packages have none.
    fn entry(self) -> Option<Entry> {
        let name = spec_name(&self.spec)?;
        let source = npm_resolved(self.resolved.as_deref()?)?;
        Some(Entry {
            ecosystem: Ecosystem::Npm,
            name: name.to_owned(),
            version: self.version,
            source,
            direct: false,
        })
    }
}

/// Classic v1: unindented `spec, spec:` headers, then two-space `version` and `resolved` fields.
fn classic(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let mut entries = Vec::new();
    let mut block: Option<Block> = None;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') {
            entries.extend(block.take().and_then(Block::entry));
            let spec = line.trim_end_matches(':').split(", ").next().unwrap_or_default();
            block = Some(Block {
                spec: spec.to_owned(),
                version: None,
                resolved: None,
            });
            continue;
        }
        let Some(current) = block.as_mut() else {
            return Err(parse_error(file, Some(index as u64 + 1), "a field outside any block"));
        };
        let Some(field) = line.strip_prefix("  ").filter(|field| !field.starts_with(' ')) else {
            continue;
        };
        if let Some(version) = field.strip_prefix("version ") {
            current.version = Some(version.trim_matches('"').to_owned());
        } else if let Some(resolved) = field.strip_prefix("resolved ") {
            current.resolved = Some(resolved.trim_matches('"').to_owned());
        }
    }
    entries.extend(block.and_then(Block::entry));
    Ok(entries)
}

#[derive(Deserialize)]
struct BerryEntry {
    version: Option<String>,
    resolution: Option<String>,
}

/// Berry: `resolution: "<name>@npm:<version>"` for registry packages, `#commit=` for git; workspaces, patches and
/// links are skipped.
fn berry(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    // `__metadata` has no `resolution`, so it yields no entry.
    let entries: BTreeMap<String, BerryEntry> = yaml(text, file)?;
    Ok(entries.values().filter_map(berry_entry).collect())
}

/// The entry of one Berry block.
fn berry_entry(entry: &BerryEntry) -> Option<Entry> {
    let resolution = entry.resolution.as_deref()?;
    let at = resolution.get(1..)?.find('@')? + 1;
    let (name, locator) = (&resolution[..at], &resolution[at + 1..]);
    let source = if locator.starts_with("npm:") {
        Source::Registry
    } else {
        let commit = berry_commit(locator)?;
        Source::Git {
            url: locator.split('#').next().unwrap_or_default().to_owned(),
            commit: commit.to_owned(),
        }
    };
    Some(Entry {
        ecosystem: Ecosystem::Npm,
        name: name.to_owned(),
        version: entry.version.clone(),
        source,
        direct: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Source;

    const WIDGET: &str = "0123456789abcdef0123456789abcdef01234567";

    fn registry(name: &str, version: &str) -> Entry {
        Entry::registry(Ecosystem::Npm, name, Some(version))
    }

    fn with_source(name: &str, version: &str, source: Source) -> Entry {
        Entry {
            source,
            ..registry(name, version)
        }
    }

    fn git(url: &str, commit: &str) -> Source {
        Source::Git {
            url: url.to_owned(),
            commit: commit.to_owned(),
        }
    }

    #[test]
    fn classic_v1_blocks_map_to_entries() {
        let text = include_str!("../../../../fixtures/lockfiles/yarn/yarn-v1.lock");
        assert_eq!(
            parse(text, Path::new("yarn.lock")).unwrap(),
            [
                registry("@babel/code-frame", "7.22.13"),
                registry("lodash", "4.17.21"),
                with_source(
                    "@acme/secret",
                    "1.0.0",
                    Source::Private {
                        url: "https://npm.acme.dev/@acme/secret/-/secret-1.0.0.tgz#cccc".to_owned()
                    }
                ),
                with_source("widget", "0.4.0", git("https://github.com/acme/widget", WIDGET)),
                registry("renamed", "2.0.1"),
                with_source(
                    "glab",
                    "1.0.0",
                    git(
                        "git+https://gitlab.com/acme/glab.git",
                        "89abcdef0123456789abcdef0123456789abcdef"
                    )
                ),
            ]
        );
    }

    #[test]
    fn berry_resolutions_map_to_entries() {
        let text = include_str!("../../../../fixtures/lockfiles/yarn/yarn-berry.lock");
        assert_eq!(
            parse(text, Path::new("yarn.lock")).unwrap(),
            [
                registry("@babel/code-frame", "7.22.13"),
                registry("lodash", "4.17.21"),
                registry("renamed", "2.0.1"),
                with_source("widget", "0.4.0", git("https://github.com/acme/widget.git", WIDGET)),
            ]
        );
    }

    #[test]
    fn a_v1_field_outside_a_block_is_a_parse_error_with_its_line() {
        let error = parse("# yarn lockfile v1\n  version \"1\"\n", Path::new("yarn.lock")).err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(2), .. })),
            "{error:?}"
        );
    }

    #[test]
    fn spec_names_handle_scopes_and_aliases() {
        assert_eq!(spec_name("\"@a/b@^1\""), Some("@a/b"));
        assert_eq!(spec_name("a@npm:@b/c@^1"), Some("@b/c"));
        assert_eq!(spec_name("plain"), None);
        assert_eq!(berry_commit("https://x/y.git#branch=main&commit=abc&x=1"), Some("abc"));
        assert_eq!(berry_commit("https://x/y.git#main"), None);
    }

    #[test]
    fn a_member_dependency_is_direct_and_a_transitive_one_is_not() {
        let root = crate::testing::fixture_root("yarn/direct");
        assert_eq!(
            crate::testing::direct_flags(root.path()),
            [
                ("npm:js-tokens@4.0.0".to_owned(), false),
                ("npm:loose-envify@1.4.0".to_owned(), false),
                ("npm:react@18.3.1".to_owned(), true),
                ("npm:typescript@5.4.5".to_owned(), true),
            ]
        );
        let berry = "__metadata:\n  version: 8\n\n\"app@workspace:packages/app\":\n  version: 0.0.0-use.local\n  resolution: \"app@workspace:packages/app\"\n\n\"loose-envify@npm:^1.1.0\":\n  version: 1.4.0\n  resolution: \"loose-envify@npm:1.4.0\"\n\n\"react@npm:^18.3.1\":\n  version: 18.3.1\n  resolution: \"react@npm:18.3.1\"\n";
        std::fs::write(root.path().join("yarn.lock"), berry).unwrap();
        assert_eq!(
            crate::testing::direct_flags(root.path()),
            [
                ("npm:loose-envify@1.4.0".to_owned(), false),
                ("npm:react@18.3.1".to_owned(), true),
            ],
            "Berry"
        );
    }
}

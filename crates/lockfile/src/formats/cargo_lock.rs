//! `Cargo.lock` v3 and v4: `[[package]]` tables whose `source` is `registry+`, `sparse+` or `git+url#commit`.

use std::path::Path;

use serde::Deserialize;
use zenodotus_core::Ecosystem;

use super::{git_fragment, toml};
use crate::error::LockfileError;
use crate::{Entry, Source};

/// The crates.io index, as a git and as a sparse registry.
const CRATES_IO: &[&str] = &["https://github.com/rust-lang/crates.io-index", "https://index.crates.io/"];

#[derive(Deserialize)]
struct Lockfile {
    #[serde(default, rename = "package")]
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
    source: Option<String>,
}

/// Every package with a source; packages without one are workspace members or path dependencies.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let lockfile: Lockfile = toml(text, file)?;
    Ok(lockfile
        .packages
        .into_iter()
        .filter_map(|package| {
            let source = package.source?;
            let source = if let Some(url) = source.strip_prefix("git+") {
                git_fragment(url)
            } else {
                let url = source.strip_prefix("registry+").or_else(|| source.strip_prefix("sparse+"))?;
                if CRATES_IO.contains(&url) {
                    Source::Registry
                } else {
                    Source::Private { url: url.to_owned() }
                }
            };
            Some(Entry::from_source(Ecosystem::Crates, package.name, Some(package.version), source))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Entry, Source};
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/cargo/Cargo.lock");

    #[test]
    fn packages_map_to_entries_and_workspace_members_are_skipped() {
        let crates = |name, version| Entry::registry(Ecosystem::Crates, name, Some(version));
        assert_eq!(
            parse(SAMPLE, Path::new("Cargo.lock")).unwrap(),
            [
                Entry::git(
                    Ecosystem::Crates,
                    "glab-lib",
                    "https://gitlab.com/acme/glab-lib?branch=main",
                    "89abcdef0123456789abcdef0123456789abcdef"
                ),
                Entry {
                    source: Source::Private { url: "https://my.registry.example/index".to_owned() },
                    ..crates("internal", "2.0.0")
                },
                crates("serde", "1.0.203"),
                crates("serde_json", "1.0.117"),
                Entry::git(
                    Ecosystem::Crates,
                    "widget",
                    "https://github.com/acme/widget.git?tag=v0.4.0",
                    "0123456789abcdef0123456789abcdef01234567"
                ),
            ]
        );
    }

    #[test]
    fn an_unknown_source_kind_is_skipped_and_malformed_toml_has_a_line() {
        let text = "[[package]]\nname = \"a\"\nversion = \"1\"\nsource = \"path+file:///x\"\n";
        assert!(parse(text, Path::new("Cargo.lock")).unwrap().is_empty());
        let error = parse("version = 4\n\n[[package]]\nname = 1\n", Path::new("Cargo.lock")).err();
        assert!(matches!(error, Some(LockfileError::Parse { line: Some(4), .. })), "{error:?}");
    }
}

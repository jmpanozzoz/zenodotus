//! `Package.resolved` v1 (`object.pins[]` with `repositoryURL`) and v2/v3 (`pins[]` with `location` and `kind`):
//! every remote pin is a git dependency at its `revision`.

use std::path::Path;

use serde::Deserialize;
use zenodotus_core::Ecosystem;

use super::{json, parse_error};
use crate::Entry;
use crate::error::LockfileError;

#[derive(Deserialize)]
struct Resolved {
    pins: Option<Vec<Pin>>,
    object: Option<Object>,
}

#[derive(Deserialize)]
struct Object {
    pins: Vec<Pin>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pin {
    #[serde(alias = "package")]
    identity: String,
    kind: Option<String>,
    #[serde(alias = "repositoryURL")]
    location: String,
    state: State,
}

#[derive(Deserialize)]
struct State {
    revision: Option<String>,
}

/// Every remote source-control pin; local, file-system and registry pins are skipped.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let resolved: Resolved = json(text, file)?;
    let pins = resolved
        .pins
        .or(resolved.object.map(|object| object.pins))
        .ok_or_else(|| parse_error(file, None, "no `pins` list"))?;
    Ok(pins
        .into_iter()
        .filter(|pin| pin.kind.as_deref().is_none_or(|kind| kind == "remoteSourceControl"))
        .map(|pin| {
            let commit = pin.state.revision.unwrap_or_default();
            Entry::git(Ecosystem::Swift, &pin.identity, &pin.location, &commit)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entry;
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    #[test]
    fn version_3_remote_pins_map_to_git_entries() {
        let text = include_str!("../../../../fixtures/lockfiles/swift/Package.resolved");
        assert_eq!(
            parse(text, Path::new("Package.resolved")).unwrap(),
            [
                Entry::git(
                    Ecosystem::Swift,
                    "swift-nio",
                    "https://github.com/apple/swift-nio.git",
                    "0123456789abcdef0123456789abcdef01234567"
                ),
                Entry::git(
                    Ecosystem::Swift,
                    "glab-kit",
                    "https://gitlab.com/acme/glab-kit.git",
                    "89abcdef0123456789abcdef0123456789abcdef"
                ),
            ]
        );
    }

    #[test]
    fn version_1_pins_map_to_git_entries() {
        let text = include_str!("../../../../fixtures/lockfiles/swift/Package.resolved.v1");
        assert_eq!(
            parse(text, Path::new("Package.resolved")).unwrap(),
            [Entry::git(
                Ecosystem::Swift,
                "swift-argument-parser",
                "https://github.com/apple/swift-argument-parser",
                "fedcba9876543210fedcba9876543210fedcba98"
            )]
        );
    }

    #[test]
    fn a_file_without_pins_is_a_parse_error() {
        let error = parse("{\"version\": 2}", Path::new("Package.resolved")).err();
        assert!(matches!(error, Some(LockfileError::Parse { line: None, .. })), "{error:?}");
        let error = parse("{\n\"pins\": 1}", Path::new("Package.resolved")).err();
        assert!(matches!(error, Some(LockfileError::Parse { line: Some(2), .. })), "{error:?}");
    }
}

//! `packages.lock.json`: `dependencies.<target framework>.<id>.resolved`; project references are skipped.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use zenodotus_core::Ecosystem;

use super::json;
use crate::Entry;
use crate::error::LockfileError;

#[derive(Deserialize)]
struct Lockfile {
    #[serde(default)]
    dependencies: BTreeMap<String, BTreeMap<String, Package>>,
}

#[derive(Deserialize)]
struct Package {
    #[serde(rename = "type")]
    kind: Option<String>,
    resolved: Option<String>,
}

/// Every package of every target framework, frameworks and ids in ordinal order.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let lockfile: Lockfile = json(text, file)?;
    Ok(lockfile
        .dependencies
        .values()
        .flatten()
        .filter(|(_, package)| package.kind.as_deref() != Some("Project"))
        .map(|(id, package)| Entry::registry(Ecosystem::Nuget, id, package.resolved.as_deref()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entry;
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/nuget/packages.lock.json");

    #[test]
    fn packages_of_every_framework_map_to_entries() {
        let nuget = |name, version| Entry::registry(Ecosystem::Nuget, name, Some(version));
        assert_eq!(
            parse(SAMPLE, Path::new("packages.lock.json")).unwrap(),
            [
                nuget("Newtonsoft.Json", "13.0.3"),
                nuget("Serilog", "3.1.1"),
                nuget("Newtonsoft.Json", "13.0.3"),
                nuget("System.Text.Json", "9.0.0"),
            ]
        );
    }

    #[test]
    fn malformed_json_is_a_parse_error_with_a_line() {
        let error = parse(
            "{\n\"dependencies\": {\n\"net8.0\": 5\n}}",
            Path::new("packages.lock.json"),
        )
        .err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(3), .. })),
            "{error:?}"
        );
    }
}

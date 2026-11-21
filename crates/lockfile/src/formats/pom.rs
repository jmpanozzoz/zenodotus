//! `pom.xml`: `<dependency>` elements of `<dependencies>` and `<dependencyManagement>`, with `${prop}` taken from
//! `<properties>` in the same file only.

use std::path::Path;

use zenodotus_core::Ecosystem;

use super::xml::{self, Element};
use crate::Entry;
use crate::error::LockfileError;

/// `text` with every `${name}` replaced from `properties`; `None` when one is not defined there.
fn interpolate(text: &str, properties: Option<&Element>) -> Option<String> {
    let mut resolved = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let (name, after) = rest[start + 2..].split_once('}')?;
        resolved.push_str(&rest[..start]);
        resolved.push_str(properties?.child_text(name)?);
        rest = after;
    }
    resolved.push_str(rest);
    Some(resolved)
}

/// The entry of one `<dependency>`; a version range or an unresolved property leaves the version out.
fn entry(dependency: &Element, properties: Option<&Element>) -> Option<Entry> {
    let group = interpolate(dependency.child_text("groupId")?, properties)?;
    let artifact = interpolate(dependency.child_text("artifactId")?, properties)?;
    let version = dependency
        .child_text("version")
        .and_then(|version| interpolate(version, properties))
        .filter(|version| !version.contains(['[', '(', ',']));
    Some(Entry::registry(Ecosystem::Maven, &format!("{group}:{artifact}"), version.as_deref()))
}

/// The managed dependencies, then the dependencies, in file order.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let project = xml::parse(text, file)?;
    let properties = project.child("properties");
    let managed = project.child("dependencyManagement").and_then(|management| management.child("dependencies"));
    Ok(managed
        .into_iter()
        .chain(project.child("dependencies"))
        .flat_map(|dependencies| dependencies.children_named("dependency"))
        .filter_map(|dependency| entry(dependency, properties))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entry;
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/pom/pom.xml");

    fn maven(name: &str, version: Option<&str>) -> Entry {
        Entry::registry(Ecosystem::Maven, name, version)
    }

    #[test]
    fn dependencies_map_to_entries_with_properties_resolved() {
        let entries = parse(SAMPLE, Path::new("pom.xml")).unwrap();
        assert_eq!(
            entries,
            [
                maven("com.fasterxml.jackson:jackson-bom", Some("2.17.1")),
                maven("org.springframework:spring-core", Some("6.1.10")),
                maven("com.fasterxml.jackson.core:jackson-databind", None),
                maven("org.junit.jupiter:junit-jupiter", Some("5.10.2")),
                maven("com.example:sibling", None),
                maven("org.slf4j:slf4j-api", None),
            ]
        );
    }

    #[test]
    fn a_pom_without_dependencies_has_none_and_malformed_xml_is_a_parse_error() {
        assert!(parse("<project/>", Path::new("pom.xml")).unwrap().is_empty());
        let error = parse("<project>\n<dependencies>\n</project>", Path::new("pom.xml")).err();
        assert!(matches!(error, Some(LockfileError::Parse { line: Some(3), .. })), "{error:?}");
    }
}

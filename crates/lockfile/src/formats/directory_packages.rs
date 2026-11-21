//! `Directory.Packages.props`: `<PackageVersion Include Version>` and `<GlobalPackageReference>` items.

use std::path::Path;

use zenodotus_core::Ecosystem;

use super::xml::{self, Element};
use crate::Entry;
use crate::error::LockfileError;

/// The items called one of `kinds` in every `<ItemGroup>` of `project`: the id from `Include`, the version from the
/// `Version` or `VersionOverride` attribute or a `<Version>` child. A build property (`$(X)`) or a range leaves
/// the version out.
pub(crate) fn package_items(project: &Element, kinds: &[&str]) -> Vec<Entry> {
    project
        .children_named("ItemGroup")
        .flat_map(|group| group.children.iter())
        .filter(|item| kinds.contains(&item.name.as_str()))
        .filter_map(|item| {
            let id = item.attribute("Include")?;
            let version = item
                .attribute("Version")
                .or_else(|| item.attribute("VersionOverride"))
                .or_else(|| item.child_text("Version"))
                .filter(|version| !version.contains(['$', '[', '(', ',']));
            Some(Entry::registry(Ecosystem::Nuget, id, version))
        })
        .collect()
}

/// Every centrally managed version and global package reference, in file order.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let project = xml::parse(text, file)?;
    Ok(package_items(&project, &["PackageVersion", "GlobalPackageReference"]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entry;
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/nuget/Directory.Packages.props");

    #[test]
    fn package_versions_map_to_entries() {
        let nuget = |name, version| Entry::registry(Ecosystem::Nuget, name, version);
        assert_eq!(
            parse(SAMPLE, Path::new("Directory.Packages.props")).unwrap(),
            [
                nuget("Newtonsoft.Json", Some("13.0.3")),
                nuget("Serilog", Some("3.1.1")),
                nuget("xunit", None),
                nuget("Polly", None),
                nuget("Nerdbank.GitVersioning", Some("3.6.133")),
            ]
        );
    }

    #[test]
    fn malformed_xml_is_a_parse_error() {
        let error = parse(
            "<Project>\n<ItemGroup>\n</Project>",
            Path::new("Directory.Packages.props"),
        )
        .err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(3), .. })),
            "{error:?}"
        );
    }
}

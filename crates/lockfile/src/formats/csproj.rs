//! `*.csproj`: `<PackageReference Include Version>` items and the target frameworks.

use std::path::Path;

use super::directory_packages::package_items;
use super::xml;
use crate::Entry;
use crate::error::LockfileError;

/// What a project file declares.
#[derive(Debug)]
pub(crate) struct CsProject {
    /// Its package references, in file order.
    pub(crate) entries: Vec<Entry>,
    /// Its target framework monikers, such as `net8.0`, in file order.
    pub(crate) frameworks: Vec<String>,
}

/// The package references and target frameworks of a project file.
pub(crate) fn parse(text: &str, file: &Path) -> Result<CsProject, LockfileError> {
    let project = xml::parse(text, file)?;
    let frameworks = project
        .children_named("PropertyGroup")
        .flat_map(|group| group.children.iter())
        .filter(|property| matches!(property.name.as_str(), "TargetFramework" | "TargetFrameworks"))
        .flat_map(|property| property.text.split(';'))
        .map(str::trim)
        .filter(|moniker| !moniker.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(CsProject {
        entries: package_items(&project, &["PackageReference"]),
        frameworks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entry;
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/nuget/src/App/App.csproj");

    #[test]
    fn package_references_and_target_frameworks_are_read() {
        let project = parse(SAMPLE, Path::new("src/App/App.csproj")).unwrap();
        let nuget = |name, version| Entry::registry(Ecosystem::Nuget, name, version);
        assert_eq!(
            project.entries,
            [
                nuget("Newtonsoft.Json", Some("13.0.3")),
                nuget("Serilog", None),
                nuget("Polly", Some("8.4.0")),
                nuget("xunit", Some("2.8.1")),
            ]
        );
        assert_eq!(project.frameworks, ["net8.0", "net9.0"]);
        let single = parse(
            "<Project><PropertyGroup><TargetFramework> net6.0 </TargetFramework></PropertyGroup></Project>",
            Path::new("a.csproj"),
        )
        .unwrap();
        assert_eq!(single.frameworks, ["net6.0"]);
        assert_eq!(single.entries.len(), 0);
    }

    #[test]
    fn malformed_xml_is_a_parse_error() {
        let error = parse("<Project>\n<a>\n</b>", Path::new("a.csproj")).err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(3), .. })),
            "{error:?}"
        );
    }
}

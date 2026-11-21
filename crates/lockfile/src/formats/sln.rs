//! `.sln` and `.slnx` solution files: the paths of their .NET projects, in solution order. The `.slnx` layout
//! (`<Project Path>` elements, possibly inside `<Folder>`) is UNVERIFIED.

use std::path::Path;

use super::xml::{self, Element};
use crate::error::LockfileError;

/// The project path as written, with `\\` turned into `/`, when it names a .NET project file.
fn project_path(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    [".csproj", ".fsproj", ".vbproj"]
        .iter()
        .any(|extension| path.ends_with(extension))
        .then_some(path)
}

/// The project paths of `Project("{type}") = "name", "path", "{guid}"` lines; solution folders are not projects.
pub(crate) fn parse_sln(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("Project("))
        .filter_map(|line| line.split('"').nth(5))
        .filter_map(project_path)
        .collect()
}

/// The `Path` of every `<Project>` element, at any `<Folder>` depth, in document order.
pub(crate) fn parse_slnx(text: &str, file: &Path) -> Result<Vec<String>, LockfileError> {
    fn walk(element: &Element, paths: &mut Vec<String>) {
        for child in &element.children {
            if child.name == "Project" {
                paths.extend(child.attribute("Path").and_then(project_path));
            }
            walk(child, paths);
        }
    }
    let solution = xml::parse(text, file)?;
    let mut paths = Vec::new();
    walk(&solution, &mut paths);
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn sln_project_paths_are_read_in_order() {
        let text = include_str!("../../../../fixtures/lockfiles/nuget/Demo.sln");
        assert_eq!(parse_sln(text), ["src/App/App.csproj", "src/Lib/Lib.fsproj"]);
    }

    #[test]
    fn slnx_project_paths_are_read_in_order() {
        let text = include_str!("../../../../fixtures/lockfiles/nuget/Demo.slnx");
        assert_eq!(
            parse_slnx(text, Path::new("Demo.slnx")).unwrap(),
            ["src/App/App.csproj", "src/Lib/Lib.fsproj", "tools/Tool.vbproj"]
        );
        let error = parse_slnx("<Solution>\n<Project>\n", Path::new("Demo.slnx")).err();
        assert!(matches!(error, Some(LockfileError::Parse { line: Some(3), .. })), "{error:?}");
    }
}

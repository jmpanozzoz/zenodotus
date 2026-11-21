//! Choosing project roots (design §8.1 and §8.4): the working-directory fallback and the one root whose
//! dependencies are used.

use std::fs;
use std::path::{Path, PathBuf};

/// The file names that mark a project root: `.git`, the files of research B2 and the version files of §7.7.
pub(crate) const MARKERS: &[&str] = &[
    ".git",
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "yarn.lock",
    "bun.lock",
    "uv.lock",
    "poetry.lock",
    "pdm.lock",
    "Pipfile.lock",
    "pyproject.toml",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "gradle.lockfile",
    "gradle/libs.versions.toml",
    "packages.lock.json",
    "Directory.Packages.props",
    "go.mod",
    "go.work",
    "go.sum",
    "Cargo.lock",
    "Cargo.toml",
    "composer.lock",
    "composer.json",
    "Gemfile",
    "Gemfile.lock",
    "pubspec.yaml",
    "pubspec.lock",
    "Package.resolved",
    "Package.swift",
    "rust-toolchain",
    "rust-toolchain.toml",
    ".python-version",
    ".nvmrc",
    ".node-version",
    "global.json",
    ".ruby-version",
];

/// Whether a file name is a marker by pattern: `*.csproj`, `*.sln`, `*.slnx` or `requirements*.txt`.
fn is_marker_pattern(name: &str) -> bool {
    ["csproj", "sln", "slnx"]
        .iter()
        .any(|extension| has_extension(name, extension))
        || is_requirements(name)
}

/// Whether the file name `name` ends in `.extension`, in any case.
pub(crate) fn has_extension(name: &str, extension: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|found| found.eq_ignore_ascii_case(extension))
}

/// Whether `name` is a `requirements*.txt` file.
pub(crate) fn is_requirements(name: &str) -> bool {
    name.starts_with("requirements") && has_extension(name, "txt")
}

/// Whether `dir` holds a project marker; nothing is read, and symlinks count as present.
#[must_use]
pub fn has_project_marker(dir: &Path) -> bool {
    MARKERS
        .iter()
        .any(|marker| fs::symlink_metadata(dir.join(marker)).is_ok())
        || fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_str().is_some_and(is_marker_pattern))
        })
}

/// The working directory of a stdio `serve` as a root (design §8.1, channel 3): only when it holds a project marker
/// and is not the home directory.
#[must_use]
pub fn working_directory_root(cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    (home != Some(cwd) && has_project_marker(cwd)).then(|| cwd.to_path_buf())
}

/// The root whose dependencies are used (design §8.4): roots that contain another root are dropped, so the deepest
/// of nested roots wins, then the first in channel order.
#[must_use]
pub fn select_root(roots: &[PathBuf]) -> Option<&Path> {
    roots
        .iter()
        .find(|root| !roots.iter().any(|other| other != *root && other.starts_with(root)))
        .map(PathBuf::as_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn the_deepest_root_wins_when_roots_nest_otherwise_the_first() {
        let roots = [PathBuf::from("/w"), PathBuf::from("/w/app"), PathBuf::from("/other")];
        assert_eq!(select_root(&roots), Some(Path::new("/w/app")));
        let nested = [
            PathBuf::from("/w/app/inner"),
            PathBuf::from("/w"),
            PathBuf::from("/w/app"),
        ];
        assert_eq!(select_root(&nested), Some(Path::new("/w/app/inner")));
        let siblings = [PathBuf::from("/b"), PathBuf::from("/a")];
        assert_eq!(select_root(&siblings), Some(Path::new("/b")));
        let prefix_only = [PathBuf::from("/w"), PathBuf::from("/wx")];
        assert_eq!(select_root(&prefix_only), Some(Path::new("/w")));
        assert_eq!(select_root(&[]), None);
    }

    #[test]
    fn a_working_directory_counts_only_with_a_marker_and_not_as_home() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("project");
        fs::create_dir(&project).unwrap();
        assert!(!has_project_marker(&project));
        assert_eq!(working_directory_root(&project, Some(home.path())), None);
        fs::write(project.join("go.mod"), "module x\n").unwrap();
        assert!(has_project_marker(&project));
        assert_eq!(
            working_directory_root(&project, Some(home.path())),
            Some(project.clone())
        );
        fs::create_dir(home.path().join(".git")).unwrap();
        assert!(has_project_marker(home.path()));
        assert_eq!(working_directory_root(home.path(), Some(home.path())), None);
        assert_eq!(
            working_directory_root(home.path(), None),
            Some(home.path().to_path_buf())
        );
        for marker in [
            "rust-toolchain.toml",
            ".nvmrc",
            "Package.swift",
            "App.csproj",
            "Demo.sln",
            "requirements-dev.txt",
        ] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(marker), "").unwrap();
            assert!(has_project_marker(dir.path()), "{marker}");
        }
    }
}

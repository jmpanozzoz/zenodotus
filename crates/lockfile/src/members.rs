//! Workspace members (design §8.2): the member lists of `package.json`, `pnpm-workspace.yaml`, `Cargo.toml`,
//! `go.work` and `settings.gradle(.kts)`, expanded one directory level inside the root.

use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::error::LockfileError;
use crate::formats::go_mod::directives;
use crate::formats::gradle_build::quoted;
use crate::formats::yaml;

/// The most members read in one root.
pub(crate) const MAX_MEMBERS: usize = 64;

/// The member directories found, relative to the root, and why some were not used.
#[derive(Debug, Default)]
pub(crate) struct Members {
    /// The member directories, in list order, each once.
    pub(crate) dirs: Vec<PathBuf>,
    /// The members refused.
    pub(crate) errors: Vec<LockfileError>,
}

#[derive(Deserialize)]
struct PnpmWorkspace {
    #[serde(default)]
    packages: Vec<String>,
}

/// The `packages` of `pnpm-workspace.yaml`.
pub(crate) fn pnpm_workspace(text: &str, file: &Path) -> Result<Vec<String>, LockfileError> {
    yaml::<PnpmWorkspace>(text, file).map(|workspace| workspace.packages)
}

/// The `use` directories of `go.work`.
pub(crate) fn go_work(text: &str) -> Vec<String> {
    directives(text)
        .into_iter()
        .filter(|directive| directive.verb == "use")
        .flat_map(|directive| directive.arguments)
        .map(str::to_owned)
        .collect()
}

/// The `include` projects of `settings.gradle(.kts)`, read as text: `:a:b` is the directory `a/b`.
pub(crate) fn settings_gradle(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with("include ") || line.starts_with("include("))
        .flat_map(quoted)
        .map(|project| project.trim_start_matches(':').replace(':', "/"))
        .collect()
}

/// Whether `text` holds a glob character.
fn is_glob(text: &str) -> bool {
    text.contains(['*', '?', '[', '{'])
}

/// `pattern` normalized lexically to a path inside the root; `None` when it is absolute or leaves the root.
pub(crate) fn normalize(pattern: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in pattern.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

/// Whether `relative` is a real directory under `root`, checking every component without following symlinks.
fn real_dir(root: &Path, relative: &Path) -> Result<bool, LockfileError> {
    let mut on_the_way = root.to_path_buf();
    let mut is_dir = true;
    for component in relative.components() {
        on_the_way.push(component);
        match fs::symlink_metadata(&on_the_way) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(source) => {
                return Err(LockfileError::Io {
                    path: relative.to_path_buf(),
                    source,
                });
            }
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(LockfileError::NotRegularFile {
                    path: relative.to_path_buf(),
                });
            }
            Ok(metadata) => is_dir = metadata.is_dir(),
        }
    }
    Ok(is_dir)
}

/// The directories of `parent` whose names match `glob`, in name order; symlinks among them are refused.
fn list(root: &Path, parent: &Path, glob: &globset::GlobMatcher, members: &mut Members) {
    match real_dir(root, parent) {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => return members.errors.push(error),
    }
    let mut matched: Vec<(String, fs::FileType)> = fs::read_dir(root.join(parent))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.file_type().ok()?)))
        .filter(|(name, _)| glob.is_match(name))
        .collect();
    matched.sort_by(|(a, _), (b, _)| a.cmp(b));
    for (name, file_type) in matched {
        let path = parent.join(name);
        if file_type.is_symlink() {
            members.errors.push(LockfileError::NotRegularFile { path });
        } else if file_type.is_dir() {
            members.dirs.push(path);
        }
    }
}

/// The member directories `patterns` name under `root`; `file` is the list's file, for errors. `!pattern` excludes.
/// A glob expands its last segment only, one directory level; `**` and globs in other segments are refused. At most
/// [`MAX_MEMBERS`] members are kept.
pub(crate) fn expand(root: &Path, file: &Path, patterns: &[String]) -> Members {
    let mut members = Members::default();
    let (exclusions, inclusions): (Vec<&String>, Vec<&String>) =
        patterns.iter().partition(|pattern| pattern.starts_with('!'));
    for pattern in inclusions {
        let Some(path) = normalize(Path::new(pattern)) else {
            members.errors.push(LockfileError::PathEscapesRoot {
                file: file.to_path_buf(),
                member: pattern.clone(),
            });
            continue;
        };
        let last = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        let parent = path.parent().unwrap_or(Path::new(""));
        let inner_glob = parent.to_str().is_none_or(is_glob);
        let glob = is_glob(last).then(|| globset::Glob::new(last));
        if pattern.contains("**") || inner_glob || matches!(glob, Some(Err(_))) {
            members.errors.push(LockfileError::GlobRefused {
                file: file.to_path_buf(),
                pattern: pattern.clone(),
            });
        } else if let Some(Ok(glob)) = glob {
            list(root, parent, &glob.compile_matcher(), &mut members);
        } else {
            match real_dir(root, &path) {
                Ok(true) => members.dirs.push(path),
                Ok(false) => {}
                Err(error) => members.errors.push(error),
            }
        }
    }
    let excluded: Vec<globset::GlobMatcher> = exclusions
        .iter()
        .filter_map(|pattern| normalize(Path::new(&pattern[1..])))
        .filter_map(|path| {
            globset::GlobBuilder::new(path.to_str()?)
                .literal_separator(true)
                .build()
                .ok()
        })
        .map(|glob| glob.compile_matcher())
        .collect();
    let mut seen = std::collections::HashSet::new();
    members
        .dirs
        .retain(|dir| !excluded.iter().any(|glob| glob.is_match(dir)) && seen.insert(dir.clone()));
    if members.dirs.len() > MAX_MEMBERS {
        members.dirs.truncate(MAX_MEMBERS);
        members.errors.push(LockfileError::TooManyMembers {
            file: file.to_path_buf(),
            limit: MAX_MEMBERS,
        });
    }
    members
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cause;
    use std::fs;

    fn tree(dirs: &[&str]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for dir in dirs {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        root
    }

    fn patterns(patterns: &[&str]) -> Vec<String> {
        patterns.iter().map(|pattern| (*pattern).to_owned()).collect()
    }

    fn messages(members: &Members) -> Vec<(Cause, String)> {
        members
            .errors
            .iter()
            .map(|error| (error.cause(), error.to_string()))
            .collect()
    }

    fn dirs(members: &Members) -> Vec<&str> {
        members.dirs.iter().map(|dir| dir.to_str().unwrap()).collect()
    }

    #[test]
    fn member_lists_are_read_from_each_workspace_file() {
        let pnpm = pnpm_workspace(
            "packages:\n  - 'packages/*'\n  - apps/web\n",
            Path::new("pnpm-workspace.yaml"),
        );
        assert_eq!(pnpm.unwrap(), ["packages/*", "apps/web"]);
        let error = pnpm_workspace("packages: [\n", Path::new("pnpm-workspace.yaml")).err();
        assert!(matches!(error, Some(LockfileError::Parse { .. })), "{error:?}");
        assert_eq!(
            go_work("go 1.22\n\nuse ./cmd // the tool\nuse (\n\t./lib\n\t\"./quoted\"\n)\n"),
            ["./cmd", "./lib", "./quoted"]
        );
        assert_eq!(
            settings_gradle(
                "rootProject.name = 'demo'\ninclude ':app', ':lib:core'\ninclude(\"feature\")\n// include ':old'\n"
            ),
            ["app", "lib/core", "feature"]
        );
    }

    #[test]
    fn a_glob_in_the_last_segment_expands_one_level_in_name_order() {
        let root = tree(&[
            "packages/b",
            "packages/a",
            "packages/a/nested",
            "packages/skip",
            "tools/cli",
            "apps/web",
        ]);
        fs::write(root.path().join("packages/file.txt"), "").unwrap();
        let members = expand(
            root.path(),
            Path::new("package.json"),
            &patterns(&[
                "packages/*",
                "!packages/skip",
                "./tools/cli",
                "missing/*",
                "missing",
                "apps/web/../web",
            ]),
        );
        assert_eq!(dirs(&members), ["packages/a", "packages/b", "tools/cli", "apps/web"]);
        assert!(members.errors.is_empty(), "{:?}", members.errors);
        let repeated = expand(
            root.path(),
            Path::new("package.json"),
            &patterns(&["tools/cli", "tools/*"]),
        );
        assert_eq!(dirs(&repeated), ["tools/cli"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_members_and_symlinked_glob_parents_are_refused() {
        let root = tree(&["real/a"]);
        std::os::unix::fs::symlink(root.path().join("real"), root.path().join("linked")).unwrap();
        std::os::unix::fs::symlink(root.path().join("real/a"), root.path().join("real/b")).unwrap();
        let members = expand(
            root.path(),
            Path::new("package.json"),
            &patterns(&["linked", "linked/*", "real/*"]),
        );
        assert_eq!(dirs(&members), ["real/a"]);
        assert_eq!(
            messages(&members),
            [
                (Cause::NotRegularFile, "linked: not a regular file".to_owned()),
                (Cause::NotRegularFile, "linked: not a regular file".to_owned()),
                (Cause::NotRegularFile, "real/b: not a regular file".to_owned()),
            ]
        );
    }

    #[test]
    fn a_member_under_a_regular_file_is_an_io_error() {
        let root = tree(&[]);
        fs::write(root.path().join("go.mod"), "module x\n").unwrap();
        let members = expand(
            root.path(),
            Path::new("go.work"),
            &patterns(&["go.mod/inner", "go.mod"]),
        );
        assert_eq!(dirs(&members), [] as [&str; 0]);
        assert_eq!(
            members.errors.iter().map(LockfileError::cause).collect::<Vec<_>>(),
            [Cause::Io]
        );
    }

    #[test]
    fn hostile_members_outside_the_root_are_refused() {
        let root = tree(&["inside"]);
        let members = expand(
            root.path(),
            Path::new("Cargo.toml"),
            &patterns(&["../../etc", "/etc", "inside/../../x", "inside"]),
        );
        assert_eq!(dirs(&members), ["inside"]);
        let escaped = |member: &str| {
            (
                Cause::PathEscapesRoot,
                format!("Cargo.toml: member {member:?} is outside the root"),
            )
        };
        assert_eq!(
            messages(&members),
            [escaped("../../etc"), escaped("/etc"), escaped("inside/../../x")]
        );
    }

    #[test]
    fn hostile_recursive_or_inner_globs_are_refused() {
        let root = tree(&["packages/a", "packages/a/x", "deep/x"]);
        let members = expand(
            root.path(),
            Path::new("package.json"),
            &patterns(&["**", "packages/**", "*/x", "pack*/a", "bad[", "packages/*"]),
        );
        assert_eq!(dirs(&members), ["packages/a"]);
        let refused = |pattern: &str| {
            let message = format!("package.json: member glob {pattern:?} is refused; only one directory level expands");
            (Cause::GlobRefused, message)
        };
        assert_eq!(
            messages(&members),
            ["**", "packages/**", "*/x", "pack*/a", "bad["].map(refused)
        );
    }

    #[test]
    fn hostile_glob_expansion_stops_at_the_member_limit() {
        let names: Vec<String> = (0..MAX_MEMBERS + 6)
            .map(|index| format!("packages/p{index:03}"))
            .collect();
        let root = tree(&names.iter().map(String::as_str).collect::<Vec<_>>());
        let members = expand(
            root.path(),
            Path::new("pnpm-workspace.yaml"),
            &patterns(&["packages/*", "late"]),
        );
        assert_eq!(members.dirs.len(), MAX_MEMBERS);
        assert_eq!(members.dirs.last().unwrap(), Path::new("packages/p063"));
        assert!(
            matches!(
                members.errors[..],
                [LockfileError::TooManyMembers { ref file, limit: MAX_MEMBERS }] if file == Path::new("pnpm-workspace.yaml")
            ),
            "{:?}",
            members.errors
        );
    }
}

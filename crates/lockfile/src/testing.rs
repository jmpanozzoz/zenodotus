//! Test helpers: temporary project roots, from files or from a fixture directory, and the `direct` flag of each
//! dependency read from them.

use std::fs;
use std::path::Path;

use crate::{Options, read_project};

/// A temporary root holding `files`, each `(relative path, text)`.
pub(crate) fn root_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    root
}

/// A temporary root holding a copy of `fixtures/lockfiles/<dir>`. A `.sample` suffix is cut from each file name:
/// cargo-machete would take a fixture named `Cargo.toml` for a package.
pub(crate) fn fixture_root(dir: &str) -> tempfile::TempDir {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/lockfiles")
        .join(dir);
    let root = tempfile::tempdir().unwrap();
    copy_dir(&source, root.path());
    root
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let target = to.join(name.strip_suffix(".sample").unwrap_or(&name));
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir(&target).unwrap();
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Each dependency of the project at `root`, as `<canonical id>[@<version>]`, with its `direct` flag.
pub(crate) fn direct_flags(root: &Path) -> Vec<(String, bool)> {
    read_project(root, &Options::default())
        .dependencies
        .into_iter()
        .map(|dependency| {
            let id = dependency.canonical_id();
            let id = dependency
                .version
                .map_or(id.clone(), |version| format!("{id}@{version}"));
            (id, dependency.direct)
        })
        .collect()
}

/// Asserts that the project at `root` has dependencies and that every one is direct.
pub(crate) fn assert_all_direct(root: &Path) {
    let flags = direct_flags(root);
    assert!(!flags.is_empty(), "no dependency read");
    assert!(flags.iter().all(|(_, direct)| *direct), "{flags:?}");
}

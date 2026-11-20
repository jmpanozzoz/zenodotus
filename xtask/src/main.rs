//! Repository tasks for zenodotus: `check-repo` and `check-errors`.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "xtask is a developer CLI that reports to the terminal"
)]

mod errors;
mod repo;

use std::path::Path;
use std::process::{Command, ExitCode};

use repo::SourceFile;

fn main() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap_or(Path::new("."));
    run(&std::env::args().skip(1).collect::<Vec<_>>(), root)
}

/// Runs one task by name and reports its violations.
fn run(args: &[String], root: &Path) -> ExitCode {
    let check: fn(&[SourceFile]) -> Vec<String> = match args.first().map(String::as_str) {
        Some("check-repo") => repo::check,
        Some("check-errors") => errors::check,
        _ => {
            eprintln!("usage: cargo xtask <check-repo | check-errors>");
            return ExitCode::from(2);
        }
    };
    let files = match load_files(root) {
        Ok(files) => files,
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::FAILURE;
        }
    };
    let violations = check(&files);
    for violation in &violations {
        eprintln!("{violation}");
    }
    if violations.is_empty() {
        println!("{}: ok ({} files)", args[0], files.len());
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Lists tracked and untracked-but-not-ignored files under `root` and reads them.
fn load_files(root: &Path) -> Result<Vec<SourceFile>, String> {
    let output = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut files = Vec::new();
    for path in listing.split('\0').filter(|p| !p.is_empty()) {
        let full = root.join(path);
        if !full.is_file() {
            continue;
        }
        let bytes = std::fs::read(&full).map_err(|e| format!("cannot read {path}: {e}"))?;
        let text = String::from_utf8(bytes).unwrap_or_default();
        files.push(SourceFile {
            path: path.to_owned(),
            text,
        });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q"]);
        for (path, text) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }
        dir
    }

    #[test]
    fn check_repo_passes_on_a_clean_tree_and_ignores_gitignored_files() {
        let dir = repo_with(&[
            ("README.md", "# x"),
            (".gitignore", "target/\n"),
            ("target/NOTES.md", ""),
        ]);
        assert_eq!(run(&["check-repo".to_owned()], dir.path()), ExitCode::SUCCESS);
    }

    #[test]
    fn check_repo_fails_on_a_violation() {
        let dir = repo_with(&[("NOTES.md", "")]);
        assert_eq!(run(&["check-repo".to_owned()], dir.path()), ExitCode::FAILURE);
    }

    #[test]
    fn check_errors_fails_on_an_untested_variant_and_passes_otherwise() {
        let dir = repo_with(&[("crates/a/src/e.rs", "pub enum AError {\n    Gone,\n}\n")]);
        assert_eq!(run(&["check-errors".to_owned()], dir.path()), ExitCode::FAILURE);
        let dir = repo_with(&[(
            "crates/a/src/e.rs",
            "pub enum AError {\n    Gone,\n}\n#[cfg(test)]\nGone",
        )]);
        assert_eq!(run(&["check-errors".to_owned()], dir.path()), ExitCode::SUCCESS);
    }

    #[test]
    fn binary_files_are_read_as_empty_text() {
        let dir = repo_with(&[("README.md", "# x"), ("logo.png", "")]);
        std::fs::write(dir.path().join("logo.png"), [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(
            load_files(dir.path())
                .unwrap()
                .iter()
                .find(|f| f.path == "logo.png")
                .unwrap()
                .text,
            ""
        );
    }

    #[test]
    fn unknown_or_missing_task_is_a_usage_error() {
        let dir = repo_with(&[]);
        assert_eq!(run(&["nope".to_owned()], dir.path()), ExitCode::from(2));
        assert_eq!(run(&[], dir.path()), ExitCode::from(2));
    }

    #[test]
    fn git_that_cannot_start_is_reported() {
        let missing = std::env::temp_dir().join("zenodotus-xtask-missing-dir");
        let error = load_files(&missing).unwrap_err();
        assert!(error.starts_with("cannot run git: "), "{error}");
    }

    #[test]
    fn staged_files_missing_from_disk_are_skipped() {
        let dir = repo_with(&[("README.md", "# x"), ("gone.rs", "")]);
        let git = Command::new("git")
            .args(["add", "gone.rs"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(git.success());
        std::fs::remove_file(dir.path().join("gone.rs")).unwrap();
        let paths: Vec<String> = load_files(dir.path()).unwrap().into_iter().map(|f| f.path).collect();
        assert_eq!(paths, ["README.md"]);
    }

    #[test]
    fn an_unreadable_file_fails_loading() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = repo_with(&[("secret.rs", "")]);
        let path = dir.path().join("secret.rs");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let error = load_files(dir.path()).unwrap_err();
        assert!(error.starts_with("cannot read secret.rs: "), "{error}");
    }

    #[test]
    fn outside_a_git_repository_loading_fails() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_files(dir.path()).is_err());
        assert_eq!(run(&["check-repo".to_owned()], dir.path()), ExitCode::FAILURE);
    }
}

//! Definition of done 3: hostile project files surface as diagnostics, never as errors or hangs, and each keeps its cause.

use std::fs;
use std::path::Path;

use zenodotus_lockfile::{Cause, DiagnosticKind, Options, read_project};

fn causes(root: &Path) -> Vec<(String, DiagnosticKind, Option<Cause>)> {
    read_project(root, &Options::default())
        .diagnostics
        .into_iter()
        .map(|diagnostic| (diagnostic.file.display().to_string(), diagnostic.kind, diagnostic.cause))
        .collect()
}

#[cfg(unix)]
#[test]
fn a_symlink_to_dev_zero_and_a_fifo_are_not_regular_files() {
    let root = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/dev/zero", root.path().join("package-lock.json")).unwrap();
    let fifo = std::process::Command::new("mkfifo")
        .arg(root.path().join("Cargo.lock"))
        .status()
        .unwrap();
    assert!(fifo.success());
    let (done, wait) = std::sync::mpsc::channel();
    let path = root.path().to_path_buf();
    std::thread::spawn(move || done.send(causes(&path)).unwrap());
    let found = wait
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("reading a FIFO must not block");
    let skipped = DiagnosticKind::SkippedFile;
    assert_eq!(
        found,
        [
            ("package-lock.json".to_owned(), skipped, Some(Cause::NotRegularFile)),
            ("Cargo.lock".to_owned(), skipped, Some(Cause::NotRegularFile)),
        ]
    );
}

#[test]
fn a_33_mib_package_lock_is_too_large_and_never_read() {
    let root = tempfile::tempdir().unwrap();
    let file = fs::File::create(root.path().join("package-lock.json")).unwrap();
    file.set_len(33 << 20).unwrap();
    let project = read_project(root.path(), &Options::default());
    assert_eq!(project.bytes_read, 0, "the size is checked before reading");
    assert_eq!(project.diagnostics.len(), 1);
    assert_eq!(project.diagnostics[0].cause, Some(Cause::TooLarge));
    assert_eq!(project.diagnostics[0].kind, DiagnosticKind::SkippedFile);
}

#[test]
fn members_outside_the_root_and_recursive_globs_are_refused() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("package.json"),
        r#"{"workspaces": ["../../etc", "/etc", "packages/**"]}"#,
    )
    .unwrap();
    let skipped = DiagnosticKind::SkippedFile;
    assert_eq!(
        causes(root.path()),
        [
            ("package.json".to_owned(), skipped, Some(Cause::PathEscapesRoot)),
            ("package.json".to_owned(), skipped, Some(Cause::PathEscapesRoot)),
            ("package.json".to_owned(), skipped, Some(Cause::GlobRefused)),
        ]
    );
}

#[test]
fn a_yaml_anchor_bomb_exceeds_the_budget() {
    let root = tempfile::tempdir().unwrap();
    let bomb = include_str!("../../../fixtures/lockfiles/pnpm/anchor-bomb.yaml");
    fs::write(root.path().join("pnpm-lock.yaml"), bomb).unwrap();
    assert_eq!(
        causes(root.path()),
        [(
            "pnpm-lock.yaml".to_owned(),
            DiagnosticKind::ParseError,
            Some(Cause::YamlBudget)
        )]
    );
}

#[test]
fn malformed_toml_json_and_yaml_are_parse_errors_with_file_and_line() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("Cargo.lock"), "version = 4\n[[package]\n").unwrap();
    fs::write(root.path().join("composer.lock"), "{\n\"packages\": [\n}").unwrap();
    fs::write(
        root.path().join("pubspec.lock"),
        "packages:\n  a:\n    source: [1]\n    description: a\n",
    )
    .unwrap();
    let project = read_project(root.path(), &Options::default());
    let found: Vec<(String, Option<u64>, Option<Cause>)> = project
        .diagnostics
        .iter()
        .map(|diagnostic| (diagnostic.file.display().to_string(), diagnostic.line, diagnostic.cause))
        .collect();
    assert_eq!(
        found,
        [
            ("Cargo.lock".to_owned(), Some(2), Some(Cause::Parse)),
            ("composer.lock".to_owned(), Some(3), Some(Cause::Parse)),
            ("pubspec.lock".to_owned(), Some(3), Some(Cause::Parse)),
        ]
    );
}

#[test]
fn a_root_reads_at_most_64_files() {
    let root = tempfile::tempdir().unwrap();
    let members: Vec<String> = (0..70).map(|index| format!("\"m{index:02}\"")).collect();
    fs::write(
        root.path().join("package.json"),
        format!("{{\"workspaces\": [{}]}}", members.join(",")),
    )
    .unwrap();
    for index in 0..70 {
        let member = root.path().join(format!("m{index:02}"));
        fs::create_dir(&member).unwrap();
        fs::write(member.join("package.json"), "{}").unwrap();
    }
    let project = read_project(root.path(), &Options::default());
    assert_eq!(project.files.len(), 64);
    assert!(
        project
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.cause == Some(Cause::FileLimit))
    );
    assert!(
        project
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.cause == Some(Cause::TooManyMembers))
    );
}

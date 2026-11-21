//! The `zenodotus-eval` binary end to end: its output and exit status.

use std::process::Command;

const BINARY: &str = env!("CARGO_BIN_EXE_zenodotus-eval");

#[test]
fn a_run_prints_its_report_and_succeeds() {
    let root = tempfile::TempDir::new().unwrap();
    let eval = root.path().join("eval");
    std::fs::create_dir_all(eval.join("questions")).unwrap();
    std::fs::write(
        eval.join("corpus.toml"),
        "[settings]\ndata_dir = \"data\"\n\n[[library]]\nid = \"tokio\"\n[[library.version]]\nversion = \"latest\"\n\
         source = { kind = \"github\", repo = \"tokio-rs/website\", reference = \"abc\", paths = [\"content\"] }\n",
    )
    .unwrap();
    std::fs::write(eval.join("context7.toml"), "[libraries]\n").unwrap();
    std::fs::write(eval.join("questions/dev.jsonl"), "").unwrap();
    let output = Command::new(BINARY)
        .arg("--root")
        .arg(root.path())
        .args(["run", "--out"])
        .arg(root.path().join("results"))
        .args(["--zenodotus", "/nonexistent/zenodotus"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.starts_with("zenodotus-eval: dev set, 0 questions\n"), "{stdout}");
    assert!(
        stdout.contains("| zenodotus | not run: cannot start /nonexistent/zenodotus"),
        "{stdout}"
    );
}

#[test]
fn an_error_is_printed_and_fails() {
    let root = tempfile::TempDir::new().unwrap();
    let output = Command::new(BINARY)
        .arg("--root")
        .arg(root.path())
        .arg("validate")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("error: "), "{stderr}");
}

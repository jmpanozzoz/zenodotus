//! The `xtask` binary reports usage errors with exit code 2.

use std::process::Command;

#[test]
fn an_unknown_task_exits_with_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_xtask")).arg("nope").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "usage: cargo xtask <check-repo | check-errors | perf [--reuse]>\n"
    );
}

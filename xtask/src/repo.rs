//! `check-repo`: repository hygiene rules that CI enforces.

/// One tracked file: its path relative to the repository root, with `/` separators, and its text.
#[derive(Debug)]
pub struct SourceFile {
    /// Path relative to the repository root.
    pub path: String,
    /// File contents; binary files are given as empty text.
    pub text: String,
}

/// Markdown files allowed at the repository root.
const ROOT_MARKDOWN: [&str; 5] = [
    "README.md",
    "CONTRIBUTING.md",
    "SECURITY.md",
    "ARCHITECTURE.md",
    "CHANGELOG.md",
];
/// Files where printing to stdout or stderr is allowed.
const PRINT_ALLOWED: [&str; 3] = ["crates/cli/src/output.rs", "crates/eval/src/report.rs", "xtask/"];

/// Third-party text committed as data, such as Stack Overflow posts, where task markers are quotes rather than ours.
const THIRD_PARTY_DATA: [&str; 2] = ["eval/v2/raw/", "eval/v2/questions/"];

/// Returns one human-readable violation per broken rule, in path order.
#[must_use]
pub fn check(files: &[SourceFile]) -> Vec<String> {
    let mut sorted: Vec<&SourceFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    sorted.into_iter().flat_map(violations).collect()
}

/// All violations of one file, in rule order.
fn violations(file: &SourceFile) -> Vec<String> {
    let path = file.path.as_str();
    let mut found = Vec::new();
    if has_extension(path, "md") && !path.starts_with("fixtures/") && !ROOT_MARKDOWN.contains(&path) {
        found.push("Markdown file outside the allowlist".to_owned());
    }
    for marker in [["TO", "DO"].concat(), ["FIX", "ME"].concat()] {
        if file.text.contains(&marker) && !THIRD_PARTY_DATA.iter().any(|data| path.starts_with(data)) {
            found.push(format!("contains {marker}"));
        }
    }
    if has_extension(path, "rs") {
        for banned in [
            ["to", "do!"].concat(),
            ["unimpl", "emented!"].concat(),
            ["db", "g!"].concat(),
        ] {
            if file.text.contains(&banned) {
                found.push(format!("contains {banned}"));
            }
        }
        let prints = ["print!", "println!", "eprint!", "eprintln!"]
            .iter()
            .any(|m| file.text.contains(m));
        if prints && !PRINT_ALLOWED.iter().any(|allowed| path.starts_with(allowed)) {
            found.push("prints to stdout or stderr".to_owned());
        }
        if is_crate_root(path) {
            for attribute in ["#![forbid(unsafe_code)]", "#![deny(missing_docs)]"] {
                if !file.text.contains(attribute) {
                    found.push(format!("missing {attribute}"));
                }
            }
        }
    }
    found.into_iter().map(|v| format!("{path}: {v}")).collect()
}

/// Whether `path` has the extension `ext`, ignoring ASCII case.
pub fn has_extension(path: &str, ext: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// Whether `path` is the root module of a workspace crate.
fn is_crate_root(path: &str) -> bool {
    let crate_relative = path
        .strip_prefix("crates/")
        .and_then(|rest| rest.split_once('/'))
        .map(|(_, rest)| rest);
    matches!(crate_relative, Some("src/lib.rs" | "src/main.rs")) || path == "xtask/src/main.rs"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, text: &str) -> SourceFile {
        SourceFile {
            path: path.to_owned(),
            text: text.to_owned(),
        }
    }

    fn crate_root(path: &str) -> SourceFile {
        file(path, "//! Docs.\n#![forbid(unsafe_code)]\n#![deny(missing_docs)]\n")
    }

    /// Builds forbidden words at run time so this file does not trip its own rules.
    fn word(parts: &[&str]) -> String {
        parts.concat()
    }

    #[test]
    fn a_clean_repository_passes() {
        let files = [
            file("README.md", "# zenodotus"),
            file("fixtures/markdown/basic.md", "# Basic"),
            crate_root("crates/core/src/lib.rs"),
            crate_root("xtask/src/main.rs"),
            file("crates/cli/src/output.rs", "fn f() { println!(\"ok\"); }"),
        ];
        assert_eq!(check(&files), Vec::<String>::new());
    }

    #[test]
    fn stray_markdown_is_rejected() {
        let files = [
            file("NOTES.MD", ""),
            file("docs/guide.md", ""),
            file("crates/core/README.md", ""),
        ];
        assert_eq!(
            check(&files),
            [
                "NOTES.MD: Markdown file outside the allowlist",
                "crates/core/README.md: Markdown file outside the allowlist",
                "docs/guide.md: Markdown file outside the allowlist",
            ]
        );
    }

    #[test]
    fn task_markers_are_rejected_in_any_text_file() {
        let files = [
            file("src/a.rs", &format!("// {} later", word(&["TO", "DO"]))),
            file("ci.yml", &format!("# {}", word(&["FIX", "ME"]))),
        ];
        assert_eq!(
            check(&files),
            [
                format!("ci.yml: contains {}", word(&["FIX", "ME"])),
                format!("src/a.rs: contains {}", word(&["TO", "DO"]))
            ]
        );
    }

    #[test]
    fn task_markers_in_third_party_data_are_allowed() {
        let marker = word(&["TO", "DO"]);
        let files = [
            file("eval/v2/raw/npm_react.json", &marker),
            file("eval/v2/questions/dev.jsonl", &marker),
            file("eval/v2/libraries.toml", &marker),
        ];
        assert_eq!(check(&files), [format!("eval/v2/libraries.toml: contains {marker}")]);
    }

    #[test]
    fn unfinished_and_debug_macros_are_rejected_in_rust() {
        let files = [file(
            "crates/core/src/x.rs",
            &format!(
                "{}(); {}(); {}(x);",
                word(&["to", "do!"]),
                word(&["unimpl", "emented!"]),
                word(&["db", "g!"])
            ),
        )];
        assert_eq!(
            check(&files),
            [
                format!("crates/core/src/x.rs: contains {}", word(&["to", "do!"])),
                format!("crates/core/src/x.rs: contains {}", word(&["unimpl", "emented!"])),
                format!("crates/core/src/x.rs: contains {}", word(&["db", "g!"])),
            ]
        );
    }

    #[test]
    fn printing_is_rejected_outside_the_allowed_files() {
        let files = [
            file(
                "crates/search/src/a.rs",
                &format!("{}(\"x\");", word(&["print", "ln!"])),
            ),
            file("crates/mcp/src/b.rs", &format!("{}(\"x\");", word(&["eprint", "ln!"]))),
            file("xtask/src/perf.rs", &format!("{}(\"x\");", word(&["print", "ln!"]))),
            file(
                "crates/eval/src/report.rs",
                &format!("{}(\"x\");", word(&["print", "ln!"])),
            ),
            file("notes.txt", &word(&["print", "ln!"])),
        ];
        assert_eq!(
            check(&files),
            [
                "crates/mcp/src/b.rs: prints to stdout or stderr",
                "crates/search/src/a.rs: prints to stdout or stderr"
            ]
        );
    }

    #[test]
    fn crate_roots_need_the_safety_attributes() {
        let files = [
            file("crates/core/src/lib.rs", "#![forbid(unsafe_code)]\n"),
            file("crates/cli/src/main.rs", "#![deny(missing_docs)]\n"),
            file("crates/cli/src/args.rs", ""),
        ];
        assert_eq!(
            check(&files),
            [
                "crates/cli/src/main.rs: missing #![forbid(unsafe_code)]",
                "crates/core/src/lib.rs: missing #![deny(missing_docs)]",
            ]
        );
    }
}

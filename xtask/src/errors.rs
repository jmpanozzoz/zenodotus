//! `check-errors`: every variant of every error enum must be named by at least one test.

use crate::repo::SourceFile;

/// One error enum and its variants, as found in library source.
#[derive(Debug, PartialEq, Eq)]
pub struct ErrorEnum {
    /// The file that declares it.
    pub path: String,
    /// The enum name, ending in `Error`.
    pub name: String,
    /// Variant names in declaration order.
    pub variants: Vec<String>,
}

/// Finds every `pub enum …Error` in `text` and lists its variants.
#[must_use]
pub fn find_enums(path: &str, text: &str) -> Vec<ErrorEnum> {
    let mut enums = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(name) = line
            .trim()
            .strip_prefix("pub enum ")
            .and_then(|rest| rest.split_whitespace().next())
        else {
            continue;
        };
        if !name.ends_with("Error") {
            continue;
        }
        let mut variants = Vec::new();
        let mut depth = 0usize;
        for body_line in lines.by_ref() {
            let trimmed = body_line.trim();
            if depth == 0 && !trimmed.starts_with('#') && !trimmed.starts_with("//") {
                let ident: String = trimmed
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if ident.starts_with(|c: char| c.is_ascii_uppercase()) {
                    variants.push(ident);
                }
            }
            depth = (depth + body_line.matches('{').count()).saturating_sub(body_line.matches('}').count());
            if trimmed == "}" && depth == 0 {
                break;
            }
        }
        enums.push(ErrorEnum {
            path: path.to_owned(),
            name: name.to_owned(),
            variants,
        });
    }
    enums
}

/// The part of a file that is test code: all of it for files under a `tests/` directory, otherwise everything after
/// the first `#[cfg(test)]`.
#[must_use]
pub fn test_text<'a>(path: &str, text: &'a str) -> &'a str {
    if path.contains("/tests/") {
        return text;
    }
    text.find("#[cfg(test)]").map_or("", |start| &text[start..])
}

/// Returns `Enum::Variant` for every variant that no test names, in a stable order.
#[must_use]
pub fn check(files: &[SourceFile]) -> Vec<String> {
    let rust: Vec<&SourceFile> = files
        .iter()
        .filter(|f| crate::repo::has_extension(&f.path, "rs"))
        .collect();
    let tests: Vec<&str> = rust.iter().map(|f| test_text(&f.path, &f.text)).collect();
    let mut missing = Vec::new();
    for file in &rust {
        let library_part = file
            .text
            .find("#[cfg(test)]")
            .map_or(file.text.as_str(), |end| &file.text[..end]);
        for error in find_enums(&file.path, library_part) {
            for variant in &error.variants {
                if !tests.iter().any(|text| contains_word(text, variant)) {
                    missing.push(format!("{}::{variant} ({})", error.name, error.path));
                }
            }
        }
    }
    missing.sort();
    missing
}

/// Whether `word` occurs in `text` with no identifier character directly before or after it.
fn contains_word(text: &str, word: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(word).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + word.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENUM_SOURCE: &str = "\
/// Docs.
#[derive(Debug)]
pub enum FetchError {
    /// A.
    Scheme,
    /// B.
    #[error(\"too large: {0}\")]
    TooLarge(u64),
    /// C.
    Status {
        /// Code.
        code: u16,
    },
}

pub enum NotAnErrorKind { Foo }

pub(crate) enum InnerError { Hidden }
";

    fn file(path: &str, text: &str) -> SourceFile {
        SourceFile {
            path: path.to_owned(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn finds_public_error_enums_and_their_variants() {
        assert_eq!(
            find_enums("crates/ingest/src/fetch.rs", ENUM_SOURCE),
            [ErrorEnum {
                path: "crates/ingest/src/fetch.rs".to_owned(),
                name: "FetchError".to_owned(),
                variants: vec!["Scheme".to_owned(), "TooLarge".to_owned(), "Status".to_owned()],
            }]
        );
    }

    #[test]
    fn test_text_is_the_cfg_test_tail_or_the_whole_integration_test() {
        assert_eq!(
            test_text("crates/a/src/x.rs", "code\n#[cfg(test)]\nmod tests {}"),
            "#[cfg(test)]\nmod tests {}"
        );
        assert_eq!(test_text("crates/a/src/x.rs", "code only"), "");
        assert_eq!(test_text("crates/a/tests/e2e.rs", "whole"), "whole");
    }

    #[test]
    fn reports_variants_no_test_mentions() {
        let files = [
            file(
                "crates/ingest/src/fetch.rs",
                &format!("{ENUM_SOURCE}\n#[cfg(test)]\nmod t {{ FetchError::Scheme }}"),
            ),
            file("crates/ingest/tests/caps.rs", "matches!(e, FetchError::TooLarge(_))"),
            file("crates/ingest/src/other.rs", "fn f() -> Status {}"),
        ];
        assert_eq!(check(&files), ["FetchError::Status (crates/ingest/src/fetch.rs)"]);
    }

    #[test]
    fn a_variant_name_inside_a_longer_word_does_not_count() {
        let files = [file(
            "crates/ingest/src/fetch.rs",
            &format!("{ENUM_SOURCE}\n#[cfg(test)]\nmod t {{ Schemes; TooLargeX; StatusCode; }}"),
        )];
        assert_eq!(check(&files).len(), 3);
    }

    #[test]
    fn non_rust_files_are_ignored() {
        assert_eq!(check(&[file("README.md", ENUM_SOURCE)]), Vec::<String>::new());
    }
}

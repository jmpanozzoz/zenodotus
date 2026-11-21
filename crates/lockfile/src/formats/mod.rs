//! One parser per file name of research B2, and the decoding helpers they share. Every parser turns text into
//! [`Entry`] values and never runs anything.

use std::path::Path;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::Source;
use crate::error::LockfileError;

pub(crate) mod bun_lock;
pub(crate) mod cargo_lock;
pub(crate) mod composer_lock;
pub(crate) mod csproj;
pub(crate) mod directory_packages;
pub(crate) mod gemfile_lock;
pub(crate) mod go_mod;
pub(crate) mod gradle_build;
pub(crate) mod gradle_catalog;
pub(crate) mod gradle_lockfile;
pub(crate) mod manifests;
pub(crate) mod npm_lock;
pub(crate) mod nuget_lock;
pub(crate) mod package_resolved;
pub(crate) mod pdm_lock;
pub(crate) mod pipfile_lock;
pub(crate) mod pnpm_lock;
pub(crate) mod poetry_lock;
pub(crate) mod pom;
pub(crate) mod pubspec_lock;
pub(crate) mod requirements;
pub(crate) mod sln;
pub(crate) mod uv_lock;
pub(crate) mod xml;
pub(crate) mod yarn_lock;

/// A parse error in `file` at `line`.
pub(crate) fn parse_error(file: &Path, line: Option<u64>, message: impl Into<String>) -> LockfileError {
    LockfileError::Parse {
        file: file.to_path_buf(),
        line,
        message: message.into(),
    }
}

/// A git source written `<url>#<commit>`.
pub(crate) fn git_fragment(url: &str) -> Source {
    let (url, commit) = url.split_once('#').unwrap_or((url, ""));
    Source::Git {
        url: url.to_owned(),
        commit: commit.to_owned(),
    }
}

/// The 1-based line of byte `offset` in `text`.
pub(crate) fn line_at(text: &str, offset: usize) -> u64 {
    let before = text.get(..offset).unwrap_or(text);
    before.bytes().filter(|&byte| byte == b'\n').count() as u64 + 1
}

/// Decodes JSON into typed records.
pub(crate) fn json<'a, T: Deserialize<'a>>(text: &'a str, file: &Path) -> Result<T, LockfileError> {
    serde_json::from_str(text).map_err(|error| parse_error(file, Some(error.line() as u64), error.to_string()))
}

/// Decodes TOML into typed records.
pub(crate) fn toml<T: DeserializeOwned>(text: &str, file: &Path) -> Result<T, LockfileError> {
    ::toml::from_str(text).map_err(|error| {
        let line = error.span().map(|span| line_at(text, span.start));
        parse_error(file, line, error.message())
    })
}

/// The YAML budget (design §23). Lockfiles use no anchors, so aliases are capped low while nodes and events allow a
/// 16 MiB file; a bomb trips the alias count, the alias-to-anchor ratio or the replayed-event cap first.
fn yaml_options() -> serde_saphyr::Options {
    let mut budget = serde_saphyr::Budget::default();
    budget.max_aliases = 1_000;
    budget.max_anchors = 1_000;
    budget.max_nodes = 4_000_000;
    budget.max_events = 16_000_000;
    let mut options = serde_saphyr::Options::default();
    options.budget = Some(budget);
    options.alias_limits.max_total_replayed_events = 100_000;
    options
}

/// Decodes YAML into typed records, within [`yaml_options`]'s budget.
pub(crate) fn yaml<T: DeserializeOwned>(text: &str, file: &Path) -> Result<T, LockfileError> {
    serde_saphyr::from_str_with_options(text, yaml_options()).map_err(|error: serde_saphyr::Error| {
        if is_budget(&error) {
            return LockfileError::YamlBudget {
                file: file.to_path_buf(),
            };
        }
        let line = error.location().map(|location| location.line());
        let message = error.to_string();
        parse_error(file, line, message.lines().next().unwrap_or_default())
    })
}

/// Whether `error`, once unwrapped from its snippet and alias context, is a budget breach.
fn is_budget(error: &serde_saphyr::Error) -> bool {
    use serde_saphyr::Error;
    match error.without_snippet() {
        Error::Budget { .. }
        | Error::AliasReplayLimitExceeded { .. }
        | Error::AliasReplayStackDepthExceeded { .. }
        | Error::AliasReplayCounterOverflow { .. } => true,
        Error::AliasError { error, .. } => is_budget(error),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    struct Doc {
        #[allow(dead_code, reason = "only decoding is tested")]
        name: String,
    }

    fn line_of<T: std::fmt::Debug>(result: Result<T, LockfileError>) -> Option<u64> {
        let error = result.unwrap_err();
        assert_eq!(error.cause(), crate::Cause::Parse, "{error}");
        let diagnostic = crate::Diagnostic::from(error);
        assert_eq!(diagnostic.file, Path::new("x"));
        diagnostic.line
    }

    #[test]
    fn malformed_json_toml_and_yaml_report_the_file_and_line() {
        assert_eq!(line_of(json::<Doc>("{\n\"name\": ,\n}", Path::new("x"))), Some(2));
        assert_eq!(line_of(toml::<Doc>("a = 1\nname = \n", Path::new("x"))), Some(2));
        assert_eq!(line_of(yaml::<Doc>("name: a\nname: [\n", Path::new("x"))), Some(2));
    }

    #[test]
    fn well_formed_documents_decode() {
        assert_eq!(json::<Doc>("{\"name\": \"a\"}", Path::new("x")).unwrap().name, "a");
        assert_eq!(toml::<Doc>("name = \"b\"", Path::new("x")).unwrap().name, "b");
        assert_eq!(yaml::<Doc>("name: c", Path::new("x")).unwrap().name, "c");
    }

    #[test]
    fn line_at_counts_newlines_before_the_offset() {
        assert_eq!(line_at("a\nb\nc", 0), 1);
        assert_eq!(line_at("a\nb\nc", 2), 2);
        assert_eq!(line_at("a\nb\nc", 99), 3);
    }
    #[test]
    fn a_yaml_anchor_bomb_exceeds_the_budget() {
        for (name, text) in [
            (
                "anchor-bomb.yaml",
                include_str!("../../../../fixtures/lockfiles/pnpm/anchor-bomb.yaml"),
            ),
            (
                "alias-flood.yaml",
                include_str!("../../../../fixtures/lockfiles/pnpm/alias-flood.yaml"),
            ),
        ] {
            let error = yaml::<serde::de::IgnoredAny>(text, Path::new(name)).err();
            assert!(
                matches!(error, Some(LockfileError::YamlBudget { ref file }) if file == Path::new(name)),
                "{name}: {error:?}"
            );
        }
    }

    #[test]
    fn a_large_yaml_file_without_aliases_stays_within_the_budget() {
        use std::fmt::Write;
        let mut text = String::from("packages:\n");
        for index in 0..100_000 {
            let _ = writeln!(text, "  p{index}@1.0.0: {{resolution: {{integrity: x}}}}");
        }
        let decoded: serde_json::Value = yaml(&text, Path::new("x")).unwrap();
        assert_eq!(decoded["packages"].as_object().map(serde_json::Map::len), Some(100_000));
    }

    #[test]
    fn one_yaml_type_meets_success_the_budget_and_a_parse_error() {
        use serde::de::IgnoredAny;
        let bomb = include_str!("../../../../fixtures/lockfiles/pnpm/anchor-bomb.yaml");
        assert!(yaml::<IgnoredAny>("a: 1\n", Path::new("x")).is_ok());
        let budget = yaml::<IgnoredAny>(bomb, Path::new("x")).unwrap_err();
        assert_eq!(budget.cause(), crate::Cause::YamlBudget);
        assert!(line_of(yaml::<IgnoredAny>("a: 1\nb: [\n", Path::new("x"))).is_some());
    }
}

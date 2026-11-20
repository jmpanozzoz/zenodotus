//! Identifier expressions in code and prose, such as `tokio::spawn`, `app.get`, `Foo#bar` and `node->next`.
//!
//! Shared by symbol extraction at ingestion and by the `code_ident` analyzer at search time, so both agree on what an
//! identifier is.

use std::ops::Range;

/// Separators that join identifiers into one qualified expression, longest first.
pub const SEPARATORS: [&str; 4] = ["::", "->", ".", "#"];

/// Whether `byte` can start an identifier.
const fn is_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$'
}

/// Whether `byte` can continue an identifier.
const fn is_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

/// Whether `byte` belongs to a word: an identifier byte or part of a non-ASCII character.
const fn is_word(byte: u8) -> bool {
    is_continue(byte) || byte >= 0x80
}

/// Byte ranges of the maximal identifier expressions in `text`, in order.
///
/// An expression is one or more ASCII identifiers joined by [`SEPARATORS`]. Words that touch non-ASCII letters or
/// start with a digit are skipped whole.
#[must_use]
pub fn expressions(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if !is_word(bytes[at]) {
            at += 1;
            continue;
        }
        let word_end = run_end(bytes, at);
        let pure_identifier = is_start(bytes[at]) && bytes[at..word_end].iter().all(|&b| is_continue(b));
        if !pure_identifier {
            at = word_end;
            continue;
        }
        let mut end = word_end;
        while let Some(next) = joined_identifier_end(bytes, end) {
            end = next;
        }
        found.push(at..end);
        at = end;
    }
    found
}

/// The end of the word run starting at `start`.
fn run_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|&b| !is_word(b))
        .map_or(bytes.len(), |offset| start + offset)
}

/// When a separator and a whole identifier follow `end`, the end of that identifier.
fn joined_identifier_end(bytes: &[u8], end: usize) -> Option<usize> {
    let rest = &bytes[end..];
    let separator = SEPARATORS.iter().find(|sep| rest.starts_with(sep.as_bytes()))?;
    let start = end + separator.len();
    if !bytes.get(start).copied().is_some_and(is_start) {
        return None;
    }
    let next_end = run_end(bytes, start);
    bytes[start..next_end]
        .iter()
        .all(|&b| is_continue(b))
        .then_some(next_end)
}

/// The identifiers of an expression, without separators: `a::b.c` gives `a`, `b`, `c`.
#[must_use]
pub fn parts(expression: &str) -> Vec<&str> {
    expression
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .filter(|part| !part.is_empty())
        .collect()
}

/// Whether an expression looks like code rather than an ordinary word: it is qualified, contains `_` between
/// letters, or has a lowercase letter followed by an uppercase one (`useEffect`, `HashMap`).
#[must_use]
pub fn is_symbol_like(expression: &str) -> bool {
    let qualified = SEPARATORS.iter().any(|sep| expression.contains(sep));
    let snake = expression.contains('_') && expression.chars().any(|c| c.is_ascii_alphanumeric());
    let camel = expression
        .as_bytes()
        .windows(2)
        .any(|pair| pair[0].is_ascii_lowercase() && pair[1].is_ascii_uppercase());
    qualified || snake || camel
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> Vec<&str> {
        expressions(text).into_iter().map(|range| &text[range]).collect()
    }

    #[test]
    fn finds_plain_and_qualified_identifiers() {
        assert_eq!(
            found("use tokio::sync::Mutex; app.get(\"/\") and Foo#bar or node->next"),
            [
                "use",
                "tokio::sync::Mutex",
                "app.get",
                "and",
                "Foo#bar",
                "or",
                "node->next"
            ]
        );
    }

    #[test]
    fn a_separator_must_be_followed_by_an_identifier() {
        assert_eq!(
            found("end. Next a:: b c-> d e.1"),
            ["end", "Next", "a", "b", "c", "d", "e"]
        );
        assert_eq!(found("trailing."), ["trailing"]);
        assert_eq!(found("x.y9z.é"), ["x.y9z"]);
    }

    #[test]
    fn skips_numbers_and_words_with_non_ascii_letters() {
        assert_eq!(found("3.14 2nd café naïve x"), ["x"]);
        assert_eq!(found("a.bé c"), ["a", "c"]);
        assert_eq!(found("ñandú_x"), Vec::<&str>::new());
    }

    #[test]
    fn dollar_and_underscore_are_identifier_characters() {
        assert_eq!(found("$ref _private __init__"), ["$ref", "_private", "__init__"]);
    }

    #[test]
    fn handles_empty_and_symbol_only_text() {
        assert_eq!(expressions(""), [] as [std::ops::Range<usize>; 0]);
        assert_eq!(expressions("-- {} ()"), [] as [std::ops::Range<usize>; 0]);
    }

    #[test]
    fn splits_expressions_into_identifiers() {
        assert_eq!(parts("std::sync::Arc"), ["std", "sync", "Arc"]);
        assert_eq!(parts("a->b#c.d"), ["a", "b", "c", "d"]);
        assert_eq!(parts("plain"), ["plain"]);
    }

    #[test]
    fn symbol_likeness() {
        for symbol in [
            "tokio::spawn",
            "app.get",
            "Foo#bar",
            "a->b",
            "read_item",
            "useEffect",
            "HashMap",
        ] {
            assert!(is_symbol_like(symbol), "{symbol}");
        }
        for word in ["spawn", "Mutex", "API", "_", "__", "Introduction"] {
            assert!(!is_symbol_like(word), "{word}");
        }
    }
}

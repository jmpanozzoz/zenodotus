//! Deterministic symbol extraction from headings, code and inline code spans.

use zenodotus_core::ident;

/// Characters that may follow an identifier in an inline code span that still names it, as in `spawn()`,
/// `vec!`, `Vec<T>` or `arr[0]`.
const CALL_SUFFIXES: [char; 4] = ['(', '!', '<', '['];

/// Code-like identifier expressions in `text` (see [`ident::is_symbol_like`]), in order, without repeats.
#[must_use]
pub fn extract(text: &str) -> Vec<String> {
    let mut symbols = Vec::new();
    for range in ident::expressions(text) {
        push_unique(&mut symbols, &text[range]);
    }
    symbols.retain(|symbol| ident::is_symbol_like(symbol));
    symbols
}

/// The identifier an inline code span names, when the whole span is one identifier expression, optionally followed
/// by a call, macro, generic or index suffix. Single characters are ignored.
#[must_use]
pub fn from_inline_code(code: &str) -> Option<String> {
    let code = code.trim();
    let first = ident::expressions(code).into_iter().next()?;
    let rest = &code[first.end..];
    let names_one_identifier = first.start == 0 && (rest.is_empty() || rest.starts_with(CALL_SUFFIXES));
    (names_one_identifier && first.len() > 1).then(|| code[first].to_owned())
}

/// Appends `symbol` unless it is already present.
pub fn push_unique(symbols: &mut Vec<String>, symbol: &str) {
    if !symbols.iter().any(|known| known == symbol) {
        symbols.push(symbol.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_code_like_identifiers_only() {
        assert_eq!(
            extract("use tokio::sync::Mutex; let read_item = useEffect(HashMap::new); fn spawn"),
            ["tokio::sync::Mutex", "read_item", "useEffect", "HashMap::new"]
        );
        assert_eq!(extract("Getting started with the API"), [] as [std::string::String; 0]);
    }

    #[test]
    fn extraction_removes_repeats() {
        assert_eq!(extract("a_b a_b x.y a_b"), ["a_b", "x.y"]);
    }

    #[test]
    fn inline_code_names_one_identifier() {
        for (code, expected) in [
            ("spawn", Some("spawn")),
            (" tokio::spawn ", Some("tokio::spawn")),
            ("spawn(future)", Some("spawn")),
            (concat!("print", "ln!"), Some(concat!("print", "ln"))),
            ("Vec<T>", Some("Vec")),
            ("arr[0]", Some("arr")),
            ("npm install vite", None),
            ("#[tokio::main]", None),
            ("x", None),
            ("x()", None),
            ("", None),
            ("--flag", None),
            ("a + b", None),
        ] {
            assert_eq!(from_inline_code(code).as_deref(), expected, "{code}");
        }
    }

    #[test]
    fn push_unique_keeps_the_first_occurrence() {
        let mut symbols = vec!["a".to_owned()];
        push_unique(&mut symbols, "b");
        push_unique(&mut symbols, "a");
        assert_eq!(symbols, ["a", "b"]);
    }
}

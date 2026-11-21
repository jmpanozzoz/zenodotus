//! The two analyzers of the index: `code_words` for the full-text field and `code_ident` for whole identifiers.
//!
//! Queries are analysed with the same functions, [`code_words`] and [`code_ident`], so index and query terms agree.

use tantivy::Index;
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream, Tokenizer};
use zenodotus_core::ident;

/// Name the `code_words` analyzer is registered under.
pub const CODE_WORDS: &str = "code_words";
/// Name the `code_ident` analyzer is registered under.
pub const CODE_IDENT: &str = "code_ident";

/// Tokens of this many bytes or more are dropped.
const MAX_TOKEN_BYTES: usize = 64;

/// Whether `c` belongs to a word.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The words of `text`: protected words whole, everything else split on non-word characters, then at camelCase,
/// acronym and `_` boundaries. Lowercased, with consecutive positions.
#[must_use]
pub fn code_words(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut at = 0;
    let mut previous_is_word = false;
    while let Some(c) = text[at..].chars().next() {
        if !previous_is_word && let Some(len) = protected_at(text, at) {
            push(&mut tokens, text, at..at + len);
            at += len;
            previous_is_word = false;
            continue;
        }
        if is_word(c) {
            let end = text[at..]
                .find(|c: char| !is_word(c))
                .map_or(text.len(), |offset| at + offset);
            for part in split_identifier(text, at, end) {
                push(&mut tokens, text, part);
            }
            at = end;
            previous_is_word = true;
        } else {
            at += c.len_utf8();
            previous_is_word = false;
        }
    }
    tokens
}

/// The byte length of the protected word starting at `at`, when one does and no word character follows it.
fn protected_at(text: &str, at: usize) -> Option<usize> {
    let rest = &text.as_bytes()[at..];
    ident::PROTECTED_WORDS
        .iter()
        .find(|word| {
            rest.get(..word.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(word.as_bytes()))
                && !text[at + word.len()..].starts_with(is_word)
        })
        .map(|word| word.len())
}

/// The byte ranges of the parts of the word `text[start..end]`, split at `_`, at a lowercase letter or digit followed
/// by an uppercase one, and before the last uppercase letter of an acronym followed by a lowercase one.
fn split_identifier(text: &str, start: usize, end: usize) -> Vec<std::ops::Range<usize>> {
    let chars: Vec<(usize, char)> = text[start..end].char_indices().map(|(i, c)| (start + i, c)).collect();
    let mut parts = Vec::new();
    let mut part_start = start;
    for (i, &(offset, c)) in chars.iter().enumerate() {
        if c == '_' {
            parts.push(part_start..offset);
            part_start = offset + 1;
            continue;
        }
        let previous = i.checked_sub(1).map(|j| chars[j].1);
        let next = chars.get(i + 1).map(|&(_, c)| c);
        let camel = previous.is_some_and(|p| p.is_lowercase() || p.is_numeric()) && c.is_uppercase();
        let acronym_end =
            previous.is_some_and(char::is_uppercase) && c.is_uppercase() && next.is_some_and(char::is_lowercase);
        if camel || acronym_end {
            parts.push(part_start..offset);
            part_start = offset;
        }
    }
    parts.push(part_start..end);
    parts.retain(|part| !part.is_empty());
    parts
}

/// Appends the lowercased `text[range]` at the next position, unless it is too long.
fn push(tokens: &mut Vec<Token>, text: &str, range: std::ops::Range<usize>) {
    let word = text[range.clone()].to_lowercase();
    if word.len() >= MAX_TOKEN_BYTES {
        return;
    }
    tokens.push(Token {
        offset_from: range.start,
        offset_to: range.end,
        position: tokens.len(),
        text: word,
        position_length: 1,
    });
}

/// The identifier terms of `text`, without repeats: each identifier expression with its separators turned into `.`,
/// lowercased, plus every qualified-path suffix, plus each of those with `_` removed.
#[must_use]
pub fn code_ident(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for range in ident::expressions(text) {
        let parts: Vec<String> = ident::parts(&text[range])
            .iter()
            .map(|part| part.to_lowercase())
            .collect();
        for start in 0..parts.len() {
            let suffix = parts[start..].join(".");
            let joined = suffix.replace('_', "");
            for term in [suffix, joined] {
                if term.contains(|c: char| c.is_ascii_alphanumeric()) && !terms.contains(&term) {
                    terms.push(term);
                }
            }
        }
    }
    terms
}

/// The exact-symbol terms of a query: the [`code_ident`] terms of each expression that looks like code
/// ([`ident::is_symbol_like`]), keeping only the qualified forms of a qualified expression. Plain words and the bare
/// last part of a path are left to the full-text field, which already scores them.
#[must_use]
pub fn query_symbols(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for range in ident::expressions(text) {
        let expression = &text[range];
        if !ident::is_symbol_like(expression) {
            continue;
        }
        let qualified = ident::parts(expression).len() > 1;
        for term in code_ident(expression) {
            if (!qualified || term.contains('.')) && !terms.contains(&term) {
                terms.push(term);
            }
        }
    }
    terms
}

/// A token stream over precomputed tokens.
#[derive(Debug)]
pub struct Tokens {
    tokens: Vec<Token>,
    next: usize,
}

impl Tokens {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, next: 0 }
    }
}

impl TokenStream for Tokens {
    fn advance(&mut self) -> bool {
        self.next += 1;
        self.next <= self.tokens.len()
    }

    fn token(&self) -> &Token {
        &self.tokens[self.next - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.next - 1]
    }
}

/// The `code_words` analyzer.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodeWordsTokenizer;

impl Tokenizer for CodeWordsTokenizer {
    type TokenStream<'a> = Tokens;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Tokens {
        Tokens::new(code_words(text))
    }
}

/// The `code_ident` analyzer.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodeIdentTokenizer;

impl Tokenizer for CodeIdentTokenizer {
    type TokenStream<'a> = Tokens;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Tokens {
        let tokens = code_ident(text)
            .into_iter()
            .enumerate()
            .map(|(position, term)| Token {
                offset_from: 0,
                offset_to: text.len(),
                position,
                text: term,
                position_length: 1,
            })
            .collect();
        Tokens::new(tokens)
    }
}

/// Registers both analyzers on `index`. Called every time an index is opened.
pub fn register(index: &Index) {
    index
        .tokenizers()
        .register(CODE_WORDS, TextAnalyzer::from(CodeWordsTokenizer));
    index
        .tokenizers()
        .register(CODE_IDENT, TextAnalyzer::from(CodeIdentTokenizer));
}

#[cfg(test)]
mod tests {
    use tantivy::schema::Schema;

    use super::*;

    fn words(text: &str) -> Vec<String> {
        code_words(text).into_iter().map(|token| token.text).collect()
    }

    #[test]
    fn splits_on_non_word_characters_and_lowercases() {
        assert_eq!(words("How do I run   tests?"), ["how", "do", "i", "run", "tests"]);
        assert_eq!(words("Ünïcode Straße 42"), ["ünïcode", "straße", "42"]);
        assert_eq!(words(""), [] as [String; 0]);
        assert_eq!(words("-- // !!"), [] as [String; 0]);
    }

    #[test]
    fn splits_identifiers_at_case_and_underscore_boundaries() {
        assert_eq!(words("getUserName"), ["get", "user", "name"]);
        assert_eq!(words("HTTPServer parseHTML"), ["http", "server", "parse", "html"]);
        assert_eq!(words("utf8String base64"), ["utf8", "string", "base64"]);
        assert_eq!(words("__init__ snake_case_name"), ["init", "snake", "case", "name"]);
        assert_eq!(words("tokio::sync::Mutex"), ["tokio", "sync", "mutex"]);
        assert_eq!(words("ABC"), ["abc"]);
    }

    #[test]
    fn protected_words_are_emitted_whole_when_they_stand_alone() {
        assert_eq!(
            words("C++ and C# or F#, .NET on Node.js"),
            ["c++", "and", "c#", "or", "f#", ".net", "on", "node.js"]
        );
        assert_eq!(
            words("asp.net node.jsx abc++ c#d"),
            ["asp", "net", "node", "jsx", "abc", "c", "d"]
        );
        assert_eq!(words("(c++)"), ["c++"]);
    }

    #[test]
    fn tokens_carry_offsets_and_consecutive_positions() {
        let tokens = code_words("a fooBar c++");
        let summary: Vec<_> = tokens
            .iter()
            .map(|t| {
                (
                    t.text.as_str(),
                    t.offset_from,
                    t.offset_to,
                    t.position,
                    t.position_length,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("a", 0, 1, 0, 1),
                ("foo", 2, 5, 1, 1),
                ("bar", 5, 8, 2, 1),
                ("c++", 9, 12, 3, 1)
            ]
        );
    }

    #[test]
    fn long_tokens_are_dropped_without_taking_a_position() {
        let long = "x".repeat(MAX_TOKEN_BYTES);
        let just_short = "y".repeat(MAX_TOKEN_BYTES - 1);
        let tokens = code_words(&format!("a {long} {just_short} b"));
        let summary: Vec<_> = tokens.iter().map(|t| (t.text.len(), t.position)).collect();
        assert_eq!(summary, [(1, 0), (MAX_TOKEN_BYTES - 1, 1), (1, 2)]);
    }

    #[test]
    fn identifiers_give_canonical_forms_suffixes_and_joined_forms() {
        assert_eq!(code_ident("std::sync::Arc"), ["std.sync.arc", "sync.arc", "arc"]);
        assert_eq!(
            code_ident("getUserName get_user_name"),
            ["getusername", "get_user_name"]
        );
        assert_eq!(
            code_ident("node->next_item app.get"),
            [
                "node.next_item",
                "node.nextitem",
                "next_item",
                "nextitem",
                "app.get",
                "get"
            ]
        );
        assert_eq!(code_ident("Foo#bar foo#bar"), ["foo.bar", "bar"]);
        assert_eq!(code_ident("a + b"), ["a", "b"]);
        assert_eq!(code_ident("__"), [] as [String; 0]);
        assert_eq!(code_ident("—"), [] as [String; 0]);
    }

    #[test]
    fn query_symbols_are_only_what_looks_like_code_in_its_qualified_forms() {
        assert_eq!(
            query_symbols("run my own validator function on a query"),
            [] as [String; 0]
        );
        assert_eq!(query_symbols("How do I use tokio::spawn here"), ["tokio.spawn"]);
        assert_eq!(query_symbols("std::sync::Arc"), ["std.sync.arc", "sync.arc"]);
        assert_eq!(
            query_symbols("BaseModel with validate_call, again validate_call"),
            ["basemodel", "validate_call", "validatecall"]
        );
        assert_eq!(query_symbols("node->next_item"), ["node.next_item", "node.nextitem"]);
    }

    #[test]
    fn tokenizers_stream_the_same_terms() {
        let mut stream = CodeWordsTokenizer.token_stream("fooBar");
        assert!(stream.advance());
        assert_eq!(stream.token().text, "foo");
        stream.token_mut().text.push('!');
        assert_eq!(stream.token().text, "foo!");
        assert!(stream.advance());
        assert_eq!(stream.token().text, "bar");
        assert!(!stream.advance());

        let mut stream = CodeIdentTokenizer.token_stream("a::b");
        let mut seen = Vec::new();
        while let Some(token) = stream.next() {
            seen.push((token.text.clone(), token.position, token.offset_to));
        }
        assert_eq!(seen, [("a.b".to_owned(), 0, 4), ("b".to_owned(), 1, 4)]);
    }

    #[test]
    fn register_makes_both_analyzers_available() {
        let index = Index::create_in_ram(Schema::builder().build());
        register(&index);
        let mut words = index.tokenizers().get(CODE_WORDS).unwrap();
        let mut stream = words.token_stream("fooBar");
        assert!(stream.advance());
        assert_eq!(stream.token().text, "foo");
        let mut ident = index.tokenizers().get(CODE_IDENT).unwrap();
        let mut stream = ident.token_stream("x.y");
        assert!(stream.advance());
        assert_eq!(stream.token().text, "x.y");
    }
}

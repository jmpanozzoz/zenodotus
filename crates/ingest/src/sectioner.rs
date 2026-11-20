//! Turns a [`ParsedDoc`] into sections: one per heading of level 1 to 4, heading-only sections merged forward, long
//! sections split at block boundaries that never separate code from the paragraph before it.

use std::ops::Range;

use zenodotus_core::ids::{ChunkId, DocKey, Scope};
use zenodotus_core::model::{Section, content_hash};

use crate::markdown::{BlockKind, ParsedDoc};
use crate::symbols;

/// Deepest heading level that opens a section.
const MAX_SECTION_LEVEL: u8 = 4;

/// Size rules for sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionRules {
    /// Sections longer than this, in characters, are split.
    pub max_chars: usize,
    /// Sections shorter than this merge into the next one; 0 disables merging by size.
    pub merge_below_chars: usize,
}

impl Default for SectionRules {
    fn default() -> Self {
        Self {
            max_chars: 6000,
            merge_below_chars: 0,
        }
    }
}

/// How a section points back at its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceRef<'a> {
    /// A file under a local source root, by its relative path.
    Local(&'a str),
    /// A canonical URL; sections add `#anchor`.
    Url(&'a str),
}

/// One section before it becomes a [`Section`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outline {
    range: Range<usize>,
    heading_path: Vec<String>,
    anchor: Option<String>,
    has_content: bool,
}

/// Sections of `parsed`, in order. `doc_key` must belong to `scope`.
#[must_use]
pub fn sectionize(
    scope: &Scope,
    doc_key: &DocKey,
    source: SourceRef<'_>,
    parsed: &ParsedDoc,
    rules: &SectionRules,
) -> Vec<Section> {
    let doc_path = doc_key.doc_path();
    let title = parsed.title.clone().unwrap_or_else(|| file_stem(doc_path));
    let mut sections = Vec::new();
    for outline in merge(outline(parsed), &parsed.text, rules) {
        let parts = split(&outline.range, parsed, rules.max_chars);
        let numbered = parts.len() > 1;
        for (index, range) in parts.into_iter().enumerate() {
            let ordinal = u32::try_from(sections.len()).unwrap_or(u32::MAX);
            let mut heading_path = outline.heading_path.clone();
            let mut section_title = title.clone();
            if numbered {
                let label = heading_path.last_mut().unwrap_or(&mut section_title);
                *label = format!("{label} (part {})", index + 1);
            }
            let text = parsed.text[range.clone()].trim().to_owned();
            sections.push(Section {
                chunk_id: ChunkId::derive(scope, doc_path, ordinal),
                doc_key: doc_key.clone(),
                ordinal,
                title: section_title,
                heading_path,
                code_languages: code_languages(parsed, &range),
                symbols: section_symbols(parsed, &range),
                source_ref: source_ref(source, outline.anchor.as_deref()),
                content_hash: content_hash(&text),
                text,
            });
        }
    }
    sections
}

/// The file name of `doc_path` without its extension.
fn file_stem(doc_path: &str) -> String {
    std::path::Path::new(doc_path)
        .file_stem()
        .map_or_else(|| doc_path.to_owned(), |stem| stem.to_string_lossy().into_owned())
}

/// The raw sections: the preamble before the first heading, then one per heading of level 1 to 4.
fn outline(parsed: &ParsedDoc) -> Vec<Outline> {
    let mut outlines = vec![Outline {
        range: 0..0,
        heading_path: Vec::new(),
        anchor: None,
        has_content: false,
    }];
    let mut stack: Vec<(u8, String)> = Vec::new();
    for block in &parsed.blocks {
        let current = outlines.len() - 1;
        match &block.kind {
            BlockKind::Heading { level, text, anchor } if *level <= MAX_SECTION_LEVEL => {
                stack.retain(|(open, _)| open < level);
                stack.push((*level, text.clone()));
                outlines[current].range.end = block.range.start;
                outlines.push(Outline {
                    range: block.range.start..block.range.start,
                    heading_path: stack.iter().map(|(_, text)| text.clone()).collect(),
                    anchor: Some(anchor.clone()),
                    has_content: false,
                });
            }
            _ => outlines[current].has_content = true,
        }
    }
    let last = outlines.len() - 1;
    outlines[last].range.end = parsed.text.len();
    outlines
}

/// Drops a blank preamble and merges heading-only (or, with `merge_below_chars`, short) sections into the next.
fn merge(outlines: Vec<Outline>, text: &str, rules: &SectionRules) -> Vec<Outline> {
    let count = outlines.len();
    let mut merged: Vec<Outline> = Vec::with_capacity(count);
    let mut carried_start: Option<usize> = None;
    for (index, mut outline) in outlines.into_iter().enumerate() {
        if let Some(start) = carried_start.take() {
            outline.range.start = start;
        }
        let body = &text[outline.range.clone()];
        let is_preamble = outline.anchor.is_none();
        if is_preamble {
            if !body.trim().is_empty() {
                merged.push(outline);
            }
            continue;
        }
        let too_small = body.chars().count() < rules.merge_below_chars;
        if (!outline.has_content || too_small) && index + 1 < count {
            carried_start = Some(outline.range.start);
        } else {
            merged.push(outline);
        }
    }
    merged
}

/// Splits `range` into parts of at most `max_chars` where possible. Parts break only at the start of a block that is
/// not code; a block longer than the limit becomes a part of its own.
fn split(range: &Range<usize>, parsed: &ParsedDoc, max_chars: usize) -> Vec<Range<usize>> {
    let mut bounds = vec![range.start];
    bounds.extend(
        parsed
            .blocks
            .iter()
            .filter(|block| block.range.start > range.start && block.range.start < range.end)
            .filter(|block| !matches!(block.kind, BlockKind::Code { .. }))
            .map(|block| block.range.start),
    );
    bounds.push(range.end);
    let chars = |part: &Range<usize>| parsed.text[part.clone()].chars().count();
    let mut parts: Vec<Range<usize>> = Vec::new();
    for piece in bounds.windows(2).map(|pair| pair[0]..pair[1]) {
        match parts.last_mut() {
            Some(last) if chars(&(last.start..piece.end)) <= max_chars => last.end = piece.end,
            _ => parts.push(piece),
        }
    }
    parts
}

/// Languages of the code blocks inside `range`, in order, without repeats.
fn code_languages(parsed: &ParsedDoc, range: &Range<usize>) -> Vec<String> {
    let mut languages = Vec::new();
    for block in blocks_in(parsed, range) {
        if let BlockKind::Code { lang: Some(lang), .. } = &block.kind {
            symbols::push_unique(&mut languages, lang);
        }
    }
    languages
}

/// Symbols from the headings, code first lines and inline code spans inside `range`.
fn section_symbols(parsed: &ParsedDoc, range: &Range<usize>) -> Vec<String> {
    let mut found = Vec::new();
    for block in blocks_in(parsed, range) {
        let text = match &block.kind {
            BlockKind::Heading { text, .. } => text,
            BlockKind::Code { first_line, .. } => first_line,
            BlockKind::Prose | BlockKind::Other => continue,
        };
        for symbol in symbols::extract(text) {
            symbols::push_unique(&mut found, &symbol);
        }
    }
    let spans = parsed.inline_code.iter().filter(|span| range.contains(&span.offset));
    for symbol in spans.filter_map(|span| symbols::from_inline_code(&span.code)) {
        symbols::push_unique(&mut found, &symbol);
    }
    found
}

/// The blocks that start inside `range`.
fn blocks_in<'a>(parsed: &'a ParsedDoc, range: &'a Range<usize>) -> impl Iterator<Item = &'a crate::markdown::Block> {
    parsed.blocks.iter().filter(|block| range.contains(&block.range.start))
}

/// `local:<path>`, or the URL with the section anchor.
fn source_ref(source: SourceRef<'_>, anchor: Option<&str>) -> String {
    match (source, anchor) {
        (SourceRef::Local(path), _) => format!("local:{path}"),
        (SourceRef::Url(url), Some(anchor)) => format!("{url}#{anchor}"),
        (SourceRef::Url(url), None) => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use zenodotus_core::ids::{LibraryId, Version};

    use super::*;
    use crate::markdown::parse;

    fn scope() -> Scope {
        Scope::new(LibraryId::parse("lib").unwrap(), Version::parse("1").unwrap())
    }

    fn sections_with(path: &str, text: &str, rules: &SectionRules) -> Vec<Section> {
        let key = DocKey::new(&scope(), path).unwrap();
        sectionize(&scope(), &key, SourceRef::Local(path), &parse(path, text), rules)
    }

    fn sections(text: &str) -> Vec<Section> {
        sections_with("guide/intro.md", text, &SectionRules::default())
    }

    fn fixture(name: &str) -> String {
        let path = format!("{}/../../fixtures/markdown/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).unwrap()
    }

    fn outline_of(sections: &[Section]) -> Vec<String> {
        sections
            .iter()
            .map(|s| format!("{} | {}", s.title, s.heading_path.join(" > ")))
            .collect()
    }

    #[test]
    fn basic_document() {
        let found = sections(&fixture("basic.md"));
        assert_eq!(
            outline_of(&found),
            ["Basic | Basic", "Basic | Basic > Install", "Basic | Basic > Usage"]
        );
        assert!(found[1].text.starts_with("## Install"));
        assert_eq!(found[2].code_languages, ["toml", "rust"]);
        let ordinals: Vec<u32> = found.iter().map(|s| s.ordinal).collect();
        assert_eq!(ordinals, [0, 1, 2]);
        assert_eq!(found[0].chunk_id, ChunkId::derive(&scope(), "guide/intro.md", 0));
        assert_eq!(found[1].content_hash, content_hash(&found[1].text));
        assert_eq!(found[1].source_ref, "local:guide/intro.md");
    }

    #[test]
    fn nested_headings_build_a_path_and_deep_levels_stay_inside() {
        let found = sections(&fixture("nested_headings.md"));
        assert_eq!(
            outline_of(&found),
            [
                "Runtime | Runtime",
                "Runtime | Runtime > Tasks",
                "Runtime | Runtime > Tasks > Spawning",
                "Runtime | Runtime > Tasks > Spawning > Blocking",
                "Runtime | Runtime > Timers",
            ]
        );
        assert!(found[3].text.contains("##### Details"));
    }

    #[test]
    fn code_stays_with_the_paragraph_before_it() {
        let found = sections_with(
            "a.md",
            &fixture("code_after_prose.md"),
            &SectionRules {
                max_chars: 120,
                merge_below_chars: 0,
            },
        );
        for section in &found {
            assert!(!section.text.starts_with("```"), "{}", section.text);
        }
        let with_code = found.iter().find(|s| s.text.contains("tokio::spawn")).unwrap();
        assert!(with_code.text.starts_with("Spawn a task"));
        assert_eq!(with_code.code_languages, ["rust"]);
    }

    #[test]
    fn long_sections_split_into_numbered_parts() {
        let found = sections_with(
            "a.md",
            &fixture("long_section.md"),
            &SectionRules {
                max_chars: 300,
                merge_below_chars: 0,
            },
        );
        let titles: Vec<&str> = found.iter().map(|s| s.heading_path[0].as_str()).collect();
        assert_eq!(titles, ["Long (part 1)", "Long (part 2)", "Long (part 3)"]);
        assert!(found.iter().all(|s| s.text.chars().count() <= 300));
    }

    #[test]
    fn a_block_longer_than_the_limit_is_its_own_part() {
        let long = "x".repeat(50);
        let text = format!("# T\n\n{long}\n\nshort\n");
        let found = sections_with(
            "a.md",
            &text,
            &SectionRules {
                max_chars: 20,
                merge_below_chars: 0,
            },
        );
        let texts: Vec<&str> = found.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["# T", long.as_str(), "short"]);
    }

    #[test]
    fn an_untitled_split_preamble_numbers_the_document_title() {
        let text = format!("{}\n\n{}\n", "a".repeat(30), "b".repeat(30));
        let found = sections_with(
            "docs/page.md",
            &text,
            &SectionRules {
                max_chars: 40,
                merge_below_chars: 0,
            },
        );
        assert_eq!(outline_of(&found), ["page (part 1) | ", "page (part 2) | "]);
    }

    #[test]
    fn front_matter_titles_every_section() {
        let found = sections_with("guide/page.mdx", &fixture("front_matter.mdx"), &SectionRules::default());
        assert!(found.iter().all(|s| s.title == "Front matter page"));
        assert!(found.iter().all(|s| !s.text.contains("import ")));
    }

    #[test]
    fn a_document_without_headings_is_one_section_titled_by_its_file() {
        let found = sections(&fixture("no_headings.md"));
        assert_eq!(outline_of(&found), ["intro | "]);
        assert_eq!(found[0].text, fixture("no_headings.md").trim());
    }

    #[test]
    fn heading_only_sections_merge_forward_and_a_final_one_stays() {
        let found = sections(&fixture("heading_only.md"));
        assert_eq!(outline_of(&found), ["API | API > spawn", "API | API > Last"]);
        assert!(found[0].text.starts_with("# API\n\n## spawn"));
        assert_eq!(found[1].text, "## Last");
    }

    #[test]
    fn a_blank_preamble_is_dropped_and_a_real_one_kept() {
        assert_eq!(outline_of(&sections("\n\n# A\n\ntext\n")), ["A | A"]);
        assert_eq!(outline_of(&sections("Intro text.\n\n# A\n\ntext\n")), ["A | ", "A | A"]);
    }

    #[test]
    fn merge_below_chars_merges_short_sections() {
        let text = "# A\n\nx\n\n# B\n\nlonger text here\n";
        let found = sections_with(
            "a.md",
            text,
            &SectionRules {
                max_chars: 6000,
                merge_below_chars: 10,
            },
        );
        assert_eq!(outline_of(&found), ["A | B"]);
        assert!(found[0].text.starts_with("# A"));
    }

    #[test]
    fn symbols_come_from_headings_code_and_inline_spans() {
        let found = sections(
            "# The `spawn` function\n\nCall `tokio::spawn()` or `JoinHandle`.\nUse `npm install`.\n\n```rust\nlet handle = tokio::task::spawn_blocking(f);\n```\n\n## read_item\n\ntext\n",
        );
        assert_eq!(
            found[0].symbols,
            ["tokio::task::spawn_blocking", "spawn", "tokio::spawn", "JoinHandle"]
        );
        assert_eq!(found[1].symbols, ["read_item"]);
    }

    #[test]
    fn url_sources_link_each_section_anchor() {
        let key = DocKey::new(&scope(), "intro").unwrap();
        let parsed = parse("intro", "Preamble\n\n# Getting Started\n\ntext\n");
        let found = sectionize(
            &scope(),
            &key,
            SourceRef::Url("https://x.dev/intro"),
            &parsed,
            &SectionRules::default(),
        );
        let refs: Vec<&str> = found.iter().map(|s| s.source_ref.as_str()).collect();
        assert_eq!(refs, ["https://x.dev/intro", "https://x.dev/intro#getting-started"]);
    }

    #[test]
    fn an_empty_document_has_no_sections() {
        assert_eq!(sections(""), [] as [zenodotus_core::model::Section; 0]);
        assert_eq!(sections("  \n\n"), [] as [zenodotus_core::model::Section; 0]);
    }

    #[test]
    fn file_stems_fall_back_to_the_path() {
        assert_eq!(file_stem("a/b/page.md"), "page");
        assert_eq!(file_stem(".."), "..");
    }

    fn markdown_line() -> impl Strategy<Value = String> {
        prop_oneof![
            "[a-z ]{0,30}",
            "#{1,6} [a-z]{1,10}",
            Just("```rust".to_owned()),
            Just("```".to_owned()),
            Just(String::new()),
            "- [a-z]{1,10}",
            "`[a-z_:]{1,12}`",
            Just("---".to_owned()),
            Just("<div>".to_owned()),
            "[ä-ö]{1,4}",
        ]
    }

    proptest! {
        #[test]
        fn sections_cover_every_non_blank_byte_once(
            lines in proptest::collection::vec(markdown_line(), 0..40),
            max_chars in 10usize..400,
            merge_below in 0usize..30,
        ) {
            let text = lines.join("\n");
            let parsed = parse("p.md", &text);
            let rules = SectionRules { max_chars, merge_below_chars: merge_below };
            let mut covered = vec![0u8; parsed.text.len()];
            for outline in merge(outline(&parsed), &parsed.text, &rules) {
                for range in split(&outline.range, &parsed, rules.max_chars) {
                    for byte in &mut covered[range] {
                        *byte += 1;
                    }
                }
            }
            for (index, count) in covered.iter().enumerate() {
                let blank = parsed.text.as_bytes()[index].is_ascii_whitespace();
                prop_assert!(*count == 1 || (*count == 0 && blank), "byte {} covered {} times", index, count);
            }
        }
    }
}

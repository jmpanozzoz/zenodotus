//! Markdown parsing: front matter and MDX stripping, then the top-level blocks of the document with byte ranges.

use std::collections::BTreeMap;
use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag};

/// A parsed document. Every range points into `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDoc {
    /// The front-matter `title:`, else the text of the first level-1 heading.
    pub title: Option<String>,
    /// The input after front-matter and MDX stripping.
    pub text: String,
    /// Top-level blocks in document order.
    pub blocks: Vec<Block>,
    /// Inline code spans in document order, headings included.
    pub inline_code: Vec<InlineCode>,
}

/// One top-level block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// What the block is.
    pub kind: BlockKind,
    /// Its bytes in [`ParsedDoc::text`].
    pub range: Range<usize>,
}

/// The kinds of top-level block that sectioning distinguishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    /// A heading.
    Heading {
        /// 1 to 6.
        level: u8,
        /// The heading as plain text.
        text: String,
        /// The explicit `{#id}`, else a GitHub-style slug of the text, unique within the document.
        anchor: String,
    },
    /// Paragraphs, lists, block quotes and tables.
    Prose,
    /// A fenced or indented code block.
    Code {
        /// The first word of the fence info string, lowercased.
        lang: Option<String>,
        /// The first line of the code.
        first_line: String,
    },
    /// HTML blocks and thematic breaks.
    Other,
}

/// An inline code span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineCode {
    /// Byte offset of the span in [`ParsedDoc::text`].
    pub offset: usize,
    /// The code, without backticks.
    pub code: String,
}

/// Parses `text`. MDX stripping applies to `.mdx` documents only.
#[must_use]
pub fn parse(doc_path: &str, text: &str) -> ParsedDoc {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (front_title, body) = split_front_matter(text);
    let text = if has_mdx_extension(doc_path) {
        strip_mdx(body)
    } else {
        body.to_owned()
    };
    let mut builder = Builder::default();
    let mut options = Options::empty();
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    for (event, range) in Parser::new_ext(&text, options).into_offset_iter() {
        builder.event(event, range);
    }
    let title = front_title.or_else(|| {
        builder.blocks.iter().find_map(|block| match &block.kind {
            BlockKind::Heading { level: 1, text, .. } => Some(text.clone()),
            _ => None,
        })
    });
    ParsedDoc {
        title,
        text,
        blocks: builder.blocks,
        inline_code: builder.inline_code,
    }
}

/// Whether `doc_path` ends in `.mdx`, ignoring ASCII case.
fn has_mdx_extension(doc_path: &str) -> bool {
    std::path::Path::new(doc_path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mdx"))
}

/// Accumulates top-level blocks from the event stream.
#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    inline_code: Vec<InlineCode>,
    depth: usize,
    open: Option<Open>,
    anchors: BTreeMap<String, usize>,
}

/// The top-level block being read.
struct Open {
    tag: OpenTag,
    range: Range<usize>,
    text: String,
}

/// What the open block will become.
enum OpenTag {
    Heading { level: u8, id: Option<String> },
    Code { lang: Option<String> },
    Prose,
    Other,
}

impl Builder {
    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        match event {
            Event::Start(tag) => {
                if self.depth == 0 {
                    self.open = Some(Open {
                        tag: open_tag(&tag),
                        range: range.clone(),
                        text: String::new(),
                    });
                }
                self.depth += 1;
            }
            Event::End(_) => {
                self.depth -= 1;
                if self.depth == 0 {
                    self.close();
                }
            }
            Event::Code(code) => {
                self.inline_code.push(InlineCode {
                    offset: range.start,
                    code: code.to_string(),
                });
                self.push_text(&code);
            }
            Event::Text(text) => self.push_text(&text),
            Event::SoftBreak | Event::HardBreak => self.push_text(" "),
            Event::Rule if self.depth == 0 => self.blocks.push(Block {
                kind: BlockKind::Other,
                range,
            }),
            _ => {}
        }
    }

    /// Text is only reported inside a block, so an open block always exists here.
    fn push_text(&mut self, text: &str) {
        self.open.iter_mut().for_each(|open| open.text.push_str(text));
    }

    /// Ends the open top-level block.
    fn close(&mut self) {
        let block = self.open.take().map(|open| self.finish(open));
        self.blocks.extend(block);
    }

    fn finish(&mut self, open: Open) -> Block {
        let kind = match open.tag {
            OpenTag::Heading { level, id } => {
                let text = open.text.trim().to_owned();
                let anchor = self.unique_anchor(id.unwrap_or_else(|| slug(&text)));
                BlockKind::Heading { level, text, anchor }
            }
            OpenTag::Code { lang } => BlockKind::Code {
                lang,
                first_line: open.text.lines().next().unwrap_or_default().trim().to_owned(),
            },
            OpenTag::Prose => BlockKind::Prose,
            OpenTag::Other => BlockKind::Other,
        };
        Block {
            kind,
            range: open.range,
        }
    }

    /// GitHub's rule: the second `intro` becomes `intro-1`, the third `intro-2`.
    fn unique_anchor(&mut self, anchor: String) -> String {
        let seen = self.anchors.entry(anchor.clone()).or_insert(0);
        let unique = if *seen == 0 { anchor } else { format!("{anchor}-{seen}") };
        *seen += 1;
        unique
    }
}

/// Classifies a top-level tag.
fn open_tag(tag: &Tag<'_>) -> OpenTag {
    match tag {
        Tag::Heading { level, id, .. } => OpenTag::Heading {
            level: heading_level(*level),
            id: id.as_ref().map(ToString::to_string),
        },
        Tag::CodeBlock(CodeBlockKind::Fenced(info)) => OpenTag::Code {
            lang: info.split_whitespace().next().map(str::to_lowercase),
        },
        Tag::CodeBlock(CodeBlockKind::Indented) => OpenTag::Code { lang: None },
        Tag::HtmlBlock => OpenTag::Other,
        _ => OpenTag::Prose,
    }
}

/// 1 for `#`, up to 6.
fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// A GitHub-style anchor: lowercase letters, digits, `-` and `_`, with spaces turned into `-`.
#[must_use]
pub fn slug(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            '-' | '_' => Some(c),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// Splits a leading `---` front-matter block from the body and reads its `title:` line.
fn split_front_matter(text: &str) -> (Option<String>, &str) {
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else {
        return (None, text);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let content = line.trim_end_matches(['\r', '\n']);
        if content == "---" || content == "..." {
            let title = rest[..offset].lines().find_map(title_value);
            return (title, &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, text)
}

/// The value of a `title:` front-matter line, unquoted.
fn title_value(line: &str) -> Option<String> {
    let value = line.strip_prefix("title:")?.trim();
    let unquoted = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value);
    (!unquoted.is_empty()).then(|| unquoted.to_owned())
}

/// Removes MDX `import`/`export` statements and self-closing JSX component lines outside code fences. A statement
/// runs on while its braces, outside string literals, are open, and always ends at a blank line, as MDX's own ends.
fn strip_mdx(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut fence: Option<Fence> = None;
    let mut open_braces = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if let Some(open) = fence {
            if open.is_closed_by(trimmed) {
                fence = None;
            }
            kept.push_str(line);
            continue;
        }
        if open_braces > 0 {
            if trimmed.is_empty() {
                open_braces = 0;
                kept.push_str(line);
            } else {
                open_braces = brace_balance(open_braces, trimmed);
            }
            continue;
        }
        if let Some(open) = Fence::opened_by(trimmed) {
            fence = Some(open);
            kept.push_str(line);
        } else if is_module_statement(trimmed) {
            open_braces = brace_balance(0, trimmed);
        } else if !is_self_closing_component(trimmed) {
            kept.push_str(line);
        }
    }
    kept
}

/// An open code fence: its character and how many of them opened it.
#[derive(Clone, Copy)]
struct Fence {
    marker: char,
    len: usize,
}

impl Fence {
    /// The fence `line` opens: three or more backticks or tildes.
    fn opened_by(line: &str) -> Option<Self> {
        let marker = line.chars().next().filter(|c| matches!(c, '`' | '~'))?;
        let len = line.chars().take_while(|&c| c == marker).count();
        (len >= 3).then_some(Self { marker, len })
    }

    /// Whether `line` closes this fence: at least as many of its characters, and nothing after them.
    fn is_closed_by(self, line: &str) -> bool {
        let run = line.chars().take_while(|&c| c == self.marker).count();
        run >= self.len && line[run * self.marker.len_utf8()..].trim().is_empty()
    }
}

/// `import … from "…"`, `import "…"`, `import {…`, or `export` followed by a declaration, on one line, or the first
/// line of a multi-line one. Prose such as "export your data" is not a statement.
fn is_module_statement(line: &str) -> bool {
    if let Some(rest) = line.strip_prefix("import") {
        let tail = rest.trim_start();
        let separated = tail.len() < rest.len() || rest.starts_with('{');
        return separated && (tail.starts_with(['"', '\'', '{', '*']) || has_from_clause(tail));
    }
    line.strip_prefix("export ").is_some_and(|rest| {
        let word = rest.trim_start().split([' ', '{', '*', '(']).next().unwrap_or_default();
        rest.trim_start().starts_with(['{', '*'])
            || matches!(
                word,
                "default" | "const" | "let" | "var" | "function" | "async" | "class" | "type" | "interface" | "enum"
            )
    })
}

/// Whether an import's tail names its module: `… from "…"` or `… from '…'`.
fn has_from_clause(rest: &str) -> bool {
    rest.contains(" from \"") || rest.contains(" from '")
}

/// Braces still open after `line`, given `open` before it, not counting braces inside string literals. A module
/// statement continues while braces are open.
fn brace_balance(open: usize, line: &str) -> usize {
    let mut open = open;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in line.chars() {
        match quote {
            Some(_) if escaped => escaped = false,
            Some(_) if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' | '`' => quote = Some(c),
                '{' => open += 1,
                '}' => open = open.saturating_sub(1),
                _ => {}
            },
        }
    }
    open
}

/// A line that is exactly one self-closing JSX component, such as `<Callout type="info" />`; a line that also closes a
/// tag, such as `<Note>text</Note> <Br />`, carries text and is not one.
fn is_self_closing_component(line: &str) -> bool {
    line.strip_prefix('<').is_some_and(|rest| {
        rest.starts_with(|c: char| c.is_ascii_uppercase()) && line.ends_with("/>") && !line.contains("</")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(doc: &ParsedDoc) -> Vec<String> {
        doc.blocks
            .iter()
            .map(|block| match &block.kind {
                BlockKind::Heading { level, text, anchor } => format!("h{level}:{text}#{anchor}"),
                BlockKind::Prose => "prose".to_owned(),
                BlockKind::Code { lang, first_line } => {
                    format!("code:{}:{first_line}", lang.as_deref().unwrap_or("-"))
                }
                BlockKind::Other => "other".to_owned(),
            })
            .collect()
    }

    #[test]
    fn lists_top_level_blocks_with_their_ranges() {
        let text = "# Title\n\nSome *prose*.\n\n```rust\nfn main() {}\n```\n\n- a\n- b\n\n---\n\n<div>x</div>\n";
        let doc = parse("a.md", text);
        assert_eq!(
            kinds(&doc),
            [
                "h1:Title#title",
                "prose",
                "code:rust:fn main() {}",
                "prose",
                "other",
                "other"
            ]
        );
        assert_eq!(&doc.text[doc.blocks[0].range.clone()], "# Title\n");
        assert_eq!(&doc.text[doc.blocks[1].range.clone()], "Some *prose*.\n");
        assert_eq!(doc.title.as_deref(), Some("Title"));
    }

    #[test]
    fn code_blocks_record_language_and_first_line() {
        let doc = parse(
            "a.md",
            "```Python title=\"x\"\nimport os\nos.x()\n```\n\n    indented()\n\n```\n\n```\n",
        );
        assert_eq!(kinds(&doc), ["code:python:import os", "code:-:indented()", "code:-:"]);
    }

    #[test]
    fn headings_are_plain_text_with_unique_anchors() {
        let doc = parse(
            "a.md",
            "## The `spawn` *function*\n\n## Intro\n\n## Intro\n\n## Custom {#my-id}\n\nSetext\n======\n\n##### Deep\n",
        );
        assert_eq!(
            kinds(&doc),
            [
                "h2:The spawn function#the-spawn-function",
                "h2:Intro#intro",
                "h2:Intro#intro-1",
                "h2:Custom#my-id",
                "h1:Setext#setext",
                "h5:Deep#deep",
            ]
        );
        assert_eq!(doc.title.as_deref(), Some("Setext"));
    }

    #[test]
    fn inline_code_is_collected_with_offsets() {
        let doc = parse("a.md", "# Use `tokio::spawn`\n\nCall `spawn()`, then\n`join`.\n");
        let codes: Vec<(usize, &str)> = doc.inline_code.iter().map(|c| (c.offset, c.code.as_str())).collect();
        assert_eq!(codes, [(6, "tokio::spawn"), (27, "spawn()"), (43, "join")]);
        assert_eq!(&doc.text[27..36], "`spawn()`");
    }

    #[test]
    fn soft_breaks_in_headings_become_spaces() {
        let doc = parse("a.md", "Two\nlines\n---\n");
        assert_eq!(kinds(&doc), ["h2:Two lines#two-lines"]);
    }

    #[test]
    fn front_matter_is_stripped_and_its_title_read() {
        for (text, title) in [
            (
                "---\ntitle: Getting started\nslug: x\n---\n# Other\n",
                "Getting started",
            ),
            ("---\ntitle: \"Quoted: yes\"\n---\nBody\n", "Quoted: yes"),
            ("---\r\ntitle: 'single'\r\n...\r\nBody\n", "single"),
        ] {
            let doc = parse("a.md", text);
            assert_eq!(doc.title.as_deref(), Some(title), "{text}");
            assert!(!doc.text.contains("title:"), "{text}");
        }
        let doc = parse("a.md", "---\nslug: x\ntitle:\n---\nBody\n");
        assert_eq!((doc.title, doc.text.as_str()), (None, "Body\n"));
    }

    #[test]
    fn unclosed_front_matter_is_ordinary_text() {
        let text = "---\ntitle: x\nno end\n";
        let doc = parse("a.md", text);
        assert_eq!(doc.text, text);
        assert_eq!(doc.title, None);
        assert_eq!(parse("a.md", "no front matter").title, None);
    }

    #[test]
    fn mdx_statements_and_self_closing_components_are_removed() {
        let text = "import { A,\n  B } from './x'\nimport C from 'c'\nexport const meta = {}\n\n# Title\n\n<Callout type=\"info\" />\n\n<Tabs>\nKept\n</Tabs>\n\n```js\nimport x from 'y'\n<Foo />\n```\n\n~~~\nexport y\n~~~\nimport{z} from 'z'\nimport {\n  w,\n} from 'w'\nexport const m = {\n  a: 1,\n}\nimport './side.css'\nexport const deep = {\n  nested: {\n    a: 1,\n  },\n}\nend\n";
        let doc = parse("page.MDX", text);
        assert_eq!(
            doc.text,
            "\n# Title\n\n\n<Tabs>\nKept\n</Tabs>\n\n```js\nimport x from 'y'\n<Foo />\n```\n\n~~~\nexport y\n~~~\nend\n"
        );
    }

    #[test]
    fn a_longer_fence_closes_only_on_a_fence_at_least_as_long() {
        let text = "````mdx\n```tsx\nexport default function Page() {\n  return <Foo />\n}\n```\n````\n\n~~~~\n~~~\nimport x from 'y'\n~~~~\n";
        assert_eq!(parse("page.mdx", text).text, text);
    }

    #[test]
    fn a_fence_line_with_text_after_the_marker_does_not_close_it() {
        let text = "```md\n```js title\nimport x from 'y'\n```\n";
        let doc = parse("page.mdx", text);
        assert_eq!(doc.text, "```md\n```js title\nimport x from 'y'\n```\n");
    }

    #[test]
    fn prose_that_starts_with_import_or_export_is_kept() {
        let text = "export your data with the CLI:\n\nimport the file in the dashboard.\n";
        assert_eq!(parse("page.mdx", text).text, text);
    }

    #[test]
    fn braces_inside_strings_do_not_extend_a_statement() {
        let text = "export const open = \"{\";\nexport const close = '}{';\nKept\n";
        assert_eq!(parse("page.mdx", text).text, "Kept\n");
    }

    #[test]
    fn a_blank_line_ends_a_statement_whose_braces_never_balance() {
        let text = "export const re = /{/;\n\nKept\n";
        assert_eq!(parse("page.mdx", text).text, "\nKept\n");
    }

    #[test]
    fn a_line_with_text_and_a_closing_tag_is_not_a_self_closing_component() {
        let text = "<Note>Requires v2</Note> <Br />\n<Br />\n";
        assert_eq!(parse("page.mdx", text).text, "<Note>Requires v2</Note> <Br />\n");
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_front_matter() {
        let doc = parse("a.md", "\u{feff}---\ntitle: Bom\n---\nBody\n");
        assert_eq!((doc.title.as_deref(), doc.text.as_str()), (Some("Bom"), "Body\n"));
    }

    #[test]
    fn strikethrough_is_removed_from_heading_text() {
        assert_eq!(kinds(&parse("a.md", "## ~~Old~~ API\n")), ["h2:Old API#old-api"]);
    }

    #[test]
    fn rules_inside_containers_are_not_top_level_blocks() {
        assert_eq!(
            kinds(&parse("a.md", "> quote\n>\n> ---\n\n- a\n\n  ***\n")),
            ["prose", "prose"]
        );
    }

    #[test]
    fn plain_markdown_keeps_import_lines() {
        let text = "import x from 'y'\n<Foo />\n";
        assert_eq!(parse("a.md", text).text, text);
    }

    #[test]
    fn slugs_follow_github() {
        assert_eq!(slug("Hello, World!"), "hello-world");
        assert_eq!(slug("snake_case and-dash"), "snake_case-and-dash");
        assert_eq!(slug("Ünïcode Ö"), "ünïcode-ö");
    }

    #[test]
    fn every_heading_level_is_recognised() {
        let doc = parse("a.md", "# 1\n## 2\n### 3\n#### 4\n##### 5\n###### 6\n");
        let levels: Vec<String> = kinds(&doc).iter().map(|kind| kind[..2].to_owned()).collect();
        assert_eq!(levels, ["h1", "h2", "h3", "h4", "h5", "h6"]);
    }
}

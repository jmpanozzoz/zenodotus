//! Markdown parsing: front matter and MDX stripping, then the top-level blocks of the document with byte ranges.

use std::collections::BTreeMap;
use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag};
use zenodotus_core::fences::Fence;

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

/// Removes MDX `import`/`export` blocks and self-closing JSX component lines outside code fences. As in MDX, a block
/// starts at the beginning of a line with the keyword and whitespace, and ends at a blank line; while its braces are
/// open it runs on across a blank line into a line that is indented or starts with a closing bracket. An import block
/// that never names its module was prose, and is kept.
fn strip_mdx(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut fence: Option<Fence> = None;
    let mut block: Option<EsmBlock> = None;
    for line in text.split_inclusive('\n') {
        if let Some(open) = fence {
            if open.is_closed_by(line) {
                fence = None;
            }
            kept.push_str(line);
            continue;
        }
        if let Some(mut open) = block.take() {
            if line.trim().is_empty() && open.open_braces == 0 {
                open.finish(&mut kept);
                kept.push_str(line);
                continue;
            }
            if line.trim().is_empty() {
                open.blanks.push_str(line);
                block = Some(open);
                continue;
            }
            if open.blanks.is_empty() || line.starts_with(char::is_whitespace) || line.starts_with(['}', ')', ']']) {
                open.extend(line);
                block = Some(open);
                continue;
            }
            open.finish(&mut kept);
        }
        if let Some(open) = Fence::opened_by(line) {
            fence = Some(open);
            kept.push_str(line);
        } else if let Some(open) = EsmBlock::started_by(line) {
            block = Some(open);
        } else if !is_self_closing_component(line) {
            kept.push_str(line);
        }
    }
    if let Some(open) = block {
        open.finish(&mut kept);
    }
    kept
}

/// An MDX `import`/`export` block being read.
struct EsmBlock {
    /// The block's lines so far.
    lines: String,
    /// Blank lines read after them, which belong to the block only if a continuation line follows.
    blanks: String,
    /// Braces still open.
    open_braces: usize,
    /// Whether the block is an import that must name its module to be one.
    needs_module: bool,
}

impl EsmBlock {
    /// The block `line` starts, if any: `import` and whitespace followed by a module, a binding list or a default
    /// binding, or `export` and whitespace followed by a declaration. Prose such as "import the file" or "export your
    /// data" starts none.
    fn started_by(line: &str) -> Option<Self> {
        let trimmed = line.trim_end();
        let started = |needs_module| Self {
            lines: line.to_owned(),
            blanks: String::new(),
            open_braces: brace_balance(0, line),
            needs_module,
        };
        if let Some(rest) = trimmed.strip_prefix("import") {
            let tail = rest.trim_start();
            if tail.len() == rest.len() {
                return None;
            }
            if tail.starts_with(['"', '\'']) {
                return Some(started(false));
            }
            let bindings = tail.strip_prefix("type ").map_or(tail, str::trim_start);
            let after_name = bindings.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$');
            let default_binding = after_name.len() < bindings.len()
                && (after_name.trim().is_empty()
                    || after_name.trim_start().starts_with(',')
                    || names_module(after_name));
            return (bindings.starts_with(['{', '*']) || default_binding || names_module(tail)).then(|| started(true));
        }
        let rest = trimmed.strip_prefix("export")?;
        let tail = rest.trim_start();
        let word = tail.split([' ', '\t', '{', '*', '(']).next().unwrap_or_default();
        let declaration = tail.starts_with(['{', '*'])
            || matches!(
                word,
                "default" | "const" | "let" | "var" | "function" | "async" | "class" | "type" | "interface" | "enum"
            );
        (tail.len() < rest.len() && declaration).then(|| started(false))
    }

    /// Adds a continuation `line`, after any blank lines read before it.
    fn extend(&mut self, line: &str) {
        self.lines.push_str(&self.blanks);
        self.blanks.clear();
        self.lines.push_str(line);
        self.open_braces = brace_balance(self.open_braces, line);
    }

    /// Ends the block: its lines are dropped, unless it is an import that never named its module, and the blank lines
    /// after it are kept.
    fn finish(self, kept: &mut String) {
        if self.needs_module && !names_module(&self.lines) {
            kept.push_str(&self.lines);
        }
        kept.push_str(&self.blanks);
    }
}

/// Whether `text` names an import's module: `from` followed by a quoted string.
fn names_module(text: &str) -> bool {
    text.split("from")
        .skip(1)
        .any(|after| after.trim_start().starts_with(['"', '\'']))
}

/// Braces still open after `line`, given `open` before it.
fn brace_balance(open: usize, line: &str) -> usize {
    (open + line.matches('{').count()).saturating_sub(line.matches('}').count())
}

/// A line that holds only JSX tags, starts with a capitalised tag and ends with `/>`, such as
/// `<Callout type="info" />`; MDX has no indented code, so indentation does not matter. A line with text outside its
/// tags, such as `<Note>text</Note> <Br />`, is kept.
fn is_self_closing_component(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed
        .strip_prefix('<')
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_uppercase()))
        && trimmed.ends_with("/>")
        && !has_text_outside_tags(trimmed)
}

/// Whether `line` has anything but whitespace outside its `<…>` tags. Inside a tag, quoted values and `{…}`
/// expressions may hold `<` and `>`; a quote or brace left open makes the line count as text, so it is kept.
fn has_text_outside_tags(line: &str) -> bool {
    let mut in_tag = false;
    let mut quote: Option<char> = None;
    let mut braces = 0_usize;
    let text = line.chars().any(|c| {
        if let Some(open) = quote {
            quote = (c != open).then_some(open);
        } else if in_tag {
            match c {
                '"' | '\'' | '`' => quote = Some(c),
                '{' => braces += 1,
                '}' => braces = braces.saturating_sub(1),
                '>' => in_tag = braces > 0,
                _ => {}
            }
        } else {
            in_tag = c == '<';
            return !in_tag && !c.is_whitespace();
        }
        false
    });
    text || quote.is_some() || braces > 0
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
    fn mdx_blocks_and_self_closing_components_are_removed() {
        let text = "import { A,\n  B } from './x'\nimport C from 'c'\nexport const meta = {}\n\n# Title\n\n<Callout type=\"info\" />\n\n<Tabs>\nKept\n</Tabs>\n\n```js\nimport x from 'y'\n<Foo />\n```\n\n~~~\nexport y\n~~~\nimport{z} from 'z'\n\nimport {\n  w,\n} from 'w'\nexport const m = {\n  a: 1,\n}\nimport './side.css'\n\nexport const deep = {\n  nested: {\n    a: 1,\n  },\n}\n\nend\n";
        let doc = parse("page.MDX", text);
        assert_eq!(
            doc.text,
            "\n# Title\n\n\n<Tabs>\nKept\n</Tabs>\n\n```js\nimport x from 'y'\n<Foo />\n```\n\n~~~\nexport y\n~~~\nimport{z} from 'z'\n\n\n\nend\n"
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
        assert_eq!(parse("page.mdx", text).text, text);
    }

    #[test]
    fn prose_that_starts_with_import_or_export_is_kept() {
        for text in [
            "export your data with the CLI:\n\nimport the file in the dashboard.\n",
            "import everything\nThen run it.\n\nMore\n",
            "Then run it.\nimport Everything\n",
            "important: exports are hoisted\n",
            "exportconst value = 1\n",
            "import \nfrom 'x'\n",
            "import ,x\nfrom 'y'\n",
            "import everything\n\"Quoted\" is prose\nMore\n",
        ] {
            assert_eq!(parse("page.mdx", text).text, text);
        }
    }

    #[test]
    fn a_block_ends_at_a_blank_line_once_its_braces_balance() {
        let text = "export function Demo() {\n  const x = 1\n\n  return <div>{x}</div>\n}\n\nexport const Note = () => { return <p>Don't do it</p> }\n\nKept\n";
        assert_eq!(parse("page.mdx", text).text, "\n\nKept\n");
        let text = "export const a = { b: 1 }\n\n  An indented paragraph\n";
        assert_eq!(parse("page.mdx", text).text, "\n  An indented paragraph\n");
    }

    #[test]
    fn a_stray_brace_cannot_carry_a_block_past_a_paragraph() {
        let text = "export const open = \"{\";\n\n# Install\n\nUse `}` to close it.\n";
        assert_eq!(parse("page.mdx", text).text, "\n# Install\n\nUse `}` to close it.\n");
        let text = "export const meta = {\n  sep: \"}\",\n  title: \"x\"\n}\n\nText\n";
        assert_eq!(parse("page.mdx", text).text, "\nText\n");
    }

    #[test]
    fn a_block_runs_to_its_blank_line_whatever_its_lines_hold() {
        let text = "export default\nfunction Layout({ children }) {\n  return <div>{children}</div>\n}\n\nimport { a } from\n  'x'\n\nText\n";
        assert_eq!(parse("page.mdx", text).text, "\n\nText\n");
    }

    #[test]
    fn indented_or_inline_backticks_do_not_open_a_fence() {
        let text =
            "    ```\n    import x from 'y'\nexport const a = 1\n\n``` `inline` ```\nexport const b = 2\n\nText\n";
        assert_eq!(
            parse("page.mdx", text).text,
            "    ```\n    import x from 'y'\n\n``` `inline` ```\n\nText\n"
        );
    }

    #[test]
    fn export_takes_any_whitespace_and_imports_may_span_lines() {
        let text = "export\tconst a = 1\n\nimport Foo,\n  { bar } from 'x'\nimport Baz\n  from \"baz\"\n\nimport _\n  from 'lodash'\n\nimport $t\n  from 'i18n'\n\nimport Qux\nfrom 'qux'\n\nimport \"./polyfill\"\n\nText\n";
        assert_eq!(parse("page.mdx", text).text, "\n\n\n\n\n\nText\n");
    }

    #[test]
    fn a_component_line_is_removed_only_when_it_carries_no_text() {
        let text = "<Note>Requires v2</Note> <Br />\n<Br /> <Br />\n<Code snippet=\"</div>\" />\n    <Br />\n\t<Br />\n<Button onClick={() => alert('hi')} />\n<Chart data={a > b} label={`>`} />\n<Br data={x} /> text <Br />\n<Badge value={/* don't */ 1} /> Requires v2 <Br />\n<Foo a={x /> Important note <Br />\nText\n";
        assert_eq!(
            parse("page.mdx", text).text,
            "<Note>Requires v2</Note> <Br />\n<Br data={x} /> text <Br />\n<Badge value={/* don't */ 1} /> Requires v2 <Br />\n<Foo a={x /> Important note <Br />\nText\n"
        );
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

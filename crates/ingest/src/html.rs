//! From an HTML page to Markdown and its links. The main content is the page's documentation root or landmark, or
//! what a port of Mozilla's Readability finds when it has neither, and the page title becomes a front-matter `title`.

use dom_query::{Document, Selection};
use dom_smoothie::{Config, Readability, TextMode};
use url::Url;

/// An HTML page as Markdown, with the links it contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The main content as Markdown, preceded by front matter with the page title when it has one.
    pub markdown: String,
    /// Every `http` or `https` link of the page, resolved against its URL, without fragments, in order and without
    /// repeats.
    pub links: Vec<Url>,
}

/// Elements that hold a page's main content, most specific first: documentation-framework roots (`Docusaurus`,
/// `MkDocs`, `VitePress`, `Sphinx`), then the HTML landmarks.
const CONTENT_ROOTS: [&str; 7] = [
    ".theme-doc-markdown",
    ".md-content",
    ".vp-doc",
    "div.body",
    "article",
    "[role=main]",
    "main",
];

/// Page furniture removed from the content root before conversion.
const CHROME: &str = "nav, footer, aside, script, style, .toc, .sidebar";

/// Converts `html`, fetched from `url`. The content root is the first of [`CONTENT_ROOTS`] the page has, without
/// [`CHROME`]; a page with none goes through Readability, which fails only on pages without any text, which become
/// empty.
#[must_use]
pub fn html_to_markdown(html: &str, url: &Url) -> Page {
    let document = Document::from(html);
    let links = links(&document, url);
    let title = one_line(&document.select("head title").text());
    let content = match root_markdown(&document) {
        Some(content) => content,
        None => readability_markdown(document, url),
    };
    let content = content.trim();
    let markdown = if title.is_empty() {
        format!("{content}\n")
    } else {
        format!("---\ntitle: {title}\n---\n\n{content}\n")
    };
    Page { markdown, links }
}

/// The Markdown of the page's content root without its chrome, or `None` when the page has no content root.
fn root_markdown(document: &Document) -> Option<String> {
    let root = CONTENT_ROOTS
        .iter()
        .map(|selector| document.select(selector).first())
        .find(Selection::exists)?;
    root.select(CHROME).remove();
    root.nodes().first().map(|node| node.md(None).to_string())
}

/// The main content Readability finds in `document`, as Markdown; empty when it finds none.
fn readability_markdown(document: Document, url: &Url) -> String {
    let config = Config {
        text_mode: TextMode::Markdown,
        keep_classes: true,
        ..Config::default()
    };
    Readability::with_document(document, Some(url.as_str()), Some(config))
        .and_then(|mut readability| readability.parse())
        .map(|article| article.text_content.to_string())
        .unwrap_or_default()
}

/// `text` with runs of whitespace, line breaks included, collapsed into single spaces.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The links of `document`, as [`Page::links`] describes them.
fn links(document: &Document, base: &Url) -> Vec<Url> {
    let mut found: Vec<Url> = Vec::new();
    for anchor in &document.select("a[href]") {
        let href = anchor.attr("href").unwrap_or_default();
        let Ok(mut link) = base.join(href.trim()) else {
            continue;
        };
        if !matches!(link.scheme(), "http" | "https") {
            continue;
        }
        link.set_fragment(None);
        if !found.contains(&link) {
            found.push(link);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    const ARTICLE: &str = r#"<!doctype html>
<html><head><title>Spawning tasks |
 Tokio</title></head>
<body>
<nav><a href="/docs/">Docs</a> <a href="/blog/">Blog</a></nav>
<main><article>
<h1>Spawning</h1>
<p>Tokio tasks are asynchronous green threads. They are created by passing an async block to
<code>tokio::spawn</code>. The spawn function returns a JoinHandle, which the caller may use to interact with the
spawned task. The async block may have a return value, which the caller may obtain using .await on the
JoinHandle.</p>
<h2>Example</h2>
<pre><code class="language-rust">let handle = tokio::spawn(async { 10 });
let out = handle.await.unwrap();</code></pre>
<p>Awaiting a JoinHandle returns a Result. When a task encounters an error during execution, the JoinHandle will
return an Err. This happens when the task either panics, or if the task is forcefully cancelled by the runtime
shutting down. See <a href="../select/#cancel">select</a> and <a href="mailto:x@y.z">mail</a> and
<a href="https://docs.rs/tokio">docs.rs</a> and <a href="/docs/">again</a> and <a href="http://[::1">broken</a>.</p>
</article></main>
<footer>Copyright</footer>
</body></html>"#;

    #[test]
    fn the_main_content_becomes_markdown_under_the_page_title() {
        let page = html_to_markdown(ARTICLE, &url("https://tokio.rs/tokio/tutorial/spawning"));
        assert!(
            page.markdown.starts_with("---\ntitle: Spawning tasks | Tokio\n---\n\n"),
            "{}",
            page.markdown
        );
        assert!(
            page.markdown
                .contains("```rust\nlet handle = tokio::spawn(async { 10 });"),
            "{}",
            page.markdown
        );
        assert!(page.markdown.contains("## Example"), "{}", page.markdown);
        assert!(!page.markdown.contains("Copyright"), "{}", page.markdown);
        assert!(page.markdown.ends_with('\n'));
        let links: Vec<&str> = page.links.iter().map(Url::as_str).collect();
        assert_eq!(
            links,
            [
                "https://tokio.rs/docs/",
                "https://tokio.rs/blog/",
                "https://tokio.rs/tokio/select/",
                "https://docs.rs/tokio",
            ]
        );
    }

    #[test]
    fn pages_without_text_keep_only_their_title() {
        let page = html_to_markdown(
            "<html><head><title>Frames</title></head><frameset><frame src=\"/a\"></frameset></html>",
            &url("https://docs.example/"),
        );
        assert_eq!(page.markdown, "---\ntitle: Frames\n---\n\n\n");
    }

    #[test]
    fn short_pages_without_a_title_are_kept_whole() {
        let page = html_to_markdown(
            "<html><body><p>Short <b>note</b>.</p></body></html>",
            &url("https://docs.example/a"),
        );
        assert_eq!(page.markdown, "Short **note**.\n");
        assert_eq!(page.links, []);
    }

    #[test]
    fn a_page_without_a_content_root_keeps_its_code_block_languages() {
        let html = r#"<html><head><title>Install</title></head><body><div id="content">
<p>Install the package with pip, then import it in your application module before the first request.</p>
<pre><code class="language-python">import fastapi
app = fastapi.FastAPI()</code></pre>
</div></body></html>"#;
        let page = html_to_markdown(html, &url("https://docs.example/install"));
        assert!(
            page.markdown
                .contains("```python\nimport fastapi\napp = fastapi.FastAPI()\n```"),
            "{}",
            page.markdown
        );
    }

    #[test]
    fn a_documentation_root_is_kept_without_its_navigation() {
        let html = r#"<html><head><title>Hooks</title></head><body>
<nav class="navbar"><a href="/docs/">Docs</a></nav>
<div class="sidebar">Sidebar entry</div>
<main><div class="theme-doc-markdown">
<header><h1>Hooks</h1></header><div class="toc">On this page</div>
<p>Call <code>useState</code> at the top level.</p>
<pre><code class="language-js">const [a, setA] = useState(0);</code></pre>
<aside>Related posts</aside><script>track()</script>
</div><footer>Edit this page</footer></main>
</body></html>"#;
        let page = html_to_markdown(html, &url("https://react.example/hooks"));
        let markdown = &page.markdown;
        assert!(markdown.starts_with("---\ntitle: Hooks\n---\n\n# Hooks"), "{markdown}");
        assert!(markdown.contains("Call `useState` at the top level."), "{markdown}");
        assert!(
            markdown.contains("```js\nconst [a, setA] = useState(0);\n```"),
            "{markdown}"
        );
        for chrome in [
            "Docs",
            "Sidebar entry",
            "On this page",
            "Related posts",
            "track()",
            "Edit this page",
        ] {
            assert!(!markdown.contains(chrome), "{chrome} in {markdown}");
        }
    }

    #[test]
    fn the_most_specific_root_wins_and_short_main_content_is_kept() {
        let roots = [
            (
                "<main><p>Outer</p><article><p>Inner article.</p></article></main>",
                "Inner article.",
            ),
            (r#"<div role="main"><p>Role main.</p></div>"#, "Role main."),
            ("<main><h2>Tiny</h2><p>One line.</p></main>", "## Tiny\n\nOne line."),
            (
                r#"<div class="md-content"><p>MkDocs.</p></div><main><p>Main.</p></main>"#,
                "MkDocs.",
            ),
            (r#"<div class="vp-doc"><p>VitePress.</p></div>"#, "VitePress."),
            (r#"<div class="body"><p>Sphinx.</p></div>"#, "Sphinx."),
        ];
        for (body, expected) in roots {
            let page = html_to_markdown(&format!("<html><body>{body}</body></html>"), &url("https://x.example/"));
            assert_eq!(page.markdown, format!("{expected}\n"), "{body}");
        }
    }
}

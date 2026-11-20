//! From an HTML page to Markdown and its links. The main content is found by a port of Mozilla's Readability, and the
//! page title becomes a front-matter `title`.

use dom_query::Document;
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

/// Converts `html`, fetched from `url`. Readability fails only on pages without any text, which become empty.
#[must_use]
pub fn html_to_markdown(html: &str, url: &Url) -> Page {
    let document = Document::from(html);
    let links = links(&document, url);
    let title = one_line(&document.select("head title").text());
    let config = Config {
        text_mode: TextMode::Markdown,
        keep_classes: true,
        ..Config::default()
    };
    let content = Readability::with_document(document, Some(url.as_str()), Some(config))
        .and_then(|mut readability| readability.parse())
        .map(|article| article.text_content.to_string())
        .unwrap_or_default();
    let content = content.trim();
    let markdown = if title.is_empty() {
        format!("{content}\n")
    } else {
        format!("---\ntitle: {title}\n---\n\n{content}\n")
    };
    Page { markdown, links }
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
}

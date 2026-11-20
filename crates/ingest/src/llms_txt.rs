//! The `llms_txt` source: an `llms.txt` file and the pages it links to on its own host.

use std::collections::BTreeSet;

use pulldown_cmark::{Event, Parser, Tag};
use url::Url;
use zenodotus_core::model::VersionChanges;

use crate::error::IngestError;
use crate::fetch::HostSet;
use crate::source::{Context, Visit, Visiting};

/// The `http` or `https` links of a Markdown `text` on the host of `base`, resolved against it, without fragments,
/// in order and without repeats.
#[must_use]
pub fn parse_links(text: &str, base: &Url) -> Vec<Url> {
    let mut links: Vec<Url> = Vec::new();
    for event in Parser::new(text) {
        let Event::Start(Tag::Link { dest_url, .. }) = event else {
            continue;
        };
        let Ok(mut link) = base.join(&dest_url) else {
            continue;
        };
        link.set_fragment(None);
        let same_host =
            link.host_str() == base.host_str() && link.port_or_known_default() == base.port_or_known_default();
        if same_host && matches!(link.scheme(), "http" | "https") && !links.contains(&link) {
            links.push(link);
        }
    }
    links
}

/// Fetches the `llms.txt` file at `url`, indexes it, and fetches up to `fetch.llms_txt_max_links` of its links. A
/// page is deleted when it answers 404 or 410 or is no longer linked.
///
/// # Errors
/// [`IngestError::ListingUnavailable`] when the file itself cannot be fetched.
pub async fn collect_llms_txt(ctx: &Context<'_>, url: &Url) -> Result<VersionChanges, IngestError> {
    let hosts = HostSet::of(&[url], ctx.extra_hosts);
    let mut changes = VersionChanges::default();
    let Some(file_key) = ctx.doc_key(url, &mut changes) else {
        return Err(IngestError::listing(url, &changes.errors[0].message));
    };
    let file = Visiting {
        url,
        doc_key: &file_key,
        hosts: &hosts,
        max_bytes: ctx.settings.document_max_bytes,
        conditional: false,
        report_missing: true,
        follows_moves: false,
    };
    let Visit::Fetched { markdown, .. } = ctx.visit(&file, &mut changes).await else {
        let reason = changes
            .errors
            .pop()
            .map_or_else(|| "no body (HTTP 304, 404 or 410)".to_owned(), |error| error.message);
        return Err(IngestError::listing(url, &reason));
    };
    let max_links = usize::try_from(ctx.settings.llms_txt_max_links).unwrap_or(usize::MAX);
    let mut listed = BTreeSet::from([file_key.clone()]);
    for link in parse_links(&markdown, url)
        .iter()
        .filter(|link| *link != url)
        .take(max_links)
    {
        let Some(doc_key) = ctx.doc_key(link, &mut changes) else {
            continue;
        };
        listed.insert(doc_key.clone());
        let page = Visiting {
            url: link,
            doc_key: &doc_key,
            max_bytes: ctx.settings.page_max_bytes,
            conditional: true,
            ..file
        };
        ctx.visit(&page, &mut changes).await;
    }
    changes
        .deletions
        .extend(ctx.previous.keys().filter(|key| !listed.contains(*key)).cloned());
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zenodotus_core::config::FetchSettings;

    use super::*;
    use crate::fetch::tests::{local_settings, url};
    use crate::source::tests::{Harness, errors, paths};

    #[test]
    fn only_links_on_the_same_host_are_kept() {
        let base = url("https://docs.example/llms.txt");
        let text = "# Docs\n\n- [A](/a.md): first\n- [A again](https://docs.example/a.md#top)\n\
                    - [B](b.md)\n- [Other](https://other.example/c.md)\n- [Port](https://docs.example:8443/d.md)\n\
                    - [Mail](mailto:x@docs.example)\n- [Bad](http://[::1)\n";
        let links: Vec<String> = parse_links(text, &base).iter().map(ToString::to_string).collect();
        assert_eq!(links, ["https://docs.example/a.md", "https://docs.example/b.md"]);
    }

    async fn serve(server: &MockServer, route: &str, status: u16, body: &str) {
        Mock::given(path(route))
            .respond_with(ResponseTemplate::new(status).set_body_raw(body, "text/markdown"))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn the_file_and_its_links_are_indexed_and_unlisted_pages_deleted() {
        let server = MockServer::start().await;
        let uri = server.uri();
        let index = format!(
            "# Lib\n\n- [A](/a.md)\n- [B]({uri}/b.md)\n- [Self]({uri}/llms.txt)\n- [Long](/{})\n- [C](/c.md)\n",
            "x".repeat(1100)
        );
        serve(&server, "/llms.txt", 200, &index).await;
        serve(&server, "/a.md", 200, "# A\n").await;
        serve(&server, "/b.md", 410, "").await;
        let at = |p: &str| url(&format!("{uri}{p}"));
        let mut harness = Harness::with_settings(FetchSettings {
            llms_txt_max_links: 3,
            ..local_settings()
        });
        harness.remember(at("/b.md").as_str(), "old", None);
        harness.remember(at("/unlisted.md").as_str(), "old", None);
        let changes = collect_llms_txt(&harness.context(), &at("/llms.txt")).await.unwrap();
        let upserted: Vec<&str> = changes.upserts.iter().map(|doc| doc.state.doc_key.doc_path()).collect();
        assert_eq!(upserted, [at("/llms.txt").as_str(), at("/a.md").as_str()]);
        assert_eq!(
            paths(&changes.deletions),
            [at("/b.md").as_str(), at("/unlisted.md").as_str()]
        );
        assert_eq!(errors(&changes).len(), 1, "{:?}", errors(&changes));
        assert!(errors(&changes)[0].ends_with("(invalid_path)"));
    }

    #[tokio::test]
    async fn linked_pages_are_conditional_and_capped_at_the_page_size() {
        let server = MockServer::start().await;
        let uri = server.uri();
        serve(&server, "/llms.txt", 200, "# Lib\n\n- [A](/a.md)\n- [Big](/big.md)\n").await;
        Mock::given(path("/a.md"))
            .and(wiremock::matchers::header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        serve(&server, "/a.md", 200, "# A changed\n").await;
        serve(&server, "/big.md", 200, &"x".repeat(64)).await;
        let at = |p: &str| url(&format!("{uri}{p}"));
        let mut harness = Harness::with_settings(FetchSettings {
            page_max_bytes: 32,
            ..local_settings()
        });
        harness.remember(at("/a.md").as_str(), "# A\n", Some("\"v1\""));
        let changes = collect_llms_txt(&harness.context(), &at("/llms.txt")).await.unwrap();
        assert_eq!(paths(&changes.unchanged), [at("/a.md").as_str()]);
        let upserted: Vec<&str> = changes.upserts.iter().map(|doc| doc.state.doc_key.doc_path()).collect();
        assert_eq!(upserted, [at("/llms.txt").as_str()]);
        assert_eq!(errors(&changes), [format!("{} (too_large)", at("/big.md"))]);
    }

    #[tokio::test]
    async fn a_missing_or_failing_file_makes_the_listing_unavailable() {
        let server = MockServer::start().await;
        serve(&server, "/gone.txt", 404, "").await;
        serve(&server, "/broken.txt", 500, "").await;
        let uri = server.uri();
        let mut harness = Harness::new();
        for (route, reason) in [
            ("/gone.txt", format!("{uri}/gone.txt: not found (HTTP 404 or 410)")),
            ("/broken.txt", format!("{uri}/broken.txt: HTTP status 500")),
        ] {
            let file = url(&format!("{}{route}", server.uri()));
            let error = collect_llms_txt(&harness.context(), &file).await.unwrap_err();
            assert!(
                matches!(&error, IngestError::ListingUnavailable { what, reason: why } if *what == file.as_str() && *why == reason),
                "{error:?}"
            );
        }
        harness.remember(&format!("{uri}/gone.txt"), "old", None);
        let error = collect_llms_txt(&harness.context(), &url(&format!("{uri}/gone.txt")))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("cannot list {uri}/gone.txt: no body (HTTP 304, 404 or 410)")
        );
        let long = url(&format!("https://docs.example/{}", "x".repeat(1100)));
        let error = collect_llms_txt(&harness.context(), &long).await.unwrap_err();
        assert!(matches!(error, IngestError::ListingUnavailable { .. }), "{error:?}");
    }
}

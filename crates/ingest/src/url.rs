//! The `url` source: Markdown documents at fixed URLs, such as `llms-full.txt`.

use std::collections::BTreeSet;

use url::Url;
use zenodotus_core::model::VersionChanges;

use crate::fetch::HostSet;
use crate::source::{Context, Visiting};

/// Fetches every configured URL. A document is deleted when its URL answers 404 or 410, or is no longer configured;
/// any other failure keeps its previous content. The listing is the config itself, so it is always complete.
pub async fn collect_urls(ctx: &Context<'_>, urls: &[Url]) -> VersionChanges {
    let own: Vec<&Url> = urls.iter().collect();
    let hosts = HostSet::of(&own, ctx.extra_hosts);
    let mut changes = VersionChanges::default();
    let mut listed = BTreeSet::new();
    for url in urls {
        let Some(doc_key) = ctx.doc_key(url, &mut changes) else {
            continue;
        };
        if !listed.insert(doc_key.clone()) {
            continue;
        }
        let visiting = Visiting {
            url,
            doc_key: &doc_key,
            hosts: &hosts,
            max_bytes: ctx.settings.document_max_bytes,
            conditional: true,
            report_missing: true,
            moves: None,
        };
        ctx.visit(&visiting, &mut changes).await;
    }
    changes
        .deletions
        .extend(ctx.previous.keys().filter(|key| !listed.contains(*key)).cloned());
    changes
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::fetch::tests::url;
    use crate::source::tests::{Harness, errors, paths};

    #[tokio::test]
    async fn configured_urls_are_fetched_and_dropped_ones_deleted() {
        let server = MockServer::start().await;
        Mock::given(path("/llms-full.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("# Full\n\nText\n", "text/plain"))
            .mount(&server)
            .await;
        Mock::given(path("/removed.md"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let full = url(&format!("{}/llms-full.txt", server.uri()));
        let removed = url(&format!("{}/removed.md", server.uri()));
        let long = url(&format!("{}/{}", server.uri(), "a".repeat(1100)));
        let mut harness = Harness::new();
        harness.remember(removed.as_str(), "old", None);
        harness.remember("https://old.example/dropped.md", "old", None);
        let urls = [full.clone(), full.clone(), removed.clone(), long];
        let changes = collect_urls(&harness.context(), &urls).await;
        assert_eq!(changes.upserts.len(), 1);
        assert_eq!(changes.upserts[0].state.doc_key.doc_path(), full.as_str());
        assert_eq!(
            paths(&changes.deletions),
            [removed.as_str(), "https://old.example/dropped.md"]
        );
        assert_eq!(errors(&changes).len(), 1);
        assert!(errors(&changes)[0].ends_with("(invalid_path)"));
    }

    #[tokio::test]
    async fn a_configured_url_that_redirects_to_another_host_is_an_error_and_keeps_its_copy() {
        let server = MockServer::start().await;
        Mock::given(path("/doc.md"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "https://elsewhere.example/doc.md"))
            .mount(&server)
            .await;
        let doc = url(&format!("{}/doc.md", server.uri()));
        let mut harness = Harness::new();
        harness.remember(doc.as_str(), "old", None);
        let changes = collect_urls(&harness.context(), std::slice::from_ref(&doc)).await;
        assert_eq!(paths(&changes.deletions), [] as [&str; 0]);
        assert_eq!(errors(&changes), [format!("{doc} (host_not_allowed)")]);
    }

    #[tokio::test]
    async fn a_configured_url_that_redirects_stores_no_validators_of_its_target() {
        let server = MockServer::start().await;
        Mock::given(path("/doc.md"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/target.md"))
            .mount(&server)
            .await;
        for page in ["/target.md", "/direct.md"] {
            Mock::given(path(page))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw("# Doc\n\nText\n", "text/plain")
                        .insert_header("etag", "\"target\"")
                        .insert_header("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
                )
                .mount(&server)
                .await;
        }
        let doc = url(&format!("{}/doc.md", server.uri()));
        let direct = url(&format!("{}/direct.md", server.uri()));
        let harness = Harness::new();
        let changes = collect_urls(&harness.context(), &[doc.clone(), direct.clone()]).await;
        let validators = |key: &Url| {
            let state = &changes
                .upserts
                .iter()
                .find(|document| document.state.doc_key.doc_path() == key.as_str())
                .unwrap()
                .state;
            (state.etag.clone(), state.last_modified.clone())
        };
        assert_eq!(validators(&doc), (None, None));
        assert_eq!(
            validators(&direct),
            (
                Some("\"target\"".to_owned()),
                Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned())
            )
        );
    }
}

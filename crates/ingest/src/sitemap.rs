//! Sitemaps: `<urlset>` files list pages, `<sitemapindex>` files list more sitemaps. Gzip-compressed files are
//! recognised by their magic bytes and decompressed up to a size limit.

use std::io::Read;

use flate2::read::GzDecoder;
use quick_xml::Reader;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;

/// The two magic bytes of a gzip stream.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// What one sitemap file lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SitemapEntries {
    /// Page URLs, from `<url><loc>`.
    pub pages: Vec<String>,
    /// Sitemap URLs, from `<sitemap><loc>`.
    pub sitemaps: Vec<String>,
}

/// Which list the `<loc>` being read belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parent {
    Url,
    Sitemap,
}

/// Parses one sitemap file of at most `max_bytes` once decompressed.
///
/// # Errors
/// A message when the file is too large once decompressed, is not valid gzip or XML, or has neither a `<urlset>` nor
/// a `<sitemapindex>` root.
pub fn parse_sitemap(bytes: &[u8], max_bytes: u64) -> Result<SitemapEntries, String> {
    if bytes.starts_with(&GZIP_MAGIC) {
        let mut decoded = Vec::new();
        GzDecoder::new(bytes)
            .take(max_bytes + 1)
            .read_to_end(&mut decoded)
            .map_err(|error| format!("invalid gzip: {error}"))?;
        if decoded.len() as u64 > max_bytes {
            return Err(format!("larger than {max_bytes} bytes once decompressed"));
        }
        return parse_xml(&decoded);
    }
    parse_xml(bytes)
}

fn parse_xml(bytes: &[u8]) -> Result<SitemapEntries, String> {
    let mut reader = Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    let mut entries = SitemapEntries::default();
    let mut has_root = false;
    let mut parent = None;
    let mut loc: Option<String> = None;
    loop {
        let event = reader
            .read_event_into(&mut buffer)
            .map_err(|error| format!("invalid XML: {error}"))?;
        match event {
            Event::Start(element) => match element.local_name().as_ref() {
                "urlset" | "sitemapindex" => has_root = true,
                "url" => parent = Some(Parent::Url),
                "sitemap" => parent = Some(Parent::Sitemap),
                "loc" => loc = Some(String::new()),
                _ => {}
            },
            Event::Text(text) => {
                if let Some(loc) = &mut loc {
                    loc.push_str(&text.into_inner());
                }
            }
            Event::GeneralRef(reference) => {
                if let Some(loc) = &mut loc {
                    loc.push_str(&resolve_reference(&reference.into_inner()));
                }
            }
            Event::End(element) => match element.local_name().as_ref() {
                "loc" => {
                    if let (Some(text), Some(parent)) = (loc.take(), parent) {
                        let list = match parent {
                            Parent::Url => &mut entries.pages,
                            Parent::Sitemap => &mut entries.sitemaps,
                        };
                        list.push(text.trim().to_owned());
                    }
                }
                "url" | "sitemap" => parent = None,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if !has_root {
        return Err("not a sitemap: no <urlset> or <sitemapindex> element".to_owned());
    }
    Ok(entries)
}

/// The text of an entity or character reference such as `amp` or `#x26`; unknown ones are kept as written.
fn resolve_reference(name: &str) -> String {
    let character = name.strip_prefix("#x").map_or_else(
        || name.strip_prefix('#').and_then(|decimal| decimal.parse().ok()),
        |hex| u32::from_str_radix(hex, 16).ok(),
    );
    character.and_then(char::from_u32).map_or_else(
        || resolve_predefined_entity(name).map_or_else(|| format!("&{name};"), str::to_owned),
        String::from,
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    const URLSET: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc> https://docs.example/a?x=1&amp;y=2 </loc><lastmod>2026-01-01&amp;</lastmod></url>
  <url><loc>https://docs.example/&#x62;&#99;&unknown;</loc></url>
  <loc>https://docs.example/stray</loc>
</urlset>"#;

    #[test]
    fn page_urls_are_read_with_their_entities_resolved() {
        assert_eq!(
            parse_sitemap(URLSET.as_bytes(), 10_000).unwrap(),
            SitemapEntries {
                pages: vec![
                    "https://docs.example/a?x=1&y=2".to_owned(),
                    "https://docs.example/bc&unknown;".to_owned(),
                ],
                sitemaps: Vec::new(),
            }
        );
    }

    #[test]
    fn an_index_lists_sitemaps() {
        let index = r#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
            <sitemap><loc>https://docs.example/s1.xml</loc></sitemap>
            <sitemap><loc>https://docs.example/s2.xml.gz</loc></sitemap>
        </sitemapindex>"#;
        let entries = parse_sitemap(index.as_bytes(), 10_000).unwrap();
        assert_eq!(
            entries.sitemaps,
            ["https://docs.example/s1.xml", "https://docs.example/s2.xml.gz"]
        );
        assert_eq!(entries.pages, [] as [String; 0]);
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn gzip_files_are_decompressed_within_the_limit() {
        let compressed = gzip(URLSET.as_bytes());
        assert_eq!(parse_sitemap(&compressed, 10_000).unwrap().pages.len(), 2);
        assert_eq!(parse_sitemap(&compressed, URLSET.len() as u64).unwrap().pages.len(), 2);
        let limit = URLSET.len() as u64 - 1;
        assert_eq!(
            parse_sitemap(&compressed, limit).unwrap_err(),
            format!("larger than {limit} bytes once decompressed")
        );
        let error = parse_sitemap(&[0x1f, 0x8b, 0, 0], 10_000).unwrap_err();
        assert!(error.starts_with("invalid gzip: "), "{error}");
    }

    #[test]
    fn files_that_are_not_sitemaps_are_refused() {
        assert_eq!(
            parse_sitemap(b"<html><body>Not found</body></html>", 10_000).unwrap_err(),
            "not a sitemap: no <urlset> or <sitemapindex> element"
        );
        let error = parse_sitemap(b"<urlset><url></loc></urlset>", 10_000).unwrap_err();
        assert!(error.starts_with("invalid XML: "), "{error}");
    }

    #[test]
    fn references_resolve_or_stay_as_written() {
        assert_eq!(resolve_reference("amp"), "&");
        assert_eq!(resolve_reference("#38"), "&");
        assert_eq!(resolve_reference("#x26"), "&");
        assert_eq!(resolve_reference("#xzz"), "&#xzz;");
        assert_eq!(resolve_reference("#55296"), "&#55296;");
        assert_eq!(resolve_reference("nbsp"), "&nbsp;");
    }
}

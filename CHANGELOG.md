# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `zenodotus` CLI: `init`, `add`, `list`, `sync`, `search`, `serve` and `status`.
- Sources: local directories, GitHub repositories at a branch, tag or commit, Markdown URLs, `llms.txt` files and
  documentation websites.
- Incremental sync: unchanged GitHub commits skip the download, ETag and Last-Modified are honoured, unchanged
  documents are not reindexed, deleted documents are removed, and a failed sync leaves the previous index usable.
- Lexical ranking on Tantivy with term-frequency-injected field weights, code-aware analyzers and exact symbol
  matching; deterministic, byte-identical responses.
- MCP tools `resolve_library` and `query_docs` over stdio and Streamable HTTP, with versions that never fall back.
- Secure fetching: address guard against SSRF and DNS rebinding, checked redirects, size and decompression caps,
  robots.txt and per-host spacing.
- `zenodotus-eval`: a question set of 229 judged questions, a bootstrap-based report and a comparison against
  Context7, Grounded Docs and Neuledge Context.
- Criterion benchmarks and `cargo xtask perf` on MDN's English pages.
- A multi-stage Dockerfile and a GitHub Actions workflow.

### Fixed

- Request headers that carry credentials are no longer sent to another origin after a redirect.
- A version whose changes cannot be stored is recorded as `failed`, and the sync goes on with the next version.
- Fetched titles, headings and source references are rendered inside the data markers, not before them.
- HTML pages keep their documentation root or landmark without navigation, as designed; Readability is the fallback.
- Sitemaps: CDATA locations are read, nested `<image:loc>` and empty locations are ignored, and a sitemap that is not
  configured no longer fails a website sync.
- A GitHub source whose `paths` changed is downloaded again even when the commit did not change.
- Tarball files that are too large or not UTF-8 keep their previous content instead of being deleted.
- New ETag and Last-Modified values are stored even when the content did not change.
- Readers no longer repair the index directory, which could race a rebuild; a leftover `index.old/` no longer
  blocks the next rebuild.
- Synonym keys must be single lowercase words, the only form a query can match.
- The default `max_pages` of a website source is 2,000.
- A stored website page linked only from a page that answers 304 is no longer deleted when a sitemap omits it;
  every crawl revisits the stored pages, and only a 404, a 410 or leaving the crawl scope deletes a page.

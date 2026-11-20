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

### Changed

- A GitHub `reference` that is a full commit id is used without asking the API, so pinned sources sync
  under the API's unauthenticated rate limit.
- The default `symbol_weight` is 4 instead of 5 (improvement-loop iteration 12, MRR@10 +0.8% on the dev set). An
  index built with the old default asks for `sync --rebuild`.

### Fixed

- Request headers that carry credentials are no longer sent to another origin after a redirect.
- A website whose start page lies outside its `allow_prefixes` keeps the start page; before, one sync indexed it and
  the next deleted it.
- A link found by a crawl that redirects temporarily to a 404 is ignored, as a direct 404 is, instead of making
  every sync `partial`.
- A version whose changes cannot be stored is recorded as `failed`, and the sync goes on with the next version.
- A commit that fails, for example on a full disk, is rolled back, so the next commit no longer carries its staged
  deletions; before, recording the version as `failed` could delete pages it meant to keep.
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
- A crawled page that redirects is indexed under the URL it redirects to, in the same visit, and its copy under
  the old URL is deleted, instead of indexing the target's content twice. The old copy is kept when the target's
  own fetch failed earlier in the crawl, and a redirect counts once toward `max_pages`.
- Only a permanent redirect (301 or 308) deletes the copy under the old URL; a temporary one (302, 303 or 307),
  which may be maintenance, keeps it, also when it leads to a 404 or 410. The source URL of a redirected page no
  longer keeps the `Location` fragment.
- A crawled page that redirects to a host the source may not reach leaves the crawl scope instead of recording an
  error, so the version is no longer `partial` on every sync.
- Stored validators are sent only to the requested URL, so a redirect target cannot answer 304 for a document it
  never served.
- A start page that redirects outside the crawl scope fails the version instead of deleting the stored pages.
- A sitemap listed in robots.txt that cannot be read no longer stops the next listed sitemaps from being read.
- Stored pages are crawled before the start page's links and the sitemap pages, so `max_pages` cuts new pages
  rather than leaving stored ones stale.
- HTML `header` elements inside the content root are kept, so a Docusaurus page keeps its title.
- Synonym keys may be words that queries keep whole, such as `node.js` or `c++`.

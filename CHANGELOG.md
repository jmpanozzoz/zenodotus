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
- BM25 counts only section documents: the per-document and per-version state no longer enters the document count
  and the average field length (improvement-loop iteration 14, MRR@10 +0.3% on the dev set). No rebuild needed.
- Searches no longer filter on the document kind: the required word group already matches only sections
  (improvement-loop iteration 17, identical results, p50 −17% and p95 −20% in `cargo xtask perf`). No rebuild needed.
- A non-stop query word found in more than a fifth of the sections no longer decides which sections are candidates:
  it only adds to the score of sections that match another word (`[settings.ranking] common_word_share`, default
  0.2; 1 restores the old behaviour). When fewer sections than asked for match, every word counts again, and no word
  is set aside in a query of stop words only, with `min_should_match` above 0, or when it has synonyms. The share is
  counted over the whole index, so which sections match also depends on the other libraries indexed.
  Improvement-loop iteration 23: identical dev results, p50 −22% and p95 −30% in `cargo xtask perf`. No rebuild
  needed.
- The default `title_weight` is 3 instead of 4 (improvement-loop iteration 19, MRR@10 +0.01% on the dev set). An
  index built with the old default asks for `sync --rebuild`.
- A website with no `allow_prefixes` whose start page is one segment deep (`/docs`, `/introduction`) now crawls the
  whole host, the start page's directory, so that sites keeping every page at the root are crawled whole; set
  `allow_prefixes = ["/docs"]` to keep such a crawl to one section.

### Fixed

- A configured URL that redirects no longer stores the target's ETag and Last-Modified, which went back to the
  requested URL on the next sync. An index synced before this fix keeps those validators while the URL answers 304
  to them; the next full download or `sync --rebuild` drops them.
- A configured URL with a fragment is rejected when the config is checked; before, it never kept its ETag and
  Last-Modified, and a website's start page with one failed the crawl as a redirect outside its scope.
- A website's crawl prefixes match whole path segments (`/docs` no longer covers `/docs-v1/`), and with no
  `allow_prefixes` the crawl covers the start page's directory rather than only paths that begin with its full path
  (a start page one segment deep is under Changed); a prefix that ends in `/` also covers its bare path (`/docs/`
  covers `/docs`), to which servers often redirect it. Stored pages outside the new scope are deleted on the next sync.
- A scored term of a field that the metadata documents share no longer aborts the search.
- MDX stripping follows MDX's own rule for `import`/`export` blocks, and removes much less content it should keep.
  - A block starts at the beginning of a line with the keyword and whitespace, and ends at a blank line. While its
    braces are open it runs on across a blank line, but only into a line that is indented or starts with a closing
    bracket, so a stray brace cannot drop the rest of a page.
  - Known limits of this line scanner, which cannot match MDX's JavaScript parser:
    - only braces are counted, not `(` or `[`, and braces inside strings, comments and regexes count too, so a block
      that spans a blank line can end early and leave JavaScript in the indexed text, or take an indented paragraph
      after it;
    - after a blank line, an unindented continuation (a function body written at column 0) ends the block;
    - text written directly under a block, with no blank line between, goes with it, as it does in MDX, which
      rejects such a file; a code fence there is read as part of the block;
    - an apostrophe inside a tag's `{…}` comment keeps a component line, and a component that spans lines is kept;
    - a fence indented 4 or more spaces or by a tab (in a nested list or indented JSX) is not seen as one, though MDX
      reads it as a fence, so component lines in it, and `import`/`export` lines at column 0, are removed;
    - a tab after `import` or `export` starts a block, where MDX may read a paragraph.
  - "export your data" or "import the file" is prose, and an import block that never names its module is kept.
  - Code fences use the `CommonMark` rule the search uses: a fence closes only on a fence of the same character at
    least as long with nothing after it, so a four-backtick example holding a three-backtick block stays whole.
  - A component line is removed only when it holds nothing but tags; `<Note>text</Note> <Br />` is kept, and a `>`
    inside a `{…}` attribute does not end the tag.
  - A front matter block after a byte order mark is read.

  Pages that hit these cases change on the next `sync --rebuild`.
- `zenodotus-eval` keeps the corpus sources in `zenodotus-eval-sources` next to the repository root by default, not
  under `target/`, whose paths Grounded Docs skips: it used to index one empty page and score 0 on every question.
  `compare` now reports Grounded Docs as not run when it indexes one page or fewer, or does not print a page count,
  naming the folder, and gives it the docs folder's real path (no `..` components or symbolic links, which it
  refuses) as a percent-encoded `file://` URL.
- `compare` reports a local copy that cannot be resolved (missing, or behind a folder that cannot be searched) or is not a folder with the reason, for Grounded Docs and Neuledge.
- The HTTP transport refuses a request whose `Origin` is not in `allowed_origins`, also when the list is empty; before,
  an empty list let every origin through.
- The error of a redirect to a host the source may not reach names the redirect's target once, and names the
  `extra_hosts` remedy only when the host is a DNS name, the one kind of host that setting accepts.
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

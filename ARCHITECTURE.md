# Architecture

zenodotus indexes documentation into a local Tantivy index and serves it over MCP. Retrieval is lexical and
deterministic: no language model, no embeddings and no inference run anywhere. The same index files and the same
request produce a byte-identical response.

## Workspace

```text
crates/
  core     zenodotus-core     ids, settings, library config, domain types, clock; config load/edit; code fences
  ingest   zenodotus-ingest   sources, secure fetching, Markdown and HTML parsing, sectioning, change detection
  search   zenodotus-search   the Tantivy store: schema, analyzers, apply, query, resolve
  mcp      zenodotus-mcp      the rmcp server: tools, rendering, stdio and Streamable HTTP transports
  cli      zenodotus          the binary: commands, sync orchestration, output
  eval     zenodotus-eval     (not published) retrieval evaluation, competitor comparison, performance runs
xtask                         (not published) repository checks and `cargo xtask perf`
```

Dependencies point one way. `core` depends on nothing internal; `ingest` and `search` depend only on `core`; `mcp`
depends on `core` and `search`; `cli` depends on all four. `ingest` never touches storage: it returns a
`VersionChanges` value and `search` applies it in one commit, so the write path is testable without a network and
the read path without parsing.

## Data flow

```text
zenodotus.toml ([settings] + [[library]])
   │  zenodotus sync            (takes data_dir/sync.lock; a second sync exits with code 4)
   ▼
Store::doc_states(scope) ──► the previous state of every document (ETag, Last-Modified, content hash)
   │
   ▼
ingest::collect   Local | GitHub | Url | LlmsTxt | Website
   │  discover → fetch → parse (Markdown, or HTML converted to Markdown) → sectionize
   ▼
VersionChanges { upserts, unchanged, deletions, errors, resolved_commit }
   │
   ▼
Store::apply      one Tantivy commit per version: delete changed and deleted documents by key,
   │              add their new sections, replace the version's state record
   ▼
Store::query / Store::resolve  ◄── the MCP tools and `zenodotus search`
```

## Sections

A document is split at every heading of level 1 to 4. A section keeps its heading path, its code blocks and the
paragraph that introduces each code block. A section longer than 6,000 characters splits at paragraph boundaries,
never inside a code fence. Front matter is removed, and so are import, export and self-closing component lines in
`.mdx` files.
HTML pages are reduced to their main content (`main`, `article`, documentation-framework roots, or a Readability
port when none exists) and converted to Markdown before sectioning.

Symbols are extracted deterministically from inline code that looks like an identifier, from the first line of each
code block and from identifier-like headings.

## Ranking

Tantivy has no BM25F and no separate query-time analyzer, so zenodotus works around both.

- **Term-frequency injection.** The multi-valued field `combined` receives the title and heading path, the symbols,
  the body and the document path's words, each repeated by its `[settings.index]` weight, inside one length
  normalization. This approximates BM25F ([Robertson et al., CIKM 2004](https://ir.webis.de/anthology/2004.cikm_conference-2004.6);
  [Sourcegraph's report](https://sourcegraph.com/blog/keeping-it-boring-and-relevant-with-bm25f)).
- **Two analyzers.** `code_words` splits on punctuation, camelCase, acronyms and `_` without stacking the original
  form, which would inflate document length. `code_ident` indexes whole identifiers joined by `::`, `.`, `#` or
  `->`, every qualified suffix (`std.sync.arc`, `sync.arc`, `arc`) and the snake-joined form, into `symbols_exact`.
- **Queries are built in code**, never through Tantivy's query parser: non-stop words must match; exact symbols,
  quoted phrases and the requested code language add score; stop words count only in a query of stop words alone. A non-stop word in more than
  `common_word_share` of the sections (20% by default) only adds score, unless no section matches another word.
- **Determinism.** Ties break on the section's `chunk_id`, and the top-k fetch keeps paging while the next hit ties
  the k-th, so no tie straddles the cut.

Corpus statistics are global to the index and count only section documents: the state documents of each document
and version have no searchable text and are left out. Scores still depend on every other section indexed. The
section count and each term's document count include sections deleted but not yet merged away, so they stay on one
basis; only fields that sections alone carry are scored, and the filters on shared fields add no score.

## Storage and processes

The data directory holds one Tantivy index for all libraries, with section, document-state and version-state
documents, plus `sync.lock`.

- `sync` is the only writer. One sync of one version is one atomic Tantivy commit, so a crash leaves either the
  previous index or the new one, and a failed sync leaves the previous index usable.
- `serve`, `search`, `list` and `status` only read. `serve` reloads on commit, and each request reads one snapshot.
  Readers never move directories: after a crash between the two renames of a rebuild's swap they read `index.old/`,
  and the next `sync`, which holds the lock, repairs the layout and deletes any leftover `index.old/`.
- The commit payload holds the schema version and a hash of `[settings.index]`. A mismatch at open fails with
  `schema_mismatch` and asks for `zenodotus sync --rebuild`, which builds a new index and swaps it in when no version
  fails. Documents that fail during a rebuild are absent from the new index until a later sync reads them. A running
  server keeps the store it opened, so restart it after a rebuild; plain syncs need no restart.

## Incremental sync

| Source | Change detection | Deletion evidence |
|---|---|---|
| Local | content hash | missing from a complete directory scan |
| GitHub | the resolved commit and the source, `paths` included; both unchanged skip the download | missing from a complete tarball listing |
| Url | ETag or Last-Modified (304) when the URL answered without redirecting, then content hash | 404 or 410, or removed from the config |
| LlmsTxt | as Url, for the file and every linked page | missing from the new `llms.txt`, or 404 or 410 |
| Website | as Url, per page | 404 or 410, or a permanent redirect outside the crawl scope or to a page already reached |

Any other per-document error, a tarball file that is too large or not UTF-8 included, keeps the previous content and
marks the version `partial`. A page whose content is unchanged but whose ETag or Last-Modified changed is stored
again, so the next sync sends the current validators. When the listing itself cannot be obtained, nothing changes and
the version is `failed`. A website has no listing that proves a page is gone: a page that answers 304 is not parsed
again, so it adds no links, and a page linked only from it is never reached through it. Every crawl therefore visits
the start page, then every stored page, the start page's links and the sitemap pages, in that order, and deletes a
page only when it answers 404 or 410, or redirects permanently (301 or 308) outside the crawl scope (to another host
included) or to a page already reached in this crawl. A page that redirects is indexed under the URL it redirects to,
from the body the redirect already downloaded. A stored copy under the old URL is deleted only when every redirect
followed was permanent, since a 302, 303 or 307 can be maintenance, and a 404 or 410 reached through one deletes
nothing either: it is an error for a stored page and nothing for a page never stored (found through a link or a
sitemap), as a direct 404 is; if the target's own fetch failed earlier in the crawl, the old copy is kept. The stored
source URL never carries a fragment from a redirect's `Location`. A redirect is one fetch of the `max_pages` budget
(page visits; robots.txt and sitemaps do not count), and a start page that redirects outside the crawl scope fails the
version. Validators are sent only to the requested URL, never to a redirect target: the target may be another
resource, where a date validator could answer 304 for content it never sent, or another origin. For the same reason
a document kept under a URL that redirected stores none of the target's validators, so it is downloaded in full on
every sync. Only a configured
sitemap must be readable: a sitemap found in robots.txt that cannot be read is reported and the next one is read, a
guessed `/sitemap.xml` that cannot be read is ignored, and an index of more than 50 sitemaps stops reading.

Pages beyond the stored ones are found only through the sitemap and changed pages; `zenodotus sync --rebuild` crawls
the site from scratch.

## Secure fetching

All network access goes through one `Fetcher`:

- `https` only, unless `fetch.allow_http` is set.
- Redirects are followed by hand, at most 5, and every hop is checked for scheme, allowed host and address. Extra
  request headers, such as the GitHub token, are sent only to the origin of the first request.
- A resolver guard drops private, loopback, link-local, multicast and reserved addresses, IPv4 and IPv6, and the
  client connects only to addresses that passed, which defeats DNS rebinding. System proxies are disabled so they
  cannot bypass the guard.
- Size caps apply while streaming: pages, documents, tarballs, tar entries, total kept bytes, entry count, and a
  decompressed-size cap of 16 times the tarball cap against gzip bombs.
- Tar entries with absolute or `..` paths, symlinks and hardlinks are skipped; local sources never follow symlinks.
- Website crawls honour robots.txt and Crawl-delay, space requests per host and stay under `allow_prefixes`, matched
  on whole path segments (the start page's directory when none is given, which is the whole host for a start page one
  segment deep; a prefix ending in `/` also covers its bare path); the start page is exempt from
  `allow_prefixes`, not from robots.txt. Configured URLs may not carry a fragment.
- Retries cover connection errors, 429 and 5xx, with exponential backoff, deterministic jitter and `Retry-After`
  in seconds.

## MCP server

Two tools, built with rmcp 3.11:

- `resolve_library(library_name, query?)`: matches by id, alias, normalized name, prefix, then Jaro-Winkler.
- `query_docs(library_id, query, version?, language?, max_results?, max_chars?)`: returns structured content and one
  Markdown block. Each section is wrapped in `<<<DOC …>>>` markers with a hash, after a preamble saying the content
  is untrusted data, not instructions. Its title, heading path and source reference come from the fetched document,
  so they sit inside the markers with its text; only the rank, score and signals are outside.

An explicit version never falls back to another. Unknown libraries and versions, empty or oversized queries and an
empty index are tool errors with an actionable message. Over stdio, stdout carries only protocol messages. Over
Streamable HTTP, the server binds to loopback by default, refuses a non-loopback bind without
`ZENODOTUS_HTTP_TOKEN`, checks Host and Origin, caps request bodies and concurrent requests, and compares the bearer
token in constant time.

## Decisions that differ from the first design

- Malformed tool arguments come back as tool errors rather than `invalid_params`: rmcp 3.11 follows MCP SEP-1303,
  which lets the model correct itself. `invalid_params` remains for unknown tools.
- The server holds the data directory rather than an open store, so a server started before the first sync answers
  `index_empty` and picks up the index once it exists.
- HTML is converted to Markdown with `dom_query`'s serializer, and with `dom_smoothie`'s Markdown mode for pages that
  need Readability, instead of `htmd`, one dependency less. Content roots are tried most specific first:
  documentation-framework roots, then `article`, `[role=main]` and `main`.
- Tarball chunks reach the blocking decompressor through a channel, not `SyncIoBridge`.
- `Retry-After` HTTP dates are ignored; only seconds are honoured.
- robots.txt answering 4xx allows everything; 5xx or a network error fails the version, following Google's rules.
- Stored pages are always revisited, and a page the crawl did not reach is never deleted for that: with 304 answers,
  a complete sitemap does not prove that an unlisted page is gone.
- The evaluation talks to every system, zenodotus included, over stdio MCP, so local latencies compare like for
  like. Context7 is reached over its public Streamable HTTP endpoint.
- The concurrency limit of the HTTP transport is wired but no test proves it, because stateless JSON requests finish
  too fast to hold one in flight.
- Resolution accuracy (whether `resolve_library` finds the intended library) is not measured.

## Quality gates

`cargo xtask check-repo` enforces the Markdown allowlist, the absence of task markers and debugging macros, printing
only from the CLI output module, the evaluation report and `xtask`, and `#![forbid(unsafe_code)]` with `#![deny(missing_docs)]` in every crate root.
`cargo xtask check-errors` checks that every error variant is named in at least one test; that the test asserts it is
checked by reading. `cargo coverage` requires 100% of lines and functions; its one exclusion, `xtask/src/perf.rs`,
only orchestrates external tools; every number it reports is measured by tested code in `zenodotus-eval`, by GNU
time or by hyperfine, or computed by the tested parsers of `xtask/src/perf_report.rs`.

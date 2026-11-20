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

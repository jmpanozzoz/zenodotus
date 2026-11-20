# Security

## Reporting a vulnerability

Report vulnerabilities privately through the repository's GitHub security advisories ("Report a vulnerability").
Do not open a public issue. Include the version, the config that triggers the problem and a minimal reproduction.
Expect an acknowledgement within a week.

## Threat model

zenodotus downloads documentation from the network and hands it to AI agents. It defends against:

- **Server-side request forgery.** Every fetched host is checked against the source's allowed hosts. Every resolved
  address is checked against private, loopback, link-local, multicast and reserved ranges, for IPv4 and IPv6, and
  the client connects only to addresses that passed, so DNS rebinding cannot reach a blocked address. Redirects are
  followed by hand and checked hop by hop. System proxies are ignored. `fetch.allow_private_networks` turns the
  address guard off and exists for tests and trusted intranets only.
- **Unsafe schemes.** Only `https` is fetched, unless `fetch.allow_http` is set.
- **Path traversal.** Tar entries with absolute or `..` paths, symlinks and hardlinks are skipped. Local sources
  never follow symlinks and only read files inside their canonical root.
- **Resource exhaustion.** Pages, documents, tarballs, tar entries, total kept bytes and the number of entries are
  capped while streaming; decompression is capped at 16 times the tarball size against gzip bombs. Website crawls
  honour robots.txt and Crawl-delay and space requests per host.
- **Prompt injection through documentation.** Returned text is wrapped in `<<<DOC …>>>` markers after a preamble that
  declares it untrusted data, and any end marker inside the text is escaped. This reduces, but cannot remove, the
  risk that an agent follows instructions found in documentation.
- **Network exposure of the server.** `serve --http` binds to loopback by default, refuses a non-loopback address
  unless `ZENODOTUS_HTTP_TOKEN` is set, checks Host and Origin headers, caps request bodies and concurrent requests,
  and compares the bearer token in constant time. The MCP tools only read the index: they never touch files or the
  network.

The crates contain no `unsafe` code (`#![forbid(unsafe_code)]`), and `cargo deny` checks advisories and licenses in
CI.

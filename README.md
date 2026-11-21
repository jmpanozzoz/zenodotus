# zenodotus

Deterministic documentation retrieval for AI agents, over MCP.

zenodotus indexes the documentation you choose (GitHub repositories, local folders, Markdown URLs, `llms.txt` files
or documentation websites) into a local index, and serves it to coding agents through two MCP tools,
`resolve_library` and `query_docs`. Retrieval is lexical and code-aware. There is no language model, no embedding and
no inference anywhere: the same index and the same question always return the same bytes, verbatim from the source,
with a link back to it.

- **Self-hosted and offline.** One binary and one index directory. After a sync, queries need no network.
- **Versions are explicit.** Index several versions of a library; a query that names a version never gets another.
- **Fast.** p95 under 2 ms on 115,000 sections, a few milliseconds to start, about 90 MiB of memory while serving.
- **Incremental.** Re-syncs skip unchanged commits, honour ETag and Last-Modified, and remove deleted pages.
- **Apache-2.0.**

## Install

From source, with Rust 1.89 or later:

```bash
cargo install --locked --path crates/cli
```

Or build the container image:

```bash
docker build -t zenodotus .
```

## Quick start

```bash
zenodotus init                                   # writes ~/.config/zenodotus/zenodotus.toml
zenodotus add tokio --version latest --default --github tokio-rs/website --docs-path content
zenodotus add my-project --version main --path ./docs
zenodotus sync                                   # downloads and indexes
zenodotus search tokio "spawn a task"            # the same engine the MCP tool uses
zenodotus status
```

[`examples/zenodotus.toml`](examples/zenodotus.toml) shows every kind of source. The config file is found through
`--config`, then `$ZENODOTUS_CONFIG`, then `$XDG_CONFIG_HOME/zenodotus/zenodotus.toml`, then
`~/.config/zenodotus/zenodotus.toml`. Set `GITHUB_TOKEN` to raise GitHub's API rate limit.

| Command | What it does |
|---|---|
| `init` | Write a commented config and create the data directory |
| `add <id> --version V (--path \| --github \| --url \| --llms-txt \| --website) …` | Add a library or a version, keeping the file's comments |
| `list` | Configured libraries and versions, with their indexed status |
| `sync [<id>] [--version V] [--rebuild]` | Index every configured version; without `<id>`, also drop versions no longer configured |
| `search <id> <query…>` | Query from the terminal, as `query_docs` does |
| `serve [--http [ADDR]]` | Serve MCP over stdio, or Streamable HTTP |
| `status` | Sync status of every version and the index size |

Large repositories may need higher fetch limits, for example `tar_max_entries` under `[settings.fetch]` for
`vercel/next.js`, whose tarball holds about 60,000 entries against a default cap of 50,000.

## Connect an agent

Claude Code:

```bash
claude mcp add zenodotus -- zenodotus serve
```

Codex (`~/.codex/config.toml`):

```toml
[mcp_servers.zenodotus]
command = "zenodotus"
args = ["serve"]
```

Cursor (`.cursor/mcp.json`):

```json
{ "mcpServers": { "zenodotus": { "command": "zenodotus", "args": ["serve"] } } }
```

For a shared server, run `zenodotus serve --http` (loopback, `127.0.0.1:8080` by default) and point clients at
`http://127.0.0.1:8080/mcp`. Binding any other address requires `ZENODOTUS_HTTP_TOKEN`, sent by clients as a bearer
token. In the container:

```bash
docker run -v "$PWD/data:/data" -e ZENODOTUS_HTTP_TOKEN=change-me -p 8080:8080 zenodotus serve --http 0.0.0.0:8080
```

with `/data/zenodotus.toml` setting `data_dir = "/data"`.

## Tools

- `resolve_library(library_name, query?)` finds library ids by id, alias, name, prefix or fuzzy match, with their
  versions and sync status.
- `query_docs(library_id, query, version?, language?, max_results?, max_chars?)` returns the best sections, each with
  its source reference, version, heading path, score and match signals, inside markers that declare the text
  untrusted data. Unknown libraries or versions return an error that lists what exists.

## How it compares

Measured with `zenodotus-eval compare` on the held-out test set: 65 questions over FastAPI, Next.js, the Tokio
website, Pydantic 2.12 and 1.10, and Vite, each pinned to a commit. A question counts as found when a returned
section contains all of its answer strings. Intervals are 95% bootstrap intervals; with 65 questions they are wide.

| System | Answered | Excluded | Failed | Recall@5 [95% CI] | MRR@10 [95% CI] | p50 ms | p95 ms | Median tokens (est.) |
|---|---|---|---|---|---|---|---|---|
| zenodotus | 65 | 0 | 0 | 0.877 [0.785, 0.954] | 0.736 [0.643, 0.821] | 0.7 | 0.8 | 3092 |
| context7 | 65 | 0 | 0 | 0.785 [0.677, 0.877] | 0.695 [0.590, 0.792] | 1107.0 | 1333.6 | 1105 |
| grounded-docs | 65 | 0 | 0 | 0.923 [0.846, 0.985] | 0.855 [0.778, 0.927] | 3.7 | 5.8 | 7391 |
| neuledge-context | 65 | 0 | 0 | 0.108 [0.046, 0.185] | 0.108 [0.046, 0.185] | 0.4 | 0.7 | 54 |

Tokens are estimated as characters / 4 for every system. Context7 is a hosted service reached over the network;
the others run locally over stdio MCP, so latencies are not like for like. Grounded Docs ran in full-text mode with
no embeddings. Neuledge Context requires every query word to match, which rarely happens with natural-language
questions. Our pre-registered target was a Recall@5 interval for zenodotus − Context7 whose lower bound is above
−0.02; the measured interval is [−0.031, 0.215], so the target is **not met**: zenodotus is ahead of Context7 on
these questions (0.877 against 0.785), but 65 questions cannot rule out a small deficit. Grounded Docs ranks best at
full response size (0.923), with responses more than twice as long; cut to Context7's median response size (about
1,105 tokens), Recall@5 is 0.862 for zenodotus, 0.769 for Context7 and 0.723 for Grounded Docs.

Scores depend on the questions, written for this project, and on everything else in the index. The harness, the
questions and the raw results format are in this repository, so you can rerun it; see
[CONTRIBUTING.md](CONTRIBUTING.md).

## Performance

Measured with `cargo xtask perf` on the MDN content repository (14,758 documents, 115,849 sections), as the median
of three runs, each after a full sync, on an AMD Ryzen 9 9950X (16 cores, 32 threads), 121 GiB of RAM, Ubuntu 26.04
with Linux 7.0, and rustc 1.100.0.

| Metric | Measured | Target |
|---|---|---|
| Query latency over stdio MCP, p50 / p95 (2,000 calls) | 0.79 ms / 1.64 ms | p95 under 20 ms |
| `serve` start to the first `tools/list` response (hyperfine) | 3.6 ms | under 300 ms |
| Peak RSS while serving | 91 MiB (93,444 KiB) | under 150 MB |
| Indexing throughput | 2,229 documents per CPU second (user + system) | reported |
| Re-sync with no upstream change | 0.05 s | reported |
| Index size on disk | 102.2 MiB | reported |

The latency and memory figures depend on the index and the machine; `cargo xtask perf` reproduces them, see
[CONTRIBUTING.md](CONTRIBUTING.md).

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md): how indexing, ranking, storage and the server work, and why.
- [SECURITY.md](SECURITY.md): the threat model and how to report a vulnerability.
- [CONTRIBUTING.md](CONTRIBUTING.md): gates, evaluation and benchmarks.
- [CHANGELOG.md](CHANGELOG.md).

## License

Apache-2.0. See [LICENSE](LICENSE).

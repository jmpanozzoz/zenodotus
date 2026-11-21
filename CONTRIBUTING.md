# Contributing

## Toolchain

`rust-toolchain.toml` pins the compiler (1.100) with rustfmt, clippy and llvm-tools. The minimum supported version
is 1.89. The gates also need `cargo-llvm-cov`, `cargo-deny`, `cargo-machete` and `cargo-mutants`:

```bash
cargo install cargo-llvm-cov cargo-deny cargo-machete cargo-mutants
```

## Rules

- **Test first.** Every behaviour starts as a failing test. Every change to `src/` comes with its tests.
- **Bad paths count.** Every variant of every error enum must be produced and asserted by a test;
  `cargo xtask check-errors` enforces it.
- **No stray files.** Markdown is allowed only as `README.md`, `CONTRIBUTING.md`, `SECURITY.md`, `ARCHITECTURE.md`
  and `CHANGELOG.md` at the root, plus fixtures under `fixtures/`. No plans, notes or reports in the repository.
- **Library code does not print.** Only the CLI output module, the evaluation report and `xtask` write to stdout or
  stderr; everything else uses `tracing`.
- **Commits are small**, with a descriptive English message.

## Gates

Run these before every commit; CI runs the same list.

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings -W clippy::pedantic
cargo xtask check-repo
cargo xtask check-errors
cargo coverage                       # tests, failing under 100% of lines or functions
cargo build --release
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
cargo machete
```

`cargo coverage` is an alias in `.cargo/config.toml`. Its only exclusion, `xtask/src/perf.rs`, is justified there.

### Mutation testing

```bash
cargo mutants -p zenodotus-core -p zenodotus-ingest -p zenodotus-search -j 4 --timeout 120
```

Some mutants turn a scanning loop into one that never advances and allocates without bound. On a shared machine, cap
the run's memory so the kernel kills only the runaway test, which then counts as caught:

```bash
systemd-run --user --scope -p MemoryMax=16G -p MemorySwapMax=0 -p OOMPolicy=continue \
  cargo mutants -p zenodotus-core -p zenodotus-ingest -p zenodotus-search -j 4 --timeout 120
```

Every surviving mutant is either killed with a new test or justified.

## Evaluation

The question sets live in `eval/questions/{dev,test}.jsonl`. **Tune only on `dev`.** The `test` set is for
checkpoint and final reports and never chooses a change.

1. Check out every corpus source at its pinned commit. `eval/corpus.toml` lists each repository, commit and docs
   path; each source goes to `<sources>/<library>` (or `<library>-<version>` for a library with several versions).
   `<sources>` defaults to `target/eval-sources`, but Grounded Docs skips every path under a `target`, `build`,
   `dist`, `out` or `test` folder, so for `compare` check them out elsewhere and pass `--sources`, for example:

   ```bash
   mkdir -p ../zenodotus-eval-sources/tokio
   curl -sL https://codeload.github.com/tokio-rs/website/tar.gz/71cdb3858fbb943843e8cf0e8baf547fe22180f7 \
     | tar xz --strip-components=1 -C ../zenodotus-eval-sources/tokio
   ```

2. Build the index and validate the questions against the real sectioner:

   ```bash
   cargo build --release --bin zenodotus --bin zenodotus-eval
   target/release/zenodotus --config eval/corpus.toml sync
   target/release/zenodotus-eval validate
   ```

3. Measure:

   ```bash
   target/release/zenodotus-eval run                  # zenodotus on the dev set
   target/release/zenodotus-eval --sources ../zenodotus-eval-sources compare   # every system on the test set
   ```

   `compare` needs Node 20 or later for Grounded Docs and Neuledge Context, which it starts with `npx` (the same
   Node that first installed them, since their native modules are built for it), and network
   access for Context7 (`CONTEXT7_API_KEY` is optional). A system that cannot run is reported as `not run: <reason>`.
   Raw results and the summary are written under `eval/results/`, which is not committed.

## Performance

```bash
cargo bench -p zenodotus-search      # criterion: query and indexing on a synthetic corpus
cargo xtask perf                     # MDN's English pages: sync, re-sync, cold start, latency, peak RSS
cargo xtask perf --reuse             # the same, reusing the synced index
```

`cargo xtask perf` needs GNU time at `/usr/bin/time` and `hyperfine`, and downloads the pinned `mdn/content`
tarball from GitHub on its first run. It writes `target/perf/summary.txt`.

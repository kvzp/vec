# Testing

## Run

```bash
cargo test                          # whole workspace; no network, no model, no system paths
cargo test -p vec-store             # one crate
cargo test chunker                  # tests whose name matches
cargo clippy -- -D warnings          # as CI; add --all-targets to lint tests too
cargo fmt --check
cargo build -p vec-cli --no-default-features --features system-sqlite   # distro build (needs libsqlite3-dev)
```

CI (`.github/workflows/ci.yml`) runs build, test, clippy, fmt, `cargo audit`
and the `system-sqlite` build.

### Real-model tests

Tests that need a real ONNX model are `#[ignore]`d and read its path from
`VEC_TEST_MODEL` (`tokenizer.json` must sit beside it):

```bash
VEC_TEST_MODEL=/usr/share/vec/models/gte-multilingual-base/model_int8.onnx \
  cargo test -- --ignored
```

They cover: output selection and pooling, batched vs single-text vectors
(`onnx_batch_size`), daemon vectors equal to in-process vectors within 1e-4,
model sha256 caching, and an index + search round trip (vec-embed, vec-daemon,
vec-core, vec-index, vec-query).

### Unix-socket tests

The vec-daemon tests (`server.rs`, `client.rs`, `activation.rs`) bind and
connect Unix sockets in a temporary directory: lifecycle, stalled and trickling
clients, short bodies, bad UTF-8, busy replies, limits, idle exit, socket
activation. They need nothing but a filesystem that allows sockets; a sandbox
that blocks `bind`/`connect` on Unix sockets (some bubblewrap or seccomp
setups) makes them fail, so run them outside such a sandbox.

## Conventions

- **Hermetic configuration.** Tests build configs with `Config::defaults()` or
  `Config::from_toml(..)` (no file read), and layering tests pass explicit
  system/user paths, so `/etc/vec.conf` and `~/.config/vec/config.toml` of the
  machine never change a result.
- **No model by default.** `Embedder::stub(dim)` gives deterministic unit vectors.
- **Temporary files only** (`tempfile`); never write to system paths.
- Unit tests live in `#[cfg(test)]` modules (vec-mcp and vec-query keep them in
  `src/tests.rs`).
- Deterministic: no network, no wall-clock assertions beyond generous timeouts.

## What is covered

| Crate | Highlights |
|-------|-----------|
| vec-core | layers, deny-unknown-fields, every validation rule, `!pattern` removal, provenance, deprecated `daemon_socket`, `vec init` templates, snippets on stale offsets and UTF-8 boundaries, symlink-safe capped reads, sha cache |
| vec-embed | HTTP client (status, chunked bodies, Content-Length, numeric checks and counts, URL path, request batching, connect failure), truncation at character boundaries, identity compatibility |
| vec-store | migration v0 → v1, model identity (`ensure_model`/`check_model`/`ModelMismatch`), atomic `replace_file`, search (filters before limit, directory-boundary path filter, blob checks), read-only open |
| vec-index | chunker property test (every line covered, ordered offsets), incremental update (size+mtime skip, hash refresh, chunk reuse), prune, `--full` semantics, probe-text failure handling, symlink swaps (`O_NOFOLLOW` + dev/inode), FIFO, non-UTF-8 names, `diff` |
| vec-watch | pending set and collapse, overflow → rescan, retry backoff, failing flushes, watches following the walker, SIGHUP reload (and keeping the old runtime on failure) |
| vec-daemon | protocol, deadlines, busy reply, limits, stale socket, socket activation |
| vec-query | ranking and path boost, filters, model checks, daemon fallback, status |
| vec-mcp | multi-query search with excludes, error results, `context` limited to indexed files, `similar`/`explain`, `index_status` |

Known gaps: macOS/BSD are not tested; the inotify queue-overflow path is
exercised by unit tests only (forcing a kernel overflow needs root); the
randomised watcher-vs-`--full` comparison used while developing `vec watch` is
not part of the test suite (`scoped_and_full_runs_agree` covers the indexer side).

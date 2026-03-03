# TESTING.md

Testing strategy, current coverage, and known gaps for `vec`.

## Running tests

```bash
cargo test                          # all unit tests (no network, no model)
cargo test -- --nocapture           # show println! output
cargo test store                    # tests in a specific module
cargo test --features integration   # tests requiring a live ONNX model (future)
```

## Current coverage (79 tests)

| Module | Tests | What's covered |
|--------|-------|----------------|
| `store.rs` | 27 | open/create schema, upsert, get, delete, search, cosine math, path filter (incl. `[` special char), model check, stats, full pipeline, byte_end clamping pattern, bad-parent error, paged search top-k correctness |
| `index.rs` | 24 | chunker (empty, short, boundary snap, offsets, overlap, no trailing newline, CRLF), glob matching, updatedb (index, skip unchanged, full re-index, binary skip, size limit, path filter, unreadable dir, symlink skip) |
| `embed.rs` | 10 | stub unit length, determinism, different texts differ, batch vs single, empty batch, long text, sha256, missing model error, missing tokenizer error |
| `config.rs` | 13 | defaults, merge, extra path, bad TOML, no user config, central DB path, system model dir, include paths, tilde expansion (plain path, bare `~`) |
| `daemon.rs` | 2 | embed request round-trip, oversized request rejection |
| `util.rs` | 3 | can_read readable file, non-existent file, directory |

All tests run without network access, without a real ONNX model, and without writing to system paths.
The stub embedder (`Embedder::stub(768)`) is used wherever an embedder is needed.

## Known gaps

### Open

**1. Real model integration tests** (`#[cfg(feature = "integration")]`)
- `Embedder::load` + `embed_batch` with actual `gte-multilingual-base` ONNX
- Validates `probe_dim`, `is_pre_pooled`, L2-normalisation on real output
- Requires model file at test time; gate behind `--features integration`
- Without this, the ONNX inference path (`EmbedderInner::Tract`) is untested

### Resolved

**1 (remaining). Real model integration tests** (`#[cfg(feature = "integration")]`)
- `Embedder::load` + `embed_batch` with actual `gte-multilingual-base` ONNX
- Requires model file at test time; gate behind `--features integration`
- Without this, the ONNX inference path (`EmbedderInner::Tract`) is untested

**2. File race condition at query time** — fixed in `main.rs`; pattern tested in `store::tests::byte_end_clamping_is_safe`

**3. Search path filter with special characters** — fixed (LIKE → GLOB with `[[` escaping); tested in `store::tests::search_path_filter_with_bracket_char`

**4. `chunk_file` edge cases** — no-trailing-newline and CRLF tested in `index::tests::chunk_file_no_trailing_newline` / `chunk_file_crlf_line_endings`; a file with no `\n` at all produces a single "line" below `MIN_CHUNK_LINES` → zero chunks (correct, not a bug)

**5. `expand_tilde` with missing home dir** — current behaviour (return path with literal `~`) is documented and tested in `config::tests::expand_tilde_tilde_alone`; returning `Err` would break `Config::load` in containers and is not worth the churn

**6. `run_updatedb` walk error handling** — unreadable directories produce a `warn:` progress line and increment `stats.errors`; tested (Unix-only, skipped as root) in `index::tests::run_updatedb_skips_unreadable_directory`

**7. SQLite error conditions** — opening a store under an invalid parent (`/dev/null/`) returns `Err`; tested in `store::tests::open_store_bad_parent_returns_err`

**8. Brute-force search memory limit** — `Store::search` now pages through chunks in batches of 1 000 rows; memory is O(page × dim + k) regardless of index size; top-k tracked with a min-heap. Tested in `store::tests::search_paged_returns_correct_top_k`.

**9. Model graph caching** — `vec daemon` keeps the compiled ONNX graph in memory over a Unix socket (`/run/vec/embed.sock`). `vec <query>` tries the socket first, falls back to in-process load if the daemon is not running. Tested in `daemon::tests::*`. Systemd unit: `contrib/vec-embed.service`.

**10. Probe token hardcoding** — `probe_dim` now tokenizes `"hello"` with the real tokenizer instead of hardcoding BERT IDs; fixed in `embed.rs`

## Test conventions

- Unit tests live in `#[cfg(test)] mod tests { ... }` at the bottom of each source file
- Use `tempfile::NamedTempFile` / `tempfile::TempDir` — never write to system paths
- Use `Embedder::stub(768)` — never require a model file in unit tests
- Use `Store::open(":memory:")` — never write to disk in unit tests
- Integration tests (real model, disk I/O) → `cargo test --features integration`
- Tests must be deterministic: no `std::time`, no randomness, no network

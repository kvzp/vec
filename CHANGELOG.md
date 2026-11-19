# Changelog

All notable changes to this project will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning follows [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

Audit of 2026-11-19 ([tasks/audit-2026-11-19.md](tasks/audit-2026-11-19.md)) and its review: correctness, robustness and security fixes across every crate.

### Breaking — read before upgrading

- **Rebuild the index: `sudo vec updatedb --full`.** Earlier versions read the ONNX model's first output (`token_embeddings`) as if it were the pooled `sentence_embedding`, so `gte-multilingual-base` indexes hold wrong 128-dimension vectors. The model output is now chosen by name. Until the rebuild, queries and incremental updates stop with a "model mismatch" error and `vec watch` does not start.
- **Index schema version 1** (`PRAGMA user_version`): adds `files.size` and `chunks.text_hash`, stores vectors normalised, records the model identity (`model_name`, `embedding_dim`, `model_sha256`). The first writer upgrades an existing index in place in one transaction; queries cannot read an old index until then.
- **Configuration is strict**: unknown keys and sections are errors in every layer; values are validated (`chunk_overlap < chunk_size`, absolute paths, ranges, timeout ordering, ...) and errors name the file that set the key. `embed_url` must start with `http://`. `[embed] daemon_socket` is deprecated (mapped to `[daemon] socket` with a warning, for one release). New sections and keys: [CONFIG.md](CONFIG.md).
- **No silent stub embedder**: a model that cannot be loaded is an error (it used to index meaningless vectors).
- **Daemon protocol v2** (handshake with model identity, batched requests). v1 daemons and clients are not compatible: restart `vec daemon` after upgrading. The socket setting moved from `[embed] daemon_socket` to `[daemon] socket`.
- **MCP results**: `search` items gain `line` (best-matching line) next to `start_line`/`end_line`; `index_status` returns the full status report (`files`, `chunks`, `schema_version`, `index_model`, `configured_model`, `model_mismatch`, `last_updatedb`, `watcher`, `daemon`, ...) instead of `{file_count, chunk_count, db_path, model, model_found}`. See [AI-INTEGRATION.md](AI-INTEGRATION.md).
- `vec model download` prints commands built from `[embed] model_download_url` / `model_download_files` (it never downloaded anything).

### Added

- **Model and runtime versioning.** HTTP (Ollama) backend: the model identity's `sha256` is the model digest from `[embed] http_tags_path` (Ollama `/api/tags`), so a model re-pulled with other weights under the same tag is a model mismatch (an index without a digest adopts it at its next write). `Embedder::runtime()` (`ollama <version>` from `http_version_path`, `tract <version> (vec <version>)` for ONNX) is recorded as `runtime.first_write` / `runtime.last_write` and shown by `vec status`, `vec doctor` and MCP `index_status` (`runtime_first_write`, `runtime_last_write`, `canary`).
- **Embedding canary** (`[embed] canary_text`, `canary_warn_cosine`, `canary_min_cosine`): stored at the first write to an index (adopted, with a note, by an existing index at its next write). Writers re-embed it before writing, `vec watch` at start-up, reload and every flush, the daemon at start-up, `vec doctor` always. Below `canary_warn_cosine`: a warning naming the runtimes; below `canary_min_cosine`: writers refuse with a typed `vec_store::EmbeddingDrift` asking for `vec updatedb --full`, `vec watch` pauses indexing and retries with backoff, `vec doctor` fails. An unscoped `--full` re-embeds and replaces the canary when it completes. New keys `[embed] http_tags_path`, `http_version_path`, `http_default_tag`, `canary_*` ([CONFIG.md](CONFIG.md)).
- `Embedder::stub_with_seed` (tests: a stub whose vectors drift).
- `vec search` (explicit form), `--files-only`, `vec doctor`, `vec config show` (with provenance) / `vec config check [FILE]`, `vec status --daemon`, `--json` for `similar` and `explain`.
- `vec status` / MCP `index_status`: model identity, schema, last `updatedb` run, watcher state (pending, overflows, rescans, reloads, last success/error).
- MCP tools `similar` and `explain`; `search` takes `queries` and `exclude`; `index_status` takes `probe_daemon`.
- `exclude_dirs` / `exclude_files`: `"!pattern"` removes a default.
- Defaults in one embedded file (`crates/vec-core/defaults.toml`, also the source of `vec init`); config locations in `locations.toml`.
- Incremental speed-ups: size+mtime skip before reading; per-chunk vector reuse by text hash; real batching (`[embed] onnx_batch_size`); batched HTTP requests (`http_concurrency`).
- `vec watch`: `SIGHUP` reloads config and model (`ExecReload=` in the units).
- Daemon: thread per connection (`max_connections`), systemd socket activation (`vec-embed.socket`, system and user), `idle_exit_secs`, hardened units.
- New crate `vec-query`: one read path for the CLI and the MCP server.
- [FAILURE-ZONE.md](FAILURE-ZONE.md): behaviour on every failure, and recovery.
- **Workspace split** — 9 crates: vec-core, vec-embed, vec-store, vec-index, vec-watch, vec-daemon, vec-query, vec-mcp, vec-cli
- **New subcommands:** `vec context`, `vec similar`, `vec repl`, `vec gc`, `vec explain`, `vec diff`, `vec completions`
- **New flags:** `--json` (structured output), `--exclude` (filter directories)
- **Multi-query search** — `vec "auth" "error handling"` merges and deduplicates results
- **Path-weighted re-ranking** — file paths containing query keywords get a score boost (`[search] path_boost`)
- **MCP snippet trimming** — `search` tool returns ±`snippet_lines` around best-matching line instead of full chunk
- **Auto-detect userland config** — `~/.config/vec/config.toml` loaded automatically, no `--config` needed
- **Scores always shown** — every result includes `(score: 0.XXX)`
- **Snippet rework** — `--snippet` shows ±3 lines with line numbers and `>` marker (was full 40-line chunk)
- **Best-line targeting** — results point to the most relevant line within a chunk, not chunk start
- Parallel embedding during indexing via rayon — `vec updatedb` now uses all available CPU cores
- `[embed] index_threads` config option (0 = auto, default; set to N to limit threads)
- `vec gc` runs automatically after daily `vec-updatedb.service` via `ExecStartPost`
- Nix flake (`flake.nix`) for NixOS users
- Systemd user units for userland installs (`contrib/user/`)
- Claude Code `/vec` skill for AI assistant integration

### Fixed

- **Audit 2026-11-19:**
  - indexing: writing a file is one transaction (`replace_file`); `--full` no longer empties the index first (only on a real model change, right before the first successful write); the chunker no longer drops a file's tail or leaves gaps; per-file embedding failures keep the old entry, and a probe text tells a down backend (run stops, nothing pruned) from a bad file; `vec diff` uses the indexer's walker and filters; nested include paths are embedded once; non-UTF-8 file names are skipped and counted; relative `--path` is normalised;
  - symlink and race safety: the indexer opens files component by component with `O_NOFOLLOW` and checks device/inode against the walk, with `O_NONBLOCK` and a size cap; snippets, `context` and MCP reads never follow symlinks and are capped;
  - queries: no panics on stale offsets, shrunk files, UTF-8 boundaries or `context` past EOF; filters (path, excludes, readability) apply before the limit; the path filter respects directory boundaries (`/a` no longer matches `/abc`) and no longer mis-escapes GLOB characters; the store is opened read-only; the model is loaded once per process (repl, MCP); `vec … | head` exits quietly;
  - model identity checked by every reader and writer; queries never hash the model file (sha256 cache in `~/.cache/vec/model-sha256.toml`);
  - HTTP backend: status checked, chunked bodies decoded, non-numeric values rejected, connect timeout, URL path kept, no byte-slicing panics;
  - daemon: per-request deadlines (`io_timeout_ms`) against slow or trickling clients, separate client `reply_timeout_ms`, request/text limits, immediate "busy" reply, safe stale-socket removal, accept backoff, no internal paths in client errors; queries fall back to in-process embedding with one warning line;
  - `vec status` and MCP `index_status` no longer start a socket-activated daemon (`--daemon` / `probe_daemon` do);
  - schema upgrades wait up to `[database] migration_wait_ms` for a lock instead of failing;
  - `PrivateTmp=` removed from the indexer units (it made `/tmp` records disappear);
  - MCP server honours `--config`, the backend, the daemon and `path_boost`; `context` is restricted to indexed files; `similar`/`explain` require read access;
  - `vec init` escapes TOML strings; the private LAN address in a doc comment is gone.
- **`vec watch` unbounded memory growth and starvation** (#2). Root cause: every flush re-walked all of `include_paths` and checked every database row once per changed directory, and the model was reloaded per directory, while an unbounded channel queued every event. Now:
  - scoped re-indexes (`vec watch`, `vec updatedb --path`) walk only the changed paths and prune only records under them, in one transaction;
  - the watcher drops events for excluded, hidden, gitignored and database paths before queueing them, and de-duplicates them into a bounded set;
  - flushes happen after `[watch] debounce_ms` of quiet or at most `[watch] max_wait_ms` after the first change; indexing no longer blocks event intake;
  - the store and the embedder are opened once per flush.
- `[watch]` config section (`debounce_ms`, `max_wait_ms`, `max_pending`) replaces the hardcoded 3 s debounce.
- `exclude_dirs` matching semantics documented in CONFIG.md, `vec init` and `contrib/vec.conf`.
- **`vec watch` review fixes and correctness rework** (#3, #4):
  - the walker is the single source of truth: scoped re-indexes walk from the include path along the changed paths, so ancestor `.gitignore`/`exclude_dirs` rules apply exactly; pruning removes exactly the records the walk did not accept (deleted, newly excluded or ignored, binary, too large), never records under a directory that could not be read; the watcher's own `.gitignore` emulation is gone;
  - non-recursive watches, one per directory the walker enters, kept in sync by a watch-manager thread: excluded and ignored trees get no inotify watch; new/moved-in directories are watched and then re-indexed; deleted, moved-out and newly ignored ones lose their watch; `.gitignore`, `.ignore`, `.git` and `.git/info/exclude` changes re-decide their directory;
  - `Pending` collapse is amortised O(1) per event, never climbs above an include path (a collapsed path above one was silently dropped), and degrades to an O(1) overflow state that rescans the include paths; a kernel queue overflow also rescans;
  - a failed flush is re-queued with exponential backoff (`[watch] retry_max_ms`) instead of being lost; `[watch] startup_rescan` catches changes made while the watcher was stopped;
  - `vec status` shows the watcher's last flush, watched directories, watch failures and last error;
  - `vec --config <file> watch` now honours `--config` (from #4);
  - incremental `vec updatedb` prunes newly excluded files (no `--full` needed); a permission error is no longer mistaken for a deleted file;
  - `exclude_dirs` patterns are rooted at each include path: `"/builds"` and absolute paths inside an include path now match.
- **CI** (from #4): clippy fixes, and the aarch64 cross-compile job installs `g++-aarch64-linux-gnu` and sets `CXX_aarch64_unknown_linux_gnu` (`esaxx-rs` compiles C++); it had failed since March.
- **MCP server exposed no tools** — `vec serve` answered `tools/list` with an empty list and every `tools/call` with "method not found": the `ServerHandler` was never wired to the tool router (`#[tool_handler]` was missing). `search`, `context` and `index_status` now work; `serverInfo` reports `vec-mcp` instead of `rmcp`.
- **`system-sqlite` was a no-op** — the feature was empty and `rusqlite` was always built with `bundled`, so distro builds embedded their own SQLite and never got the distro's SQLite security updates. vec-store now has `bundled-sqlite` (default) and `system-sqlite`, exactly one of which must be enabled (a compile error explains otherwise); the distro recipe is `cargo build --release -p vec-cli --no-default-features --features system-sqlite` (PACKAGING.md, `contrib/debian/rules`, `contrib/vec.spec` updated). CI builds it and checks the binary links `libsqlite3.so`.
- `Config::load` layering is tested without reading the machine's `/etc/vec.conf` or `~/.config/vec/config.toml` (the test failed on any machine with a user config); crates inherit version/edition/MSRV/license from the workspace; unused `ndarray` dependency removed.
- **Dependency audit** — all RustSec vulnerabilities fixed:
  - `rusqlite` 0.31 → 0.43 (bundled SQLite, `libsqlite3-sys` 0.28 → 0.40, RUSTSEC-2026-0459);
  - `rmcp` 0.3 → 3.11 (RUSTSEC-2026-0462, RUSTSEC-2026-0189; both in HTTP transports vec does not use);
  - `tract-onnx` 0.21 → 0.22.5 (RUSTSEC-2026-0217; embeddings are bit-identical to 0.21); `time` 0.3.55 (RUSTSEC-2026-0009, held back by tract 0.21.19's `time < 0.3.42` pin);
  - `crossbeam-epoch`, `tar` patch updates; `anyhow`, `memmap2`, `rand`, `rustix`, `tokio` patch updates for unsoundness advisories.

- ONNX model loading with `gte-multilingual-base` — cleared `value_info` symbolic dims that tract couldn't parse
- Watcher feedback loop — events from the DB directory are now filtered out

### Changed

- **Minimum supported Rust version is now 1.89** (was 1.85), required by `rusqlite` ≥ 0.41 and `rmcp` 3. Debian 13 ships `rustc` 1.85: build with [rustup](https://rustup.rs) there.
- File read cache in search — no redundant I/O for best-line targeting and snippet display

---

## [0.1.0] — 2026-03-04

### Added

- `vec <query>` — semantic search over the indexed filesystem; returns ranked `file:line` results
- `vec updatedb` — incremental indexer: walks configured paths, chunks files (~40 lines, 10-line overlap), embeds via local ONNX, stores in SQLite
- `vec updatedb --full` — force full re-index, clearing stale entries
- `vec status` — DB stats, model info, last-indexed timestamp
- `vec watch` — inotify-based real-time re-indexer (debounced); runs as `vec-watch.service`
- `vec serve` — MCP server over stdio; exposes `search`, `context`, `index_status` tools to AI assistants (Claude Code, Cursor, Continue)
- `vec init` — print default `/etc/vec.conf` to stdout
- `vec daemon` — optional persistent embedding daemon over `/run/vec/embed.sock`; reduces per-query startup latency
- `vec model download` — print curl commands to fetch `gte-multilingual-base` ONNX + tokenizer
- Local ONNX inference via `tract-onnx` — pure Rust, no C deps, no network calls
- `gte-multilingual-base` int8 model support (~90 MB, 50+ languages, 768 dimensions)
- SQLite schema: `meta`, `files`, `chunks` — **no `content` column**; snippets read from live files at query time
- `access(path, R_OK)` check on every result — filesystem permissions are the sole ACL
- Hash-based incremental indexing (sha256): only changed files are re-embedded
- `.gitignore`-aware file walker via the `ignore` crate (ripgrep's engine)
- Model mismatch detection: exits with a clear error if DB model differs from configured model
- Paged cosine similarity search: O(page × dim + k) memory regardless of index size
- `/etc/vec.conf` TOML config with compiled-in defaults — no config file required
- `system-sqlite` Cargo feature for distro builds (links against system `libsqlite3`)
- `contrib/` packaging files: systemd units, sysctl config, RPM specs, Debian skeleton
- 79 unit tests; all run without network access, model files, or system path writes

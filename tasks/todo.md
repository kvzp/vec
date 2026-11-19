# Audit fix plan — branch `audit-fixes`

Source: [audit-2026-11-19.md](audit-2026-11-19.md) (full findings with file:line).
Rule reminder: no tunable, path, model name, timeout, URL or limit as a literal in Rust code. Values come from config; a missing value is an error.

## Phase A — foundation (vec-embed, vec-core, vec-store)

### vec-embed
- [x] `ModelIdentity { name, dim, sha256: Option<String> }`; `Embedder::identity()`, `dim()`, `is_stub()`; ONNX name = configured model name (not "tract-onnx")
- [x] Options structs from config: `OnnxOptions { max_tokens, .. }`, `HttpOptions { url, model, connect_timeout_ms, read_timeout_ms, concurrency, max_input_bytes, endpoint_path }` (removes MAX_CONCURRENT, MAX_BYTES, 300 s, "/api/embed")
- [x] Real batching: sequence length bucketed to the longest text in the batch (not always max_tokens); batch dimension > 1; output equal to the padded path within 1e-4 (test against the local model)
  - Done with symbolic B/S dims (one compiled plan). The 1e-4 target is impossible for gte-multilingual-base **int8**: it has 48 DynamicQuantizeLinear nodes, whose scale covers padding and the other rows, so any padding (even the old pad-to-128) moves vectors to cosine ~0.98. New key `[embed] onnx_batch_size` (default 1 = unpadded, deterministic, still 1.5x faster than the old path); batch=1 equals single-text within 1e-4 (tested), batch>1 asserted cosine > 0.95.
  - The old code read `outputs[0]` (`token_embeddings`) as "pre-pooled" for this model: 128-dim garbage. Output now chosen by name.
- [x] Map ONNX inputs by name, not position; pad id from tokenizer
- [x] HTTP: decode chunked on bytes, check status, reject non-numeric values, connect timeout, keep URL path, array input per request, no byte-slicing panics
- [x] Doc drift: module header, "dynamic batch" comment

### vec-core
- [x] Defaults live in one TOML file (`crates/vec-core/defaults.toml`, embedded with include_str!, also the source of `vec init` templates and contrib/vec.conf); no literal defaults in Rust
- [x] `deny_unknown_fields` on all raw config structs
- [x] Validation: chunk_size > 0, chunk_overlap < chunk_size, batch_size > 0, max_file_size >= min_file_size, db_path and include_paths absolute (after ~ expansion), model_search_path non-empty …
- [x] New keys: [embed] http_* options, [search] min_score, context_window, [database] busy_timeout_ms, search_page_size, [index] boundary_scan, boundary_patterns, [daemon] socket, socket_mode, max_request_bytes, io_timeout_ms, idle_exit_secs …
- [x] exclude_dirs / exclude_files: allow re-including a default (`!pattern`)
- [x] Config provenance (which layer set each value) for `vec config show`
- [x] `load_embedder` returns Result — no silent stub fallback
- [x] Remove private LAN address from config.rs doc comment
- [x] Fix error text `vec-model-gte` → `vec-model-base`; header comment about per-user config; resolve_model_path doc order
- [x] util::can_read doc (real vs effective uid)

### vec-store
- [x] Schema versioning via PRAGMA user_version + migrations (existing DBs upgrade in place)
- [x] `replace_file(..)` atomic per file (one transaction, hash written with chunks)
- [x] Model identity in meta: `ensure_model(&ModelIdentity)` set-or-check; called by every writer and checked by readers
  - Store side done (`ensure_model`, `check_model`, `reset_model`, `model`; `replace_file` refuses without an identity; search refuses a query of another dimension). vec-index calls `ensure_model` per walk and `reset_model` for `--full`. Readers calling `check_model` with the query embedder = Phase B2.
- [x] Reject blobs whose length != dim*4; `dot` checks lengths; skip non-finite scores
- [x] Store vectors normalized; score from blob without per-row allocation
- [x] Search: one read transaction, keyset paging (no OFFSET)
  - One streaming statement over one read snapshot instead of pages: same constant memory, O(N), and the path filter uses the index. So no `search_page_size` key.
- [x] Path filter: range scan with directory boundary (`/a` must not match `/abc`), no GLOB escaping bug
- [x] get_missing_file_paths uses is_gone; duplicate doc comment
- [x] busy_timeout and page size from config; read-only open for query paths
  - `StoreOptions { wal, busy_timeout_ms, read_only }` + `StoreOptions::from_config`; callers still open read-write (switching query paths = Phase B2).
- [x] files.size column (for mtime+size skip); chunk text hash column (for per-chunk embedding reuse) (`Store::embeddings_by_text_hash` for B1)
- [x] `upsert` RETURNING id (needs SQLite >= 3.35)

## Phase B1 — vec-index ‖ vec-watch + vec-daemon (parallel, disjoint crates)

### vec-index
- [x] Chunker: no dropped tail, no gaps when overlap < scan, every line covered (property test); snapped starts
  - Sparse windows (< min_chunk_lines) are merged into a neighbour instead of dropped, so every line is covered unless the whole file is too sparse.
- [x] Use `replace_file`; `ensure_model` before writing; per-file embedding failure = error count, old record kept
  - A failed batch is retried file by file; when no file of the batch embeds, the backend is down and the run stops (nothing pruned).
- [x] `--full` does not empty the index first (re-embed, then prune)
  - With a different model the old vectors cannot coexist (no staging table in the store): the index is emptied right before the first successful write, so a model that cannot embed leaves it intact. Scoped `--full` with another model is refused.
- [x] `vec diff` shares the exact walker/filters with updatedb; no unbounded reads; store errors propagate
  - `DiffResult.warnings` added. Records under directories the walk no longer enters (newly excluded/ignored) are reported only once gone: listing them needs a read-only "all indexed paths" API in vec-store.
- [x] Relative path filter normalised; warn when outside include paths
- [x] Symlink race: O_NOFOLLOW open, fstat, capped read (also O_NONBLOCK: a swapped-in FIFO cannot block the indexer)
- [x] `--full` nested include paths: no double embedding
- [x] Skip unchanged files by mtime+size before reading; per-chunk reuse by text hash
  - A touched file (same sha256) gets its size/mtime refreshed with all vectors reused. `IndexStats.chunks_reused` added.
- [x] Non-UTF-8 file names: skipped with a warning (counted: `IndexStats.files_skipped_non_utf8`)
- [x] Local rayon ThreadPool; backend as enum; BOUNDARY_SCAN/patterns from config (drop "pub impl ")
- [x] Hermetic tests; doc drift (header, comments)
  - IndexFilter now excludes the database file and its SQLite side files (not the whole database directory), matching the walker; fixes the relative-db_path "exclude everything" bug. ARCHITECTURE.md §Chunking left for Phase C.

### vec-watch + vec-daemon
- [x] Watch: embedder loaded once, failure = error at startup; flush failures retried
  - Startup also runs `check_model` against the index. A flush failure (store, or vec-index stopping on a down backend) is re-queued with backoff.
- [x] reconcile walk outside the mutex; counters atomic
  - The manager thread now owns the watches outright (no mutex at all); `vec status` counters are atomics.
- [x] Pending: inserting a dir prunes its descendants; reuse store in write_status
- [x] SIGHUP reloads config (include/exclude changes without restart)
  - Reloads config + embedder + filter as one runtime; a failed reload keeps the old one. Dropped include paths are re-indexed (pruned). `ExecReload=` in the units. Checked end to end with the real model.
- [x] Status extras: pending count, overflow/rescan counts, last flush time
  - New meta keys `watch.last_success`, `watch.pending`, `watch.overflows`, `watch.rescans`, `watch.reloads`. Showing them in `vec status` = Phase B2.
- [x] Daemon: thread per connection (bounded), read/write timeouts, socket mode/size limit from config, safe stale-socket removal, accept backoff, no debug error chain to clients, reject invalid UTF-8
- [x] Daemon protocol v2: handshake carries ModelIdentity, batch requests; client library `vec_daemon::client`
  - New key `[daemon] accept_retry_ms`. `vec` queries use the client, checked against the index's model (minimal change; the rework is B2).
- [x] Socket activation (systemd .socket, LISTEN_FDS) + idle exit; hardened service units
  - `systemd-analyze verify` clean (system and user); `security`: embed 0.9 SAFE, watch 2.3 OK. User units use only seccomp-based options (namespacing ones need unprivileged user namespaces). The debian/rpm packaging does not ship vec-embed.* (unchanged).
- [x] Tests: run_daemon lifecycle, stalled client, short body, bad UTF-8, failing embedder flush
  - Plus an ignored real-model test (daemon vectors = in-process within 1e-4), run once.
- [x] Daemon client: separate reply wait from stall timeout (new key `[daemon] reply_timeout_ms`, >= io_timeout_ms)
  - Root cause: the handshake reply of a socket-activated daemon waits for model load + sha256 (16.5 s in a debug build), but the client allowed only io_timeout_ms (10 s). The "after ~7 s" was a wall-clock step in WSL2 (date), not an early timeout: monotonic time was 10.15 s. Client reads now use whole-read deadlines (not per read(2)). Server timeouts only bound waits on the client, never inference. Eager start-up kept and documented (defaults.toml, contrib units).

## Phase B2 — vec-cli + vec-mcp
- [x] Shared snippet/best-line helper (one implementation, bounds-safe, char-boundary-safe)
  - `vec_core::snippet`. The rest of the read side (read-only store, model check, daemon-or-in-process embedding, filters, ranking, annotation, status) is one new crate, `vec-query`, used by both the CLI and MCP.
- [x] Fix panics: context past EOF, shrunk files, UTF-8 offsets, empty model_search_path
  - Also: a huge `--limit` no longer pre-allocates in `Store::search`; `vec … | head` exits quietly on SIGPIPE.
- [x] Relative paths canonicalised for similar/explain/--path/--exclude
  - Absolute + lexical `..` (`vec_core::util::absolute_lexical`), the form the walker stores (symlinks are not resolved, as in the walker).
- [x] Filter before truncating to limit
  - `Store::search_filtered` applies excludes/readability inside the top-k scan.
- [x] Daemon client via vec_daemon::client with timeouts + identity check; model loaded once per process (multi-query, repl)
  - Query paths open the store read-only; `[embed] model` ≠ index model name → error asking for `vec updatedb --full`; in-process embedder checked with `check_model` (name, dim, sha256).
- [x] `serve` honours --config; MCP keeps one embedder, honours backend/daemon, path_boost; spawn_blocking
- [x] MCP context restricted to indexed files, regular files, size cap
  - Also O_NOFOLLOW/O_NONBLOCK open (`vec_core::util::read_text_capped`); CLI `context` uses the same reader (not restricted to the index).
- [x] Features: `vec search` subcommand + `--files-only`, `vec doctor`, `vec config show|check`, status shows model identity / last run time / watcher state, MCP similar + explain tools, MCP multi-query + exclude
  - `run_updatedb` records `updatedb.last_run`; MCP `index_status` returns the same report as `vec status`; `vec similar|explain --json`. Doctor's optional stale-offset count not done.
- [x] repl: errors don't end session, honours min_score/snippets/path_boost
- [x] init escapes TOML strings; templates generated from defaults.toml
  - `vec_core::init`; userland paths and system-binary check from `locations.toml`; no `~` fallback. `contrib/vec.conf` stays hand-maintained (packaging choices), still checked as a valid layer.
- [x] `vec model download`: URLs/files from `[embed] model_download_url`/`model_download_files`; prints commands (no TLS client in the deps); help text matches.
- [x] `vec diff` lists records under newly excluded/ignored directories (`Store::for_each_indexed_path`, same stale rule as the prune).
- [x] New keys: `[search] path_boost_min_word_chars`, `max_queries`; path boost counts words below the include path, capped at 1.0.

## Review fixes — [review-2026-11-19.md](review-2026-11-19.md)
- [x] #1 Daemon: one deadline per whole request (and per reply write), recomputed before each read/write (`deadline::DeadlineStream`); a client beyond `max_connections` gets an immediate "busy" reply
- [x] #2 `[daemon] max_texts_per_request` (server rejects, client splits by count)
- [x] #3 `PrivateTmp=` removed from the system watch and updatedb units (the user units never had it)
- [x] #4 Indexer opens each component below the include path with `openat(O_NOFOLLOW)` and checks `(st_dev, st_ino)` against the walker's `lstat`; tests swap a directory for a symlink
- [x] #5/#8 Typed `vec_store::ModelMismatch`; `--full` resets only on it; `replace_file(&ModelIdentity, ..)` checks inside its transaction
- [x] #6 Probe text before declaring the backend down
- [x] #7 vec-query falls back in-process with a one-line warning (once per `Searcher`)
- [x] #9 `vec_core::model_sha`: readers never hash (sha only from the cache), writers use/fill the cache (`locations.toml` `user_sha_cache`)
- [x] #10 `vec status` / MCP `index_status` do not probe the daemon by default (`--daemon` / `probe_daemon`); `vec doctor` probes and says so
- [x] #11 Schema upgrade retries the lock up to `[database] migration_wait_ms`, one message; "run `vec updatedb` once after upgrade" documented
- [x] #12 `similar` / `explain` require read access
- [x] #13 `[embed] daemon_socket` mapped to `[daemon] socket` with a warning; validation errors name the file(s) that set the key
- [x] #14 `[embed] http_error_preview_chars`; `OUTPUTS` / `PAD_TOKENS` documented as model-format constants
- [x] #15 Removed `Store::get_missing_file_paths`, `delete_all_files`, `delete_missing_files_under`, `Embedder::is_stub`; `resolve_path_filter` uses `absolute_lexical`. `TAKEN` kept (soundness guard, documented)
- [x] #16 `updatedb --full` help text; #17 `snippet_lines` documented as a radius

## Phase C — docs and verification
- [x] README, ARCHITECTURE, CONFIG, AI-INTEGRATION, SECURITY, TESTING, PACKAGING, CHANGELOG, BUGS, TODO in sync with code
  - Also new FAILURE-ZONE.md; CONTRIBUTING, PROJECT rewritten; debian/rpm ship `vec-embed.{socket,service}` disabled and the user units as docs; `contrib/vec.spec` license fixed (MIT, `LICENSE`); `Documentation=` of the embed units points at CONFIG.md (no man page exists).
- [ ] cargo fmt, clippy -D warnings, all tests (daemon tests outside sandbox), end-to-end run with the real model
- [ ] Review section below

## Review

- All audit findings (tasks/audit-2026-11-19.md) and all 17 findings of the independent review (tasks/review-2026-11-19.md) fixed; one item kept on purpose (activation.rs TAKEN guard, documented).
- Extra bug found on the way: the old ONNX path used the `token_embeddings` output instead of the pooled `sentence_embedding`, so existing indexes hold wrong (128-dim) vectors → upgrade needs `vec updatedb --full` (CHANGELOG "Breaking").
- Features added: `vec search` + `--files-only`, `vec doctor`, `vec config show|check`, richer `vec status` (`--daemon`), MCP similar/explain/multi-query/exclude, daemon protocol v2 (identity handshake, batches), socket activation + idle exit, SIGHUP reload of `vec watch`, chunk-level embedding reuse, size+mtime skip, new crate vec-query (shared read side).
- Verification: fmt + clippy -D warnings clean; 254 tests pass, 6 real-model tests pass with VEC_TEST_MODEL (Unix-socket tests run outside the sandbox); release-build end-to-end run on the real model (index, search, files-only, similar with relative path, chunk reuse 60/61, context past EOF, doctor, config check).
- Remaining limitations: BUGS.md; open ideas: TODO.md. Failure behaviour: FAILURE-ZONE.md.

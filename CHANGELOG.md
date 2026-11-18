# Changelog

All notable changes to this project will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning follows [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

### Added

- **Workspace split** — 8 crates: vec-core, vec-embed, vec-store, vec-index, vec-watch, vec-daemon, vec-mcp, vec-cli
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

- **`vec watch` unbounded memory growth and starvation** (#2). Root cause: every flush re-walked all of `include_paths` and checked every database row once per changed directory, and the model was reloaded per directory, while an unbounded channel queued every event. Now:
  - scoped re-indexes (`vec watch`, `vec updatedb --path`) walk only the changed paths and prune only records under them, in one transaction;
  - the watcher drops events for excluded, hidden, gitignored and database paths before queueing them, and de-duplicates them into a bounded set;
  - flushes happen after `[watch] debounce_ms` of quiet or at most `[watch] max_wait_ms` after the first change; indexing no longer blocks event intake;
  - the store and the embedder are opened once per flush.
- `[watch]` config section (`debounce_ms`, `max_wait_ms`, `max_pending`) replaces the hardcoded 3 s debounce.
- `exclude_dirs` matching semantics documented in CONFIG.md, `vec init` and `contrib/vec.conf`.

- ONNX model loading with `gte-multilingual-base` — cleared `value_info` symbolic dims that tract couldn't parse
- Watcher feedback loop — events from the DB directory are now filtered out

### Changed

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

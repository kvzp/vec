# Changelog

All notable changes to this project will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning follows [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

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

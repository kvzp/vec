# TODO

## Usability

- [x] **Auto-detect userland config** — `~/.config/vec/config.toml` is now loaded automatically after `/etc/vec.conf` (userland overrides system). `--config` flag still works as highest priority override.
- [x] **Show scores by default** — every result now shows `file:line (score: 0.XXX)` without needing any flag.
- [x] **`vec similar <file:line>`** — find code similar to a given chunk using stored embeddings, no model load needed.
- [x] **`--exclude <dir>`** — filter out results from specific directories at query time.
- [x] **`vec repl`** — interactive mode that loads the model once and accepts queries in a loop.
- [x] **JSON output (`--json`)** — structured JSON output for programmatic consumption.
- [x] **`vec diff`** — show files changed since last index (new, modified, deleted).
- [x] **Multi-query search** — `vec "auth" "error handling"` merges and deduplicates results.
- [x] **Shell completion** — `vec completions bash/zsh/fish` generates shell completions.
- [x] **`vec context <file:line>`** — show source lines around a location. Enables `fzf` preview mode.

## Index Quality

- [ ] **Language-aware chunking (treesitter)** — deferred: adds C compilation dependency, conflicts with "pure Rust, no C deps" principle. Current boundary-pattern chunking works well enough. Revisit when quality complaints arise.
- [x] **Path-weighted re-ranking** — boost results where the file path matches query keywords. Configurable via `[search] path_boost` (default: 0.05).
- [ ] **Index compression** — deferred: current DB is ~108 MB for 27k chunks, manageable. Revisit when DB exceeds 1 GB.

## Performance

- [x] **Cache file reads** — each result file is read once for best-line targeting and snippet display.
- [x] **Skip unchanged files without reading** (size+mtime), per-chunk vector reuse, real ONNX batching.
- [ ] **int8 / f16 vector storage** — 4x / 2x smaller index and faster scans; needs a schema version and a quality check against f32.
- [ ] **Measure and document** indexing and query speed (release build, reference machine) before quoting numbers.

## MCP / AI Integration

- [x] **Apply snippet and best-line targeting to MCP server** — MCP `search` tool now returns ±`snippet_lines` around the best-matching line with line numbers and `>` marker.

## Operational

- [x] **`vec gc`** — garbage collect orphaned entries and vacuum the database.
- [x] **Watcher cleanup on delete** — deletions, moves out and newly excluded/ignored paths are pruned by the scoped re-index (the walker decides); covered by `scoped_update_prunes_deleted_file_and_dedups_nested_roots`, `incremental_updatedb_prunes_newly_excluded_files` and the watcher tests.
- [x] **`vec explain <file:line>`** — show which chunk(s) cover a given line and their stats.
- [x] **`vec doctor`**, **`vec config show|check`**, `vec status` with model identity, last run and watcher state.
- [x] **Socket-activated daemon** with idle exit and hardened units.
- [ ] **Stale-offset count in `vec doctor`** — files whose size/mtime changed since indexing (stale snippets).
- [ ] **`--full` staging swap** — build a model change into a staging table and swap at the end, so an interrupted rebuild keeps the old index searchable.
- [ ] **Non-zero exit (or a flag) for `vec updatedb` when files failed**, for scripts and timers.
- [ ] **Model download over HTTPS** in-process (`vec model download` only prints `curl` commands; needs a TLS client dependency), with checksum verification.
- [ ] **HTTPS for the HTTP embedding backend.**

## Architecture

- [x] **Split into crates** — 9 workspace crates (vec-query shares the read side between CLI and MCP).

## Distribution

- [x] **Nix flake** — `flake.nix` for NixOS users.
- [ ] **Homebrew formula** — deferred: macOS is untested, no hardware available.
- [ ] **Writers coordinate full walks** — `vec watch`'s startup/reload rescan and a running `vec updatedb` walk the same tree and embed the same changed files twice (seen 2026-11-19 on a large index: half the Ollama throughput wasted). Take an advisory lock (or a meta flag) for whole-tree walks; the watch defers its rescan while another writer holds it.

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

- [x] **Cache file reads** — `cmd_search` caches file content in a HashMap, shared between best-line targeting and snippet display. No redundant I/O.

## MCP / AI Integration

- [x] **Apply snippet and best-line targeting to MCP server** — MCP `search` tool now returns ±`snippet_lines` around the best-matching line with line numbers and `>` marker.

## Operational

- [x] **`vec gc`** — garbage collect orphaned entries and vacuum the database.
- [ ] **Watcher cleanup on delete** — verify that the watcher notices file deletions and removes stale entries from the index. Currently `run_updatedb` prunes missing files, but this should be confirmed end-to-end for inotify `Remove` events.
- [x] **`vec explain <file:line>`** — show which chunk(s) cover a given line and their stats.

## Architecture

- [x] **Split into crates** — 8 workspace crates: vec-core, vec-embed, vec-store, vec-index, vec-watch, vec-daemon, vec-mcp, vec-cli.

## Distribution

- [x] **Nix flake** — `flake.nix` for NixOS users.
- [ ] **Homebrew formula** — deferred: macOS is untested, no hardware available.

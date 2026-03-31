# TODO

## Usability

- [x] **Auto-detect userland config** — `~/.config/vec/config.toml` is now loaded automatically after `/etc/vec.conf` (userland overrides system). `--config` flag still works as highest priority override.
- [x] **Show scores by default** — every result now shows `file:line (score: 0.XXX)` without needing any flag.

- [ ] **Multi-query search** — accept multiple queries in a single invocation (e.g. `vec "auth middleware" "error handling" "database pooling"`). Embed all queries in one batch, run each against the index, and return combined/deduplicated results. Saves model load time and reduces round-trips — especially useful for AI agents that need to explore several concepts at once.

## Performance

- [ ] **Cache file reads in `best_line_in_chunk`** — currently re-reads each result file from disk even in plain `file:line` mode (no snippet). For 10 results that's 10 redundant reads just to refine line numbers. Either cache reads across results or skip best-line targeting when `--snippet` is not used.

## MCP / AI Integration

- [ ] **Apply snippet and best-line targeting to MCP server** — the `search` tool still returns full chunk text (~40 lines per result). AI agents are the most token-sensitive consumers. Apply the same ±`snippet_lines` windowing and best-line targeting to MCP results.

## Architecture

- [ ] **Split into crates** — the single `src/` directory mixes embedding, indexing, storage, config, CLI, MCP server, daemon, and watcher into one compilation unit. Split into workspace crates for faster incremental builds, clearer dependency boundaries, and reusability:
  - `vec-core` — config, store, embedder, chunker (the library)
  - `vec-cli` — clap CLI, `cmd_search`, `cmd_init`, progress output
  - `vec-mcp` — MCP server (`vec serve`)
  - `vec-daemon` — embedding daemon (`vec daemon`)
  - `vec-watch` — inotify watcher (`vec watch`)

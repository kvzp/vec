# TODO

## Usability

- [ ] **Auto-detect userland config** — fall back to `~/.config/vec/config.toml` when `/etc/vec.conf` doesn't exist, eliminating the need for `--config` on every command. This simplifies the skill, systemd units, shell aliases, and the entire userland experience.
- [ ] **Add `--score` flag or always show scores** — non-snippet output (`file:line`) gives no indication of match confidence. A 0.95 hit and a 0.72 hit look identical. Either add a `--score` flag or append `(score: 0.XXX)` by default.

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

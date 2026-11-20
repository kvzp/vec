# vec

[![Release](https://img.shields.io/github/v/release/kvzp/vec?sort=semver)](https://github.com/kvzp/vec/releases) [![CI](https://github.com/kvzp/vec/actions/workflows/ci.yml/badge.svg)](https://github.com/kvzp/vec/actions/workflows/ci.yml)

**`locate` finds files by name. `vec` finds them by meaning.**

Search for "where do we retry failed uploads" and get the file and line, even when
none of those words appear in the code. vec indexes your files with a local
embedding model, answers in a terminal or as an MCP tool for Claude Code and
other AI agents, and never sends your text anywhere. Results respect Unix file
permissions: each user only ever sees files they can read.

## See it

```console
$ vec "upgrade an old database schema in place" --limit 2 --snippet
~/src/vec/crates/vec-store/src/lib.rs:918 (score: 0.760)
    915:     }
    916:     tx.pragma_update(None, "user_version", SCHEMA_VERSION)
    917:         .context("writing the schema version")?;
>   918:     tx.commit().context("committing the schema upgrade")
    919: }

~/src/vec/crates/vec-store/src/lib.rs:883 (score: 0.672)
    880:             Err(e) if is_busy(&e) && started.elapsed() < wait => {
    881:                 if !told {
    882:                     anstream::eprintln!(
>   883:                         "vec: the index at {} needs a schema upgrade and another process holds \
    884:                          its lock (probably upgrading it); waiting up to {} s \
```

(Real output on vec's own source; paths shortened, second snippet cut. The
snippet radius is `[search] snippet_lines`, default 3.) Without `--snippet` you get one
`file:line (score: …)` per result, ready for `xargs`, `fzf` or your editor.

## Why vec

- **vs grep / ripgrep:** they match strings; vec matches intent. Use rg when you
  know the identifier, vec when you know what the code *does*. Multi-query
  (`vec "jwt" "session expiry"`) and path filters narrow it down.
- **vs cloud code search and hosted embeddings:** the default backend runs an ONNX
  model in-process on the CPU (pure Rust, tract). No account, no network, no text
  leaving the machine. The index stores vectors and byte offsets, **never file
  text**; snippets are read from the live file.
- **Permission-aware:** one system-wide index serves every user; each result is
  checked with `access(R_OK)` for the user asking, so nobody sees a file they
  cannot read.
- **Cheap to keep fresh:** unchanged files are skipped by size and mtime, then
  sha256; inside a changed file only chunks whose text changed are re-embedded.
  `vec watch` (inotify) re-indexes within seconds of a save.
- **Built for AI agents:** `vec serve` is an MCP server (search, similar,
  explain, context, index status) for Claude Code, Cursor, Continue and any MCP
  client; a Claude Code skill is included too.
- **Built like system software:** one self-contained Rust binary (SQLite
  bundled, or the system one for distro builds), systemd units with sandboxing
  (capability-limited indexer, `DynamicUser` daemon without network), an
  optional socket-activated daemon that keeps the model loaded, `vec doctor` when
  something is off.

## Quickstart

```bash
cargo install --git https://github.com/kvzp/vec vec-cli     # or build from source, see Install
mkdir -p ~/.config/vec && vec init --user > ~/.config/vec/config.toml   # index your home directory
vec model download | sh                                     # gte-multilingual-base, ~90 MB
vec updatedb                                                # build the index (first run takes a while)
vec "where do we retry failed uploads"
claude mcp add vec -- vec serve                             # give Claude Code the tools
```

For a system-wide index shared by all users, see [Install](#install).

> **Status:** active development, not yet in distro repositories ([PACKAGING.md](PACKAGING.md)).
> Tested on Linux x86_64 only; macOS and Windows are untested ([BUGS.md](BUGS.md)).

## Install

**Release binary** (Linux x86_64 and aarch64; the aarch64 build is cross-compiled and untested). Every [release](https://github.com/kvzp/vec/releases) lists its changes and checksums:

```bash
curl -LO https://github.com/kvzp/vec/releases/latest/download/vec-linux-x86_64.tar.gz
curl -LO https://github.com/kvzp/vec/releases/latest/download/vec-linux-x86_64.tar.gz.sha256
sha256sum -c vec-linux-x86_64.tar.gz.sha256 && tar xzf vec-linux-x86_64.tar.gz
sudo install -m755 vec /usr/local/bin/
```

**From source** — requirements: Rust 1.89+. SQLite is bundled by default. `main` is the development line; a version branch (`git checkout 0.1`) is the stable one ([CONTRIBUTING.md](CONTRIBUTING.md#versions-and-branches)).

```bash
git clone https://github.com/kvzp/vec && cd vec
cargo build --release
sudo install -m755 target/release/vec /usr/local/bin/
```

### System-wide (central index, all users)

```bash
vec model download | sudo sh              # gte-multilingual-base into /usr/share/vec/models (~90 MB)
vec init | sudo tee /etc/vec.conf         # optional: every key, commented out
sudo vec updatedb                         # build /var/lib/vec/vec.db
```

Units in `contrib/` keep it fresh (packages install them, see [PACKAGING.md](PACKAGING.md)):

| Unit | Purpose |
|------|---------|
| `vec-updatedb.timer` / `.service` | daily `vec updatedb` + `vec gc` |
| `vec-watch.service` | real-time watcher; `systemctl reload` re-reads the config |
| `vec-embed.socket` / `.service` | **optional** embedding daemon, started on the first query |

```bash
sudo systemctl enable --now vec-updatedb.timer vec-watch.service
sudo systemctl enable --now vec-embed.socket   # optional: faster queries, 150–300 MB while running
```

### Userland (no root)

Only when no system-wide vec exists (`vec init --user` warns otherwise).

```bash
install -m755 target/release/vec ~/.local/bin/
mkdir -p ~/.config/vec && vec init --user > ~/.config/vec/config.toml   # data under ~/.local/share/vec
vec model download | sh
vec updatedb
```

The per-user config is picked up automatically. User units live in
`contrib/user/` (same names; the socket is `~/.local/share/vec/embed.sock`, mode 0600):

```bash
mkdir -p ~/.config/systemd/user && cp contrib/user/* ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now vec-updatedb.timer vec-watch.service
systemctl --user enable --now vec-embed.socket   # optional
```

### Upgrading

Indexes built by earlier versions hold **wrong vectors**: the old code read the
model's `token_embeddings` output instead of the pooled `sentence_embedding`
([CHANGELOG.md](CHANGELOG.md)). After installing this version, before restarting
services, run once as the index owner:

```bash
sudo vec updatedb --full
```

It upgrades the schema in place (one transaction; other processes wait up to
`[database] migration_wait_ms`), then rebuilds the vectors. Until then queries
fail with a "model mismatch" or "schema version" error. Move any
`[embed] daemon_socket` setting to `[daemon] socket` (still accepted, with a
warning, for one release). Restart a running `vec daemon`: protocol v1 daemons
are not understood (queries fall back to in-process embedding with a warning).

The index now also records the embedding runtime (`ollama <version>` or
`tract <version>`) and an **embedding canary**; an existing index records them at
its next write (a `note:` line says so). After upgrading Ollama, re-pulling a
model or upgrading vec, run `vec doctor`: when the canary reports drift,
writers (including `vec watch`) stop indexing until `sudo vec updatedb --full`
([FAILURE-ZONE.md](FAILURE-ZONE.md#model)).

## Usage

```bash
# Search
vec "auth logic"                          # file:line (score), best first
vec search "diff"                         # explicit form, for a query that is a subcommand name
vec "auth" "jwt validation"               # several queries, merged (at most [search] max_queries)
vec "auth" --limit 5 --min-score 0.5
vec "auth" --path ~/src/api --exclude ~/src/api/vendor   # relative paths work too
vec "auth" --snippet                      # lines around each result's best line
vec "auth" --files-only | xargs grep -n token
vec "auth" --json
vec repl --snippet                        # model loaded once, one query per line (same options)

# Around a result
vec context src/auth.rs:42 --window 5     # lines around file:line
vec similar src/auth.rs:42 [--json]       # chunks similar to that one (stored vector, no model)
vec explain src/auth.rs:42 [--json]       # chunks covering the line, indexed hash, changed since?

# Index
vec updatedb                              # incremental
vec updatedb --path ~/src                 # only under a path
vec updatedb --full                       # re-embed everything (required after a model change)
vec diff                                  # what updatedb would add / change / remove
vec gc                                    # prune deleted files, VACUUM

# Health
vec status                                # index, model identity, last run, watcher, daemon socket
vec status --daemon                       # also ask the daemon (starts a socket-activated one)
vec doctor                                # check config, model, index, daemon; exit 1 on a problem
vec config show                           # effective config and the layer that set each key
vec config check my.toml                  # validate a file; exit 1 on error

# Services and setup
vec watch                                 # real-time watcher (normally vec-watch.service)
vec daemon                                # embedding daemon (normally vec-embed.socket)
vec serve                                 # MCP server on stdio
vec init [--user]                         # starter config
vec model download                        # print the commands that fetch the model
vec completions bash > ~/.local/share/bash-completion/completions/vec
```

Every subcommand takes `--config <file>` (applied last, instead of the per-user file).

## Output

```
/home/user/backend/src/auth/middleware.rs:42 (score: 0.943)
/home/user/backend/src/auth/jwt.rs:17 (score: 0.891)
```

The line is the chunk's line that best matches the query words. `--snippet`
adds `[search] snippet_lines` lines (default 3) on each side, read from the live
file, with `>` marking the line. Works with `fzf`:
`vec "auth" | fzf --preview 'vec context {1}'`.

## How it works

1. `vec updatedb` walks `include_paths` (gitignore-aware), splits files into
   overlapping ~40-line chunks, embeds them, and stores vectors + byte offsets.
2. A query is embedded (by the daemon if one serves the index's model, otherwise
   in-process), scored against every stored vector (cosine), filtered by path and
   by `access(R_OK)`, then annotated from the live file.

Details: [ARCHITECTURE.md](ARCHITECTURE.md).

## Documentation

| | |
|-|-|
| [CONFIG.md](CONFIG.md) | every config key, layers, recipes |
| [ARCHITECTURE.md](ARCHITECTURE.md) | crates, data flow, schema, chunker, daemon protocol |
| [SECURITY.md](SECURITY.md) | threat model |
| [FAILURE-ZONE.md](FAILURE-ZONE.md) | what vec does when things go wrong, and how to recover |
| [AI-INTEGRATION.md](AI-INTEGRATION.md) | MCP server and Claude Code skill |
| [PACKAGING.md](PACKAGING.md) | for distro packagers |
| [CONTRIBUTING.md](CONTRIBUTING.md), [TESTING.md](TESTING.md) | development |
| [CHANGELOG.md](CHANGELOG.md), [BUGS.md](BUGS.md), [TODO.md](TODO.md) | history, limitations, backlog |

## License

MIT ([LICENSE](LICENSE)).

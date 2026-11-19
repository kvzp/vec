# Contributing

## Dev Setup

```bash
git clone https://github.com/kvzp/vec
cd vec
cargo build
```

No Python, no protoc, no npm. Rust plus a C/C++ compiler (bundled SQLite, `tokenizers`).

### Getting a model for development

```bash
vec model download        # prints the mkdir/curl commands; pipe to sh (sudo for /usr/share)
```

Or point `[embed] model` at an absolute `.onnx` path with `tokenizer.json` beside it.

### system-sqlite feature (links against system libsqlite3)

```bash
# Debian/Ubuntu
apt install libsqlite3-dev

# Fedora
dnf install sqlite-devel

cargo build -p vec-cli --no-default-features --features system-sqlite
```

No other build-time dependencies — no protobuf-compiler, no Python, no HuggingFace tooling.

### Cross-compilation (x86_64 → aarch64)

The release builds target both `x86_64-unknown-linux-gnu` and
`aarch64-unknown-linux-gnu`. To build the aarch64 binary locally on an x86_64
machine:

```bash
# Install the cross-compiler (Debian/Ubuntu)
sudo apt-get install gcc-aarch64-linux-gnu g++-aarch64-linux-gnu

# Add the Rust target
rustup target add aarch64-unknown-linux-gnu

# Build
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
cargo build --release --target aarch64-unknown-linux-gnu
```

The bundled SQLite (C) and `esaxx-rs` (C++, via `tokenizers`) need the aarch64
C and C++ compilers.

Output: `target/aarch64-unknown-linux-gnu/release/vec`

The CI release workflow (`.github/workflows/release.yml`) does this
automatically on version tags and uploads both binaries as GitHub release
assets.

---

## Running tests

See [TESTING.md](TESTING.md). In short: `cargo test`, `cargo clippy -- -D warnings`,
`cargo fmt --check`; real-model tests with
`VEC_TEST_MODEL=/path/model_int8.onnx cargo test -- --ignored`.

---

## Project structure

```
crates/
  vec-embed/    embedders (ONNX via tract, Ollama HTTP, stub), ModelIdentity
  vec-core/     config (defaults.toml, locations.toml), init templates, snippet, sha cache, util
  vec-store/    SQLite schema + migrations, model identity, atomic writes, search
  vec-index/    walker, chunker, incremental indexer, diff
  vec-watch/    inotify watcher (vec watch)
  vec-daemon/   embedding daemon + client, protocol v2, socket activation
  vec-query/    read side shared by CLI and MCP
  vec-mcp/      MCP server (vec serve)
  vec-cli/      clap CLI (src/commands/: search, index, status, setup, services)
contrib/        systemd units (system + user), sysctl, vec.conf, RPM/Debian packaging
```

Docs: README, CONFIG, ARCHITECTURE, SECURITY, FAILURE-ZONE, AI-INTEGRATION,
PACKAGING, TESTING, CHANGELOG, BUGS, TODO, PROJECT.

---

## Key invariants (don't break these)

1. **No source text in the DB.** `chunks` has no content column; snippets are read
   from the live file through `byte_offset..byte_end`.
2. **Filesystem is the ACL.** `access(path, R_OK)` on every result before display,
   before the limit. No separate permission system.
3. **No literal defaults in code.** Tunables go in `crates/vec-core/defaults.toml`
   (and CONFIG.md), file locations in `locations.toml`. A missing value is an error,
   never a guessed fallback. New config keys need validation where a value can be wrong.
4. **One model per index.** Writers call `Store::ensure_model`; readers check the
   identity; vectors of two models are never mixed.
5. **Atomic per file, exact pruning.** A file's record and chunks change in one
   transaction (`replace_file`); prune only after a completed walk, and only what
   the walker did not accept. `vec watch` must never re-implement walker rules.
6. **Incremental by default**: size+mtime, then sha256, then per-chunk text hash.
   Full re-embedding only with `--full`.
7. **Never read through a symlink** on the index or display path (`O_NOFOLLOW`,
   regular files, capped reads).
8. **Bounded everything** in the daemon: request size, text count, connections,
   deadlines.

---

## Adding file-type support

Chunk boundaries come from `[index] boundary_patterns` (line prefixes, after
indentation) and `[index] boundary_scan` in `defaults.toml`. Add patterns there,
document them in CONFIG.md if the meaning changes, and check the chunks of a real
file with `vec explain file:line`.

---

## Submitting changes

- One logical change per PR; `cargo test`, clippy and fmt must pass.
- Update the docs your change touches: ARCHITECTURE (data flow, components),
  SECURITY (DB, permissions, daemon), CONFIG (keys), FAILURE-ZONE (error
  behaviour), CHANGELOG (`Unreleased`).
- Packaging changes: build with `-p vec-cli --no-default-features --features system-sqlite`.

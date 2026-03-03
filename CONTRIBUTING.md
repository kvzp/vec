# Contributing

## Dev Setup

```bash
git clone https://github.com/kvzp/vec
cd vec
cargo build
```

No Python, no protoc, no npm. Pure Rust.

### Getting a model for development

Download `gte-multilingual-base` int8 ONNX and place it under `/usr/share/vec/models/`:

```bash
vec model download   # prints exact curl commands to run
```

Or place the ONNX file + `tokenizer.json` manually in `/usr/share/vec/models/gte-multilingual-base/`.

### system-sqlite feature (links against system libsqlite3)

```bash
# Debian/Ubuntu
apt install libsqlite3-dev

# Fedora
dnf install sqlite-devel

cargo build --features system-sqlite
```

No other build-time dependencies — no protobuf-compiler, no Python, no HuggingFace tooling.

### Cross-compilation (x86_64 → aarch64)

The release builds target both `x86_64-unknown-linux-gnu` and
`aarch64-unknown-linux-gnu`. To build the aarch64 binary locally on an x86_64
machine:

```bash
# Install the cross-compiler (Debian/Ubuntu)
sudo apt-get install gcc-aarch64-linux-gnu

# Add the Rust target
rustup target add aarch64-unknown-linux-gnu

# Build
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
cargo build --release --target aarch64-unknown-linux-gnu
```

The `rusqlite` bundled feature compiles SQLite from C, which requires the
aarch64 C compiler (`CC_aarch64_unknown_linux_gnu`). The rest of the crate is
pure Rust and cross-compiles without extra tooling.

Output: `target/aarch64-unknown-linux-gnu/release/vec`

The CI release workflow (`.github/workflows/release.yml`) does this
automatically on version tags and uploads both binaries as GitHub release
assets.

---

## Running Tests

```bash
cargo test                          # all unit tests
cargo test store                    # tests in store.rs
cargo test -- --nocapture           # show println output
cargo test --features integration   # integration tests (requires model files present)
```

Tests that require the embedding model on disk are gated behind `#[cfg(feature = "integration")]` and skipped by default.

---

## Project Structure

```
vec/
├── src/
│   ├── main.rs         # CLI entry point (clap)
│   ├── config.rs       # TOML config, compiled-in defaults
│   ├── embed.rs        # tract-onnx local ONNX inference
│   ├── store.rs        # SQLite read/write, cosine similarity, access()
│   ├── index.rs        # File walker, chunker, incremental hash check
│   ├── watch.rs        # inotify watcher (vec watch subcommand)
│   ├── mcp.rs          # MCP server (vec serve)
│   └── util.rs         # Shared helpers
├── contrib/            # systemd units, sysctl, distro packaging
├── Cargo.toml
├── README.md
├── ARCHITECTURE.md
└── SECURITY.md
```

---

## Key Invariants (don't break these)

1. **No source text in the DB.** `chunks` table has no `content` column. Snippets are always read from the live file using `byte_offset..byte_end`.

2. **Filesystem is the ACL.** `access(path, R_OK)` is called on every query result before display — drop silently if denied. Don't add a separate permission system.

3. **Gitignore-aware.** The indexer must use the `ignore` crate (ripgrep's engine) to respect `.gitignore` at every directory level.

4. **Incremental by default.** `vec updatedb` must skip files whose sha256 hash hasn't changed. Full re-index only on `--full`.

5. **No runtime dependencies in release binary.** The default build bundles SQLite (`rusqlite` bundled feature). The `system-sqlite` feature is for distro packagers who control the environment.

6. **Model mismatch detection.** If `meta.model_name` in the DB differs from the configured model, vec must exit with a clear error rather than silently return wrong results.

---

## Adding File Type Support

Boundary detection lives in `src/index.rs: BOUNDARY_PATTERNS`. It's a flat
`&[&str]` of line-start strings applied to every file type. vec is
general-purpose — it indexes any text files, not just source code.

```rust
const BOUNDARY_PATTERNS: &[&str] = &[
    "fn ", "pub fn ", "async fn ", "pub async fn ",
    "impl ", "pub struct ", "pub enum ",
    "def ", "class ", "func ", "function ",
    // add yours here
];
```

Verify manually that chunks look reasonable on a real file in that language.

---

## Submitting Changes

- One logical change per PR
- Update `ARCHITECTURE.md` if you change data flow or add a component
- Update `SECURITY.md` if you change anything touching the DB or permissions
- `cargo test` must pass
- If adding distro packaging support, test the build with `--features system-sqlite`

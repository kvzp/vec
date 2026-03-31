# vec — Architecture

## Core Design Decisions

### 1. No source text in the database

The DB stores `(path, byte_offset, byte_end, embedding)` — never the file text itself.

**Why:** A shared semantic index creates a new attack surface. If code is embedded in the DB, whoever reads the DB reads your source. By storing only byte offsets, the DB is inert without the source files. Filesystem permissions remain the single source of truth for access control.

**Consequence:** Snippets are always read from the live source file at query time. This also means snippets are always fresh — no stale cache to invalidate.

### 2. Filesystem is the security boundary

Every query re-validates each result with `access(path, R_OK)` as the requesting user before returning it. There is no separate ACL, no mode distinction, no daemon required for this property. If your filesystem permissions change after indexing, the next query reflects that immediately — stale results are silently dropped.

### 3. `locate` mental model, not a search engine

`vec` is a pre-indexed lookup tool, not a query engine. The heavy work (embedding) happens at index time. Query time is a dot product over float32 vectors — sub-second regardless of codebase size.

### 4. Static binary, system-packaged

`vec` produces a single static binary. No Python runtime, no npm, no pip. Distro packages link against system libsqlite3 via the `system-sqlite` Cargo feature. This makes vec packageable by Debian, Fedora, Arch, and NixOS without modification to their standard packaging workflows.

---

## Components

Cargo workspace with 8 crates:

```
crates/
├── vec-core/     # Config (TOML, path resolution), util (access() check), load_embedder()
├── vec-embed/    # ONNX inference via tract, HuggingFace tokenizer, rayon parallel embedding
├── vec-store/    # rusqlite, cosine similarity search, pack/unpack embeddings
├── vec-index/    # File walker (ignore crate), chunker, incremental update (sha256), diff
├── vec-watch/    # inotify-based real-time re-indexing (vec watch)
├── vec-daemon/   # Unix socket embedding daemon (vec daemon)
├── vec-mcp/      # MCP server (rmcp): search, context, index_status
└── vec-cli/      # Clap CLI entry point, all subcommands
```

Dependency graph: `vec-core` and `vec-embed` and `vec-store` are standalone. `vec-index` depends on all three. `vec-cli` depends on everything.

---

## Data Flow

### Indexing (`vec updatedb`)

```
configured paths
    → file walker (ignore crate — respects .gitignore, exclude config)
        → changed files only (sha256 hash check vs stored hash)
            → chunker (40-line chunks, 10-line overlap, boundary-aware)
                → embed.rs (local ONNX via tract, or HTTP backend)
                    → store.rs (rusqlite: path + byte_offset + embedding BLOB)
```

### Query (`vec "auth middleware"`)

```
query string
    → embed.rs (single embed call — local or HTTP)
        → store.rs (cosine similarity → top-k hits)
            → for each hit: access(path, R_OK) — drop if unreadable
                → read snippet from live file (byte_offset..byte_end)
                    → stdout
```

---

## Database Schema

```sql
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
    -- keys: model_name (str), model_sha256 (hex), embedding_dim (int)
);

CREATE TABLE files (
    id      INTEGER PRIMARY KEY,
    path    TEXT UNIQUE NOT NULL,
    mtime   REAL NOT NULL,
    hash    TEXT NOT NULL         -- sha256; skip re-embed if unchanged
);

CREATE TABLE chunks (
    id           INTEGER PRIMARY KEY,
    file_id      INTEGER REFERENCES files(id) ON DELETE CASCADE,
    byte_offset  INTEGER NOT NULL, -- start byte in source file
    byte_end     INTEGER NOT NULL, -- end byte in source file
    start_line   INTEGER NOT NULL,
    end_line     INTEGER NOT NULL,
    embedding    BLOB NOT NULL     -- float32[N] little-endian; N from meta.embedding_dim
);

CREATE INDEX idx_chunks_file ON chunks(file_id);
```

No `content` column. Text is always read from `path` at `byte_offset..byte_end` at display time.

**Model mismatch detection:** on startup, `store.rs` reads `meta.model_name` and `meta.model_sha256` and compares against the configured model. If they differ, vec exits with a clear error: `model changed — run 'vec updatedb --full' to re-index`. This prevents silently returning garbage results when a user swaps models.

---

## Chunking Strategy

- **Size:** 40 lines, 10-line overlap between adjacent chunks
- **Boundary detection:** Before committing a chunk boundary, scan ±5 lines for a function/class start (`fn `, `pub fn `, `impl `, `def `, `class `, `func `, `function `, `pub struct`, `pub enum`). Prefer splitting there.
- **Minimum chunk:** Skip chunks with fewer than `cfg.min_chunk_lines` non-blank lines (default 5; configurable via `/etc/vec.conf`)
- **Encoding:** Files read as UTF-8; skip on decode error (binary files)
- **Byte offsets:** Computed during chunking from cumulative UTF-8 byte lengths

---

## Embedding Backend

`vec` runs ONNX models in-process via `tract-onnx` — pure Rust, no C deps, no system libs, no network calls. Text never leaves the process.

```toml
[embed]
model = "gte-multilingual-base"   # short name resolved via model_search_path
```

**Model package** (separate distro package, installed independently):

| Package | Model | Size | Languages | Dimensions |
|---------|-------|------|-----------|------------|
| `vec-model-base` | `gte-multilingual-base` | ~90 MB | 50+ | 768 |

`vec` installs without a model package and will not embed until one is installed. Distro meta-packages pull in `vec-model-base` as a recommendation.

**Model resolution order** (first match wins):
1. `{dir}/{name}/model_int8.onnx`
2. `{dir}/{name}/model.onnx`
3. `{dir}/{name}.onnx`
4. Absolute path — if `model` is an absolute path, used directly

**Embedding dimension** is probed automatically on first load and stored in `meta.embedding_dim`. Changing models requires `vec updatedb --full`.

---

## Deployment

vec supports two deployment modes: **system-wide** (default, recommended) and **userland** (no root required).

### System-wide (default)

Central DB at `/var/lib/vec/vec.db` — indexed by a system service, readable by all users. `access(path, R_OK)` enforces per-user visibility at query time.

```
/var/lib/vec/vec.db   chmod 644, owned by root
```

```ini
# /etc/systemd/system/vec-updatedb.timer
[Timer]
OnCalendar=daily
Persistent=true   # fires at boot if missed
RandomizedDelaySec=10min
```

Real-time indexing via `vec-watch.service` (inotify) supplements the daily timer. The timer acts as a reconciliation safety net.

### Userland (no root)

For users without root access. All paths live under `$HOME`:

| Resource | Path |
|----------|------|
| Config | `~/.config/vec/config.toml` |
| Database | `~/.local/share/vec/vec.db` |
| Models | `~/.local/share/vec/models/` |
| Socket | `~/.local/share/vec/embed.sock` |
| Binary | `~/.local/bin/vec` |

Generate the config with `vec init --user > ~/.config/vec/config.toml`.

Systemd user units (`contrib/user/`) provide the same automation as the system units — daily timer, real-time watcher, optional embedding daemon — but run under `systemctl --user` with no privilege escalation. Units use `%h` (systemd specifier for `$HOME`) so they work without modification across users.

The userland DB is private to the user. There is no `access()` filtering — the user owns everything they indexed.

---

## MCP Server

Optional. `vec serve` starts an MCP server for AI assistant integration.

```
search(query: String, limit?: u32, path_filter?: String, min_score?: f32)
    → JSON array of SearchResultItem {path, start_line, end_line, score, snippet?}

context(file_path: String, line: u32, window?: u32)
    → String (annotated source lines, > marks the target line)

index_status()
    → JSON IndexStatusResult {file_count, chunk_count, db_path, model, model_found}
```

Transport: stdio (default for Claude Code MCP registration).

---

## System Packaging

### File Hierarchy

```
/usr/bin/vec
/etc/systemd/system/vec-updatedb.{service,timer}
/etc/systemd/system/vec-watch.service
/etc/sysctl.d/99-vec.conf                          # inotify watch limit
/etc/vec.conf                                      # system-wide config (all defaults commented)
/usr/share/vec/models/                             # populated by vec-model-* packages
/usr/share/doc/vec/
/usr/share/doc/vec/contrib/user/                   # userland systemd units (user copies manually)
```

### Build Flags for Distros

```bash
cargo build --release --features system-sqlite
```

No extra steps. No model download at build time. No Python. No HuggingFace tooling.

### Package Split

**`vec`** — the binary. No hard model dependency; recommends `vec-model-base`.

**`vec-model-base`** — Default model: `gte-multilingual-base` (~90MB int8, 50+ languages). ONNX file + tokenizer.json installed to `/usr/share/vec/models/gte-multilingual-base/`. No build step.

This mirrors how other data-heavy tools are packaged (e.g., `tesseract-ocr` + language packs, `hunspell` + dictionaries).

### Distro-Specific Notes

- **Debian/Ubuntu:** `libsqlite3-dev` build dep; `vec-model-base` is `noarch`/`all`
- **Fedora/RHEL:** `sqlite-devel`; `contrib/vec.spec` + `vec-model-base.spec` provided
- **Arch:** `sqlite`; PKGBUILDs in `contrib/`
- **NixOS:** `rustPlatform.buildRustPackage`; model derivation as a fixed-output derivation (FOD) with known hash

---

## Performance Characteristics

| Operation | Cost | Notes |
|-----------|------|-------|
| Embed single query | ~50–150ms | tract CPU int8, first call compiles model |
| Cosine similarity, 10k chunks | <2ms | Vec<f32> dot products |
| Cosine similarity, 100k chunks | <20ms | Vec<f32> dot products |
| Read snippet from file | <1ms | Local filesystem |
| `vec updatedb`, unchanged files | O(file count) | Hash check only |
| `vec updatedb`, N changed files | N × ~100ms / batch_size | CPU-bound, runs under nice 19 |

100k chunks ≈ 300MB embeddings in memory (100k × 768 × 4 bytes).

# vec — Semantic locate for your codebase

> `locate` finds files by name. `vec` finds files by meaning.

---

## The Problem

`locate auth` finds files named `auth*`.
But you want the file *containing* auth logic — regardless of what it's called.

`grep -r "authentication" .` finds literal strings.
But you want everything *related to* authentication — even if the word never appears.

No existing tool does this at the system level. `vec` does.

---

## Positioning

Most semantic code search tools are developer toys: installed via `pip install`, require Python runtimes, bundle their own dependencies, and aren't designed for multi-user or system-level deployment.

**vec is infrastructure.** It ships as a static binary with no runtime dependencies, integrates with systemd, uses filesystem permissions as the sole access control mechanism, and is designed to be packaged by Linux distributions. The security model is auditable without reading application code — inspect the schema, see no `content` column, done.

---

## The Mental Model

```
locate "filename pattern"    →  matched file paths
vec    "concept or question" →  semantically relevant file paths + line ranges
```

`vec` is to `locate` what semantic search is to ctrl+F.

It works because it pre-indexes your filesystem with embeddings (via local CPU inference), exactly like `updatedb` pre-indexes your filesystem with file paths. Query time is sub-second — the heavy work is done upfront.

---

## Usage

```bash
# Search (the main thing)
vec "authentication middleware"
vec "database connection pooling"
vec "where does error handling happen"
vec "payment processing"

# With snippets
vec "JWT token validation" --snippet

# Limit results
vec "cache invalidation" --limit 5

# Restrict to a path
vec "auth logic" --path ~/projects/backend

# Update the index (like updatedb)
vec updatedb

# Show index stats
vec status
```

---

## Output

Plain file paths by default — pipe-friendly, just like `locate`:

```
/home/user/projects/backend/src/auth/middleware.rs:12
/home/user/projects/backend/src/auth/jwt.rs:1
/home/user/projects/frontend/src/hooks/useAuth.ts:34
/home/user/projects/shared/lib/permissions.rs:88
```

With `--snippet`:

```
/home/user/projects/backend/src/auth/middleware.rs:12 (score: 0.943)
    pub fn verify_token(req: &Request) -> Result<Claims, AuthError> {
        let token = req.headers().get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(AuthError::Missing)?;
```

---

## Architecture

Mirrors `locate` in structure, diverges on one critical point: **the DB never stores code text**.

| locate               | vec                    | purpose                               |
|----------------------|------------------------|---------------------------------------|
| `updatedb`           | `vec updatedb`         | rebuild/update the index              |
| `mlocate.db`         | `vec.db`               | central index (vectors + paths only)  |
| `/etc/updatedb.conf` | `/etc/vec.conf`        | what to index, what to skip           |
| cron job             | systemd unit           | keep index fresh automatically        |

`locate` is safe to share because it only stores paths, not content. `vec` achieves the same property by storing **only embeddings and byte offsets** — never the source text. Snippets are read from the live filesystem on demand. Stealing the DB gives you vectors and paths. Useless without the source files.

### Database Schema

```sql
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
    -- stores: model_name, model_sha256, embedding_dim
    -- vec refuses to search if model_name != configured model
);

CREATE TABLE files (
    id      INTEGER PRIMARY KEY,
    path    TEXT UNIQUE NOT NULL,
    mtime   REAL NOT NULL,
    hash    TEXT NOT NULL      -- sha256, skip re-embedding if unchanged
);

CREATE TABLE chunks (
    id           INTEGER PRIMARY KEY,
    file_id      INTEGER REFERENCES files(id) ON DELETE CASCADE,
    byte_offset  INTEGER NOT NULL,  -- start byte in file
    byte_end     INTEGER NOT NULL,  -- end byte in file
    start_line   INTEGER NOT NULL,
    end_line     INTEGER NOT NULL,
    embedding    BLOB NOT NULL      -- float32[N], little-endian; N from meta.embedding_dim
    -- NO content column — text is read from the live file on demand
);

CREATE INDEX idx_chunks_file ON chunks(file_id);
```

### Deployment

Central system-wide DB:

```
/var/lib/vec/vec.db   chmod 644, owned by root
```

The CLI reads the DB directly. `access(path, R_OK)` is called on every result before display — enforces per-user file visibility at query time with no daemon or separate ACL.

---

## Project Layout

```
vec/
├── Cargo.toml
├── Cargo.lock
├── src/
│   ├── main.rs         # CLI entry point
│   ├── config.rs       # TOML config (/etc/vec.conf)
│   ├── embed.rs        # tract-onnx CPU inference (pure Rust, no C deps)
│   ├── store.rs        # SQLite read/write, cosine similarity, access() validation
│   ├── index.rs        # File walker, chunker, gitignore-aware, incremental
│   ├── watch.rs        # inotify-based real-time re-indexing (vec watch)
│   ├── mcp.rs          # MCP server: search, context, index_status
│   └── util.rs         # Shared helpers
├── contrib/
│   ├── vec.conf               # /etc/vec.conf template (all defaults commented)
│   ├── vec-updatedb.service   # systemd oneshot service
│   ├── vec-updatedb.timer     # daily timer (Persistent=true)
│   ├── vec-watch.service      # inotify real-time watcher service
│   ├── 99-vec.conf            # sysctl: inotify watch limit
│   ├── vec.spec               # RPM spec (binary)
│   ├── vec-model-base.spec    # RPM spec (base model package)
│   └── debian/                # Debian packaging
├── PACKAGING.md           # guide for distro packagers
└── README.md
```

---

## Config

`/etc/vec.conf` — mirrors `/etc/updatedb.conf` in spirit

```toml
[embed]
# model   = "gte-multilingual-base"        # short name resolved via model_search_path
# model_search_path = ["/usr/share/vec/models"]
# batch_size = 16
# max_tokens = 128

[index]
# chunk_size    = 40
# chunk_overlap = 10
# include_paths = ["/"]
# exclude_dirs  = ["vendor", "third_party"]   # appends to compiled defaults
# exclude_files = ["*.generated.ts"]          # appends to compiled defaults

[database]
# db_path = "/var/lib/vec/vec.db"
# wal     = true
```

---

## Embedding Backend

Runs ONNX models in-process via `tract-onnx` (pure Rust, no C deps, no system libs, no network calls). Text never leaves the process.

| Package | Model | Size | Languages |
|---------|-------|------|-----------|
| `vec-model-base` | `gte-multilingual-base` | ~90 MB | 50+ |

---

## Crate Dependencies

```toml
[dependencies]
clap         = { version = "4", features = ["derive"] }      # CLI
rusqlite     = { version = "0.31", features = ["bundled"] }  # SQLite (or system)
tokenizers   = "0.21"                                        # HuggingFace tokenizers
serde        = { version = "1", features = ["derive"] }
serde_json   = "1"
toml         = "0.8"
ignore       = "0.4"    # gitignore-aware walker (ripgrep's engine)
sha2         = "0.10"
hex          = "0.4"
rmcp         = { version = "0.3", features = ["server", "transport-io"] }
tokio        = { version = "1", features = ["rt-multi-thread", "macros"] }
anstream     = "0.6"
anyhow       = "1"
dirs         = "5"
notify       = "6"      # inotify-based real-time watching
tract-onnx   = "0.21"   # pure-Rust ONNX inference (no C deps)
ndarray      = "0.17"

[target.'cfg(unix)'.dependencies]
nix = { version = "0.29", features = ["user", "fs"] }  # access() syscall

[features]
system-sqlite = []   # link against system libsqlite3 (distro builds only)
```

---

## MVP Scope

- [x] `src/store.rs` — SQLite schema (no content col) + cosine similarity
- [x] `src/embed.rs` — tract-onnx CPU inference (local ONNX, no HTTP backend)
- [x] `src/index.rs` — file walker + chunker + gitignore filter + hash-based incremental
- [x] `src/main.rs` — `vec`, `vec updatedb`, `vec status`, `vec model download`, `vec watch`, `vec serve`
- [x] `src/config.rs` — TOML config with compiled-in defaults
- [x] `src/watch.rs` — inotify watcher with debounce
- [x] `src/mcp.rs` — MCP server with `search`, `context`, `index_status`
- [x] `Cargo.toml` — features (system-sqlite)
- [x] `contrib/` — systemd units, sysctl, RPM spec skeleton, Debian skeleton
- [x] `README.md` — quick start and usage guide

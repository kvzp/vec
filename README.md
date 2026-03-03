# vec

`locate` finds files by name. `vec` finds files by meaning.

```bash
vec "authentication middleware"
vec "database connection pooling"
vec "where does error handling happen"
```

Fully local. No cloud. Embeddings run in-process on CPU via a pure-Rust ONNX engine — no Python, no external service. Index in SQLite. Ships as a static binary with no runtime dependencies.

---

## Install

### Distro packages (recommended)

```bash
# Debian / Ubuntu
apt install vec vec-model-base

# Fedora / RHEL
dnf install vec vec-model-base

# Arch
pacman -S vec vec-model-base

# NixOS
nix-env -iA nixpkgs.vec nixpkgs.vec-model-base
```

`vec-model-base` ships the `gte-multilingual-base` embedding model (~90MB, 50+ languages).

### Pre-built binary

Download from [GitHub Releases](https://github.com/kvzp/vec/releases) — static binary, no dependencies:

```bash
curl -L https://github.com/kvzp/vec/releases/latest/download/vec-linux-x86_64.tar.gz | tar xz
install -m755 vec /usr/local/bin/
```

Then install a model package — see [Config](#config).

### From source

```bash
cargo install vec-locate
```

Configure a model — see [Config](#config).

---

## Quick Start

```bash
vec "JWT token validation"
```

That's it. `vec updatedb` runs automatically at install time via the systemd timer — no manual step required. It indexes everything on the filesystem the user can read, excluding common noise dirs.

---

## Usage

```bash
# Search
vec "authentication middleware"
vec "payment processing"
vec "cache invalidation" --limit 5
vec "auth logic" --path ~/projects/backend

# Show snippets inline
vec "error handling" --snippet

# Filter weak matches at query time
vec "auth logic" --min-score 0.82

# Index management
vec updatedb                        # incremental re-index (changed files only)
vec updatedb --full                 # force full re-index
vec status                          # DB stats, config, last update

# Real-time watching (runs as a systemd service, rarely invoked directly)
vec watch

# MCP server (AI assistant integration)
vec serve

# First-time setup
vec init | sudo tee /etc/vec.conf   # write default config
```

---

## Output

Plain `file:line` by default — pipe-friendly:

```
/home/user/projects/backend/src/auth/middleware.rs:12
/home/user/projects/backend/src/auth/jwt.rs:1
/home/user/projects/frontend/src/hooks/useAuth.ts:34
```

With `--snippet`:

```
/home/user/projects/backend/src/auth/middleware.rs:12 (score: 0.943)
    pub fn verify_token(req: &Request) -> Result<Claims, AuthError> {
        let token = req.headers().get("Authorization")
```

Works with `fzf`:

```bash
vec "auth logic" | fzf
```

---

## Config

No config file is required — compiled-in defaults work after installing `vec` and a model package.

To customize, run `vec init | sudo tee /etc/vec.conf` and edit:

```toml
[embed]
model = "all-MiniLM-L6-v2"        # short name resolved via model_search_path
# model_search_path = ["/usr/share/vec/models"]

[index]
# Scope to directories with meaningful content (default is "/")
include_paths = ["/etc", "/home", "/root", "/opt", "/srv", "/usr/local", "/var/www"]
# exclude_dirs and exclude_files append to compiled defaults — only list additions

[search]
default_limit = 10

[database]
db_path = "/var/lib/vec/vec.db"
wal     = true
```

See [CONFIG.md](CONFIG.md) for the full reference.

---

## Embedding Models

Embeddings run in-process on CPU via tract-onnx. Install a model package:

| Package | Model | Size | Languages |
|---------|-------|------|-----------|
| `vec-model-base` | `gte-multilingual-base` | ~90 MB | 50+ |

No external service. No network calls. Text never leaves the process.

---

## MCP Integration (Claude Code / AI Assistants)

`vec serve` starts an MCP server so AI assistants can call semantic search mid-session.

Add to `~/.claude.json`:

```json
{
  "mcpServers": {
    "vec": {
      "command": "vec",
      "args": ["serve"]
    }
  }
}
```

See [MCP.md](MCP.md) for Cursor, Continue, and other MCP clients.

Exposed tools: `search`, `context`, `index_status`.

---

## How It Works

1. `vec updatedb` walks configured paths, chunks files (~40 lines each), embeds each chunk in-process via tract-onnx on CPU
2. Only embeddings + byte offsets are stored — **no source text in the DB**
3. On search, your query is embedded and compared against all stored vectors (cosine similarity)
4. Matching byte ranges are resolved to snippets by reading the live source file

The DB contains no source code. Stealing it gives you vectors and paths — nothing useful without the source files.

---

## Why vec is different

- **Designed for system packaging.** Static binary, links against system libsqlite3, ships systemd units, follows FHS. Works the same whether installed via `apt` or compiled from source.
- **Auditable security model.** No source text in the DB — verifiable by inspecting the schema. Filesystem permissions are the ACL — no separate permission system to audit.
- **Unix ACL as the security boundary.** Every result is validated with `access()` before being shown. Revoke file read access → result disappears on next query. No index invalidation needed.
- **Zero cloud.** Embeddings run in-process on CPU. DB on local disk. Nothing leaves the machine.
- **Pipe-friendly.** Default output is plain `file:line` paths. Works with `xargs`, `fzf`, editors.
- **Incremental.** Hash-based change detection. `vec updatedb` only re-embeds changed files.
- **Gitignore-aware.** Respects `.gitignore`. No indexing of build artifacts.
- **Real-time.** `vec-watch.service` (inotify) re-indexes changed files within seconds.

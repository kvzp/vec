# vec Configuration Reference

## Config file

One config file: **`/etc/vec.conf`** (TOML format).

vec is a system tool — there is no per-user config file by design. Users control search at query time via CLI flags (`--path`, `--limit`, `--snippet`). Admins control indexing policy via `/etc/vec.conf`.

```
compiled-in defaults  →  /etc/vec.conf (if present)  →  CLI flags
```

Every setting has a sensible compiled-in default. An empty or absent `/etc/vec.conf` is valid — vec works out of the box.

Generate a starter config:
```bash
vec init | sudo tee /etc/vec.conf
```

---

## Merge semantics

Most settings **replace** the default when set in `/etc/vec.conf`. Two exceptions that **append** instead:

| Setting | Behaviour |
|---------|-----------|
| `include_paths` | **Replaces** — explicitly defines the scan root(s) |
| `exclude_dirs` | **Appends** to compiled defaults |
| `exclude_files` | **Appends** to compiled defaults |

This means for `exclude_dirs` and `exclude_files` you only specify what you want to add. The compiled defaults (`.git`, `node_modules`, `proc`, `ssl`, `dpkg`, etc.) are always in effect.

For `include_paths`, you define the full list — it replaces `["/"]` entirely.

---

## Settings reference

### `[embed]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `model` | string | `"gte-multilingual-base"` | Model name or absolute path to ONNX file |
| `model_search_path` | array of paths | `["/usr/share/vec/models"]` | Directories searched for named models, in order |
| `batch_size` | integer | `16` | Chunks per inference call. Higher = better throughput; lower = less peak RAM |
| `max_tokens` | integer | `128` | Token limit per chunk passed to the tokeniser. Longer chunks are truncated. Attention is O(n²) — halving this quadruples indexing speed |
| `daemon_socket` | path | `"/run/vec/embed.sock"` | **Optional.** Unix socket path for `vec daemon`. `vec` tries this socket first and falls back to in-process model loading if the daemon is not running. See note below. |

> **`daemon_socket` / `vec daemon` is completely optional.**
> The daemon keeps the compiled ONNX model in resident memory (150–300 MB)
> to eliminate per-query startup latency. Only enable it if you run `vec`
> interactively many times a day **and** have RAM to spare. vec works
> identically without it — the only difference is a few seconds of startup
> time per query. See `contrib/vec-embed.service` for the systemd unit.

**Model resolution:** given `model = "gte-multilingual-base"`, vec searches each `model_search_path` directory for:
1. `{dir}/gte-multilingual-base/model_int8.onnx`
2. `{dir}/gte-multilingual-base/model.onnx`
3. `{dir}/gte-multilingual-base.onnx`

Or set `model` to an absolute path to bypass the search.

**Changing the model** invalidates all existing embeddings. Run `vec updatedb --full` after any model change.

---

### `[index]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `include_paths` | array of paths | `["/"]` | Root paths to walk. **Replaces** the default — specify the full list |
| `exclude_dirs` | array of strings | *(see below)* | Directory names to skip. **Appends** to compiled defaults |
| `exclude_files` | array of patterns | *(see below)* | Glob patterns matched against filenames. **Appends** to compiled defaults |
| `chunk_size` | integer | `40` | Target lines per chunk |
| `chunk_overlap` | integer | `10` | Overlap lines between adjacent chunks — prevents concepts at boundaries from being missed |
| `max_file_size` | integer (bytes) | `10485760` | Skip files larger than this (10 MB). Prevents indexing huge logs or binaries |
| `min_file_size` | integer (bytes) | `50` | Skip files smaller than this. Avoids embedding near-empty files |
| `min_chunk_lines` | integer | `5` | Skip chunks with fewer non-blank lines. Short fragments embed near the centroid of the vector space and score misleadingly high for unrelated queries. **Note:** the chunker currently uses this as a compiled constant; the config value is stored but not yet read by the chunker. |
| `gitignore` | bool | `true` | Respect `.gitignore` files at every directory level |

#### Compiled-in `exclude_dirs` defaults

These are always excluded regardless of config:

| Directory name | Reason |
|----------------|--------|
| `.git` | VCS metadata |
| `node_modules`, `target`, `dist`, `build` | Build artifacts |
| `__pycache__`, `.venv`, `.cache`, `.cargo` | Toolchain caches |
| `proc`, `sys`, `dev`, `run` | Linux virtual filesystems — entries vanish mid-scan, causing spurious errors |
| `ssl`, `certs` | Certificate stores — binary DER/PEM blobs; public certs carry no semantic meaning |
| `dpkg`, `apt`, `rpm` | Package manager databases — file lists and checksums, not useful search content |
| `cloud`, `alternatives` | Runtime/cloud-init state |
| `dist-packages`, `site-packages` | Installed Python packages, not user code |
| `apparmor.d`, `iproute2`, `abi` | System lookup tables (ABI flags, routing scopes) — terse key/value files that embed near the vector centroid and pollute results |

#### Compiled-in `exclude_files` defaults

| Pattern | Reason |
|---------|--------|
| `*.lock` | Dependency lockfiles — generated, not authored |
| `*.min.js`, `*.map` | Minified/source-map files |
| `*.pyc` | Python bytecode |
| `.env`, `.env.*` | **Secret material** — environment variables, credentials |
| `*.key`, `*.pem`, `*.p12`, `*.pfx` | **Key material** — never index private keys |
| `*.onnx`, `*.bin`, `*.so`, `*.dylib` | Binary blobs |

---

### `[search]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `default_limit` | integer | `10` | Results returned when `--limit` is not specified |
| `snippet_lines` | integer | `6` | Lines of context shown per result with `--snippet` |

---

### `[database]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `db_path` | path | `/var/lib/vec/vec.db` | Path to the SQLite database. `~` is expanded |
| `wal` | bool | `true` | Enable SQLite WAL mode — allows concurrent reads during indexing |

The DB stores only file paths, byte offsets, and embedding vectors — no source text. It is safe to make world-readable (mode 644). Access control is enforced by `access(path, R_OK)` at query time.

---

## Common recipes

### Server deployment — index only meaningful paths

```toml
# /etc/vec.conf
[embed]
model = "all-MiniLM-L6-v2"

[index]
include_paths = ["/etc", "/home", "/root", "/opt", "/srv", "/usr/local", "/var/www"]
```

`include_paths` replaces the default `["/"]`, so the walker never touches `/var/lib`, `/usr/share`, or other system noise.

### Developer workstation — index home directory only

```toml
[index]
include_paths = ["~"]
exclude_dirs = ["Downloads", "Videos", "Music", "Pictures"]
```

### Add project-specific exclusions

```toml
[index]
# These append to the compiled defaults — .git, node_modules etc. still excluded
exclude_dirs = ["vendor", "third_party", ".terraform"]
exclude_files = ["*.generated.ts", "*.pb.go"]
```

### Custom model

```toml
[embed]
model = "/path/to/custom/model_int8.onnx"
```

Absolute paths bypass the search. Run `vec updatedb --full` after changing models — old embeddings are incompatible.

### Suppress weak matches

Use the `--min-score` CLI flag at query time:
```bash
vec --min-score 0.82 "authentication middleware"
```

### Large codebase — tune chunking

```toml
[index]
chunk_size = 60
chunk_overlap = 15
max_tokens = 256  # more context per chunk; slower indexing
```

---

## What NOT to index

**Never index secret material.** The compiled defaults exclude the common patterns (`.env`, `*.key`, `*.pem`), but project-specific secret files may need explicit exclusion:

```toml
[index]
exclude_files = ["secrets.yaml", "credentials.json", "*.vault"]
exclude_dirs = ["secrets", "private", ".secrets"]
```

Although the vec DB contains no source text, the existence of a file path in search results can leak information. Exclude sensitive files from indexing entirely.

---

## Troubleshooting

**"Why is `/var/lib/dpkg` showing up in results?"**
You're running an old binary or the current binary was built before the `dpkg` exclusion was added. Run `vec updatedb --full` to purge the old entries.

**"Why isn't `/home/user/project` being indexed?"**
Check two things: (1) `include_paths` — if set in `/etc/vec.conf`, it replaces the default `["/"]`; add your path. (2) `.gitignore` — if `gitignore = true` (the default), gitignored files are skipped.

**"Why are scores so high for unrelated files?"**
The stub embedder is running — the model failed to load. Check `vec status` for `Model path: not found`. Fix with `vec model download` or check the model path in `/etc/vec.conf`.

**"Indexing is very slow."**
`max_tokens` is O(n²) in the attention mechanism. Halving it (e.g. 128 → 64) quadruples speed with minimal quality impact for short code chunks. Also verify indexing runs under `nice -n 19`.

**"How do I re-index after changing config?"**
Run `vec updatedb --full`. A plain `vec updatedb` only re-indexes files with changed checksums; it won't remove chunks from paths that are now excluded.

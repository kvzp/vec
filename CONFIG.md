# vec Configuration Reference

Every key, its default and its meaning. The defaults are compiled in from
[`crates/vec-core/defaults.toml`](crates/vec-core/defaults.toml), which is the
single source of truth (the Rust code holds no default values). No config file
is needed.

```bash
vec init | sudo tee /etc/vec.conf                  # every key, commented out
vec init --user > ~/.config/vec/config.toml         # userland: paths under your home
vec config show                                     # effective values + which layer set each
vec config check /etc/vec.conf                      # validate one file (exit 1 on error)
```

## Layers

Applied in order, each overriding the previous one:

| # | Layer | Notes |
|---|-------|-------|
| 1 | compiled-in defaults | `defaults.toml` |
| 2 | `/etc/vec.conf` | system-wide, admin-controlled |
| 3 | `$XDG_CONFIG_HOME/vec/config.toml` (usually `~/.config/vec/config.toml`) | per-user; **skipped when `--config` is given** |
| 4 | `--config <file>` | explicit file |
| 5 | command-line flags | `--limit`, `--min-score`, `--path`, ... |

The locations of layers 2 and 3 cannot come from a config file; they are
compiled in from [`crates/vec-core/locations.toml`](crates/vec-core/locations.toml)
(packagers may change them before building).

**Provenance.** `vec config show` prints each effective key with the layer(s)
that set it (`# defaults`, `# /etc/vec.conf`, ...). Validation errors name the
file(s) that set the offending key.

**Merge rules.**

- Scalars and most lists **replace** the value of the previous layer (`include_paths` included).
- `exclude_dirs` and `exclude_files` **add** to the list built so far. An entry
  `"!pattern"` removes `pattern` from it (`exclude_dirs = ["!build"]` indexes
  `build/` directories again).

**Strictness.** An unknown key or section is an error in every layer, so a typo
never goes unnoticed. Paths may start with `~/` (home of the user running vec);
after that expansion they must be absolute.

**Deprecated.** `[embed] daemon_socket` is accepted for one more release: its
value is used as `[daemon] socket` with a warning naming the file (ignored, with
a warning, when the same file also sets `[daemon] socket`). Move it.

## `[embed]`

| Key | Default | Meaning | Validation |
|-----|---------|---------|------------|
| `backend` | `"onnx"` | `"onnx"`: local ONNX model in-process (tract, pure Rust). `"ollama"`: Ollama-compatible HTTP embedding API | one of the two |
| `model` | `"gte-multilingual-base"` | onnx: short name resolved in `model_search_path`, or an absolute `.onnx` path (`tokenizer.json` beside it). ollama: model name sent to the server | non-empty |
| `embed_url` | unset | ollama: base URL `http://host:port[/path]` | required for ollama; must start with `http://` (no TLS client) |
| `http_endpoint_path` | `"/api/embed"` | ollama: path appended to the path of `embed_url` | starts with `/` |
| `http_connect_timeout_ms` | `5000` | ollama: TCP connect timeout | > 0 |
| `http_read_timeout_ms` | `300000` | ollama: read/write timeout of one request | > 0 |
| `http_concurrency` | `4` | ollama: requests in flight at once | > 0 |
| `http_max_input_bytes` | `1200` | ollama: texts are cut to this many bytes (at a character boundary) before sending | > 0 |
| `http_error_preview_chars` | `200` | ollama: characters of an error response body quoted in errors | > 0 |
| `model_search_path` | `["/usr/share/vec/models"]` | onnx: directories searched in order for a short model name | non-empty, absolute |
| `model_files` | `["model_int8.onnx", "model.onnx"]` | onnx: file names tried inside `{dir}/{model}/` | non-empty entries |
| `batch_size` | `16` | texts per embedding call during indexing (per HTTP request; per indexing work unit for onnx) | > 0 |
| `onnx_batch_size` | `1` | onnx: texts per forward pass. `1` = unpadded and deterministic (a text always gets the vector a query would get). Larger is ~1.5x faster but int8 models then return slightly different vectors (cosine ~0.98) | > 0 |
| `max_tokens` | `128` | onnx: token limit per chunk; longer chunks are truncated | > 0 |
| `index_threads` | `0` | onnx: embedding threads during indexing; `0` = one per CPU core | |
| `model_download_url` | Hugging Face URL of the default model | base URL used by `vec model download` | `http(s)://` |
| `model_download_files` | `["onnx/model_int8.onnx", "tokenizer.json"]` | files fetched by `vec model download`, relative to the URL, saved under their last path component in `{model_search_path[0]}/{model}/` | relative, no `..` |
| `daemon_socket` | — | **deprecated**, see above | |

**Model resolution** (onnx): an absolute `model` is used as is. Otherwise, for
each directory of `model_search_path`: `{dir}/{model}.onnx`, then
`{dir}/{model}/{f}` for each `f` of `model_files`. The first existing file wins.

**Changing the model** makes existing vectors incompatible. vec records the
model identity (name, dimension, sha256 of the model file) in the index and
refuses to mix: queries and incremental updates fail with a "model mismatch"
error until you run `vec updatedb --full`. See
[ARCHITECTURE.md](ARCHITECTURE.md#model-identity).

## `[index]`

| Key | Default | Meaning | Validation |
|-----|---------|---------|------------|
| `include_paths` | `["/"]` | root paths to walk (**replaces**) | non-empty, absolute |
| `exclude_dirs` | see `defaults.toml` | directories to skip, `.gitignore` syntax rooted at each include path (**adds**; `!name` removes) | |
| `exclude_files` | see `defaults.toml` | file-name globs to skip (**adds**; `!pattern` removes) | |
| `chunk_size` | `40` | target lines per chunk | > 0 |
| `chunk_overlap` | `10` | lines shared by adjacent chunks | < `chunk_size` |
| `max_file_size` | `10485760` | skip larger files (bytes); also the read cap for snippets and MCP `context` | >= `min_file_size` |
| `min_file_size` | `50` | skip smaller files (bytes) | |
| `min_chunk_lines` | `5` | a window with fewer non-blank lines is merged into a neighbour; a file with fewer in total gets no chunk | |
| `gitignore` | `true` | honour `.gitignore` and `.git/info/exclude` (`.ignore` files are always honoured, the global git excludes file never) | |
| `boundary_scan` | `5` | chunk boundaries may move up to this many lines to land on a boundary line; `0` = never | |
| `boundary_patterns` | `fn `, `pub fn `, `impl `, `def `, `class `, `func `, `function `, ... | a line starting with one of these (after leading whitespace) is a boundary line | no blank entries |

`exclude_dirs` matching: a bare name (`"builds"`) matches at any depth; a
pattern with a leading or inner `/` (`"/builds"`, `"web/dist"`) is anchored to
the include path; an absolute path inside an include path matches exactly that
directory. Hidden entries (names starting with `.`) are always skipped, and
symlinks are never followed.

The default `exclude_dirs` cover VCS metadata and build output (`.git`,
`node_modules`, `target`, `build`, ...), virtual filesystems (`proc`, `sys`,
`dev`, `run`), certificate stores, package-manager databases and installed
Python packages. The default `exclude_files` cover lock files, minified files,
bytecode, **secret material** (`.env`, `.env.*`, `*.key`, `*.pem`, `*.p12`,
`*.pfx`) and binary blobs. The full lists, with reasons, are in
`defaults.toml` and `vec config show`.

## `[search]`

| Key | Default | Meaning | Validation |
|-----|---------|---------|------------|
| `default_limit` | `10` | results without `--limit` | > 0 |
| `snippet_lines` | `3` | snippet **radius**: lines before and after the best line (`--snippet`, MCP), so up to `2n+1` lines | |
| `path_boost` | `0.05` | added to the score for each query word found in the result's path below its include path; a boosted score never exceeds 1.0; `0` = off | >= 0 |
| `path_boost_min_word_chars` | `3` | shorter query words never boost | |
| `max_queries` | `8` | most queries per search (`vec a b c`, MCP `queries`) | > 0 |
| `min_score` | `0.0` | results below this cosine similarity are dropped | -1.0 to 1.0 |
| `context_window` | `10` | lines before and after the line of `vec context` / MCP `context` | |

## `[database]`

| Key | Default | Meaning | Validation |
|-----|---------|---------|------------|
| `db_path` | `"/var/lib/vec/vec.db"` | the SQLite index | absolute |
| `wal` | `true` | write-ahead logging (`synchronous=NORMAL`): readers never block the indexer | |
| `busy_timeout_ms` | `5000` | how long a connection waits for a lock held by another process | > 0 |
| `migration_wait_ms` | `600000` | while another process holds the lock during a schema upgrade, keep retrying this long (one message on stderr) | >= `busy_timeout_ms` |

After upgrading vec, run `vec updatedb` once (as the index owner) before
restarting services: opening an index of an older schema for writing upgrades it
in place, in one transaction; queries cannot read it until then. See the upgrade
note in [README.md](README.md#upgrading).

## `[watch]`

Settings of `vec watch` (`vec-watch.service`); behaviour in
[ARCHITECTURE.md](ARCHITECTURE.md#real-time-watcher).

| Key | Default | Meaning | Validation |
|-----|---------|---------|------------|
| `debounce_ms` | `3000` | re-index once no event arrived for this long; also the first retry delay | > 0 |
| `max_wait_ms` | `30000` | re-index at the latest this long after the first pending change, even under constant churn | >= `debounce_ms` |
| `max_pending` | `10000` | distinct pending paths kept; above it they collapse into parent directories (never above an include path), and failing that into a rescan of the include paths | > 0 |
| `startup_rescan` | `true` | re-index the include paths at start (catches changes made while stopped; reads every file whose size or mtime changed) | |
| `retry_max_ms` | `60000` | upper bound of the retry delay after a failed re-index (doubles from `debounce_ms`) | >= `debounce_ms` |

`SIGHUP` (`systemctl reload vec-watch`) reloads the configuration and the
embedder; a reload that fails keeps the running configuration.

## `[daemon]`

The optional embedding daemon (`vec daemon`, `vec-embed.socket` /
`vec-embed.service`). Queries use it when it serves the index's model, otherwise
they embed in-process (with a one-line warning when the daemon exists but cannot
be used). Protocol and threat model: [ARCHITECTURE.md](ARCHITECTURE.md#embedding-daemon),
[SECURITY.md](SECURITY.md#embedding-daemon).

| Key | Default | Meaning | Validation |
|-----|---------|---------|------------|
| `socket` | `"/run/vec/embed.sock"` | Unix socket the daemon listens on and queries try first | absolute |
| `socket_mode` | `"0666"` | permission bits of a socket the daemon binds itself (with systemd, the `.socket` unit's `SocketMode=` applies instead) | octal string, <= `0777` |
| `max_request_bytes` | `1048576` | largest request accepted, headers included | > 0 |
| `max_texts_per_request` | `64` | most texts per request (each costs one inference and `dim × 4` reply bytes); clients split larger batches | > 0 |
| `io_timeout_ms` | `10000` | daemon: deadline for each whole request and each whole reply write (a trickling or non-reading client is dropped). Client: bound on finishing a reply once it started | > 0 |
| `reply_timeout_ms` | `60000` | how long a client waits for a reply to start: covers a socket-activated cold start (model load + sha256) and the inference of one request | >= `io_timeout_ms` |
| `idle_exit_secs` | `0` | exit after this many seconds without a request; `0` = never (useful with socket activation) | |
| `max_connections` | `16` | connections served at once; one more gets an immediate "busy" reply and embeds in-process | > 0 |
| `accept_retry_ms` | `100` | pause after a failed `accept()` (e.g. out of file descriptors) | > 0 |

`vec status` does not contact the daemon (with socket activation that would
start it); `vec status --daemon` and `vec doctor` do.

## Recipes

```toml
# Server: index only meaningful paths (include_paths replaces ["/"])
[index]
include_paths = ["/etc", "/home", "/root", "/opt", "/srv", "/usr/local", "/var/www"]

# Workstation: home only, skip media; re-include build/ directories
[index]
include_paths = ["~"]
exclude_dirs = ["Downloads", "Videos", "Music", "Pictures", "!build"]
exclude_files = ["*.generated.ts", "*.pb.go", "secrets.yaml", "credentials.json"]

# Remote embeddings (text leaves the machine)
[embed]
backend = "ollama"
embed_url = "http://ollama.example:11434"
model = "nomic-embed-text"

# Socket-activated daemon that frees its memory after 10 idle minutes
[daemon]
idle_exit_secs = 600
```

After changing `model`, `backend` or the chunking keys, run
`vec updatedb --full`. Exclusion changes need only `vec updatedb` (it prunes
newly excluded files) or, for the watcher, `systemctl reload vec-watch`.

## Troubleshooting

Start with `vec doctor`: it checks the configuration, model, index and daemon
and exits non-zero on a problem. Failure behaviour in detail:
[FAILURE-ZONE.md](FAILURE-ZONE.md).

| Symptom | Cause / fix |
|---------|-------------|
| `model mismatch: the index at ... was built with ...` | model changed, or index built by vec before this release: `vec updatedb --full` |
| `the index ... uses schema version 0` | run `vec updatedb` once as the index owner |
| `unknown field` at startup | typo or removed key in a config layer; `vec config check <file>` |
| `/home/user/project` not indexed | `include_paths` replaces `["/"]`; gitignored; hidden directory; `exclude_dirs` match |
| indexing slow | lower `max_tokens`; raise `onnx_batch_size` (see its trade-off); limit `include_paths` |

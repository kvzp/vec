# vec — Architecture

## Design decisions

1. **No source text in the database.** The index stores paths, byte offsets,
   line numbers and vectors. Snippets are read from the live file at query time,
   after an access check, so stealing the DB reveals file names and vectors only,
   and snippets are never stale.
2. **The filesystem is the ACL.** Every result is checked with `access(path, R_OK)`
   as the querying user. No user list, no separate permission system.
3. **`locate` model.** Heavy work (embedding) at index time; a query is one
   embedding plus a scan of float vectors.
4. **System packaging.** One binary, systemd units, FHS paths; distro builds link
   the system SQLite (`system-sqlite` feature).
5. **No literal defaults in code.** Every tunable lives in
   `crates/vec-core/defaults.toml` (compiled in); file locations in
   `crates/vec-core/locations.toml`. See [CONFIG.md](CONFIG.md).

## Crates

```
vec-embed    ONNX (tract) / HTTP (Ollama) / stub embedders, ModelIdentity
vec-core     config layers + validation + provenance, init templates, snippet,
             model sha256 cache, util (access, symlink-safe capped reads)
vec-store    SQLite schema + migrations, model identity, atomic per-file writes, search
vec-index    walker, chunker, incremental indexer, diff
vec-watch    inotify watcher (vec watch)
vec-daemon   embedding daemon + client, protocol v2, socket activation (Unix only)
vec-query    read side shared by CLI and MCP: read-only store, model check,
             daemon-or-in-process embedding, filters, ranking, annotation, status
vec-mcp      MCP server (vec serve)
vec-cli      clap CLI, every subcommand
```

Dependencies point down: `vec-embed` ← `vec-core` ← `vec-store` ← `vec-index` ←
`vec-watch` ← `vec-query` ← `vec-mcp` ← `vec-cli`; `vec-daemon` depends on
`vec-core` + `vec-embed` and is used by `vec-query` and `vec-cli`.

## Data flow

### Indexing (`vec updatedb`, `vec watch` flushes)

```
include_paths ─ walker (ignore crate: .gitignore, exclude_dirs, hidden, no symlinks)
  → filters (exclude_files, size range, regular file, not the DB or its side files)
  → same size+mtime as stored?  skip unread
  → open below the include path component by component with O_NOFOLLOW,
    (dev, inode) must equal the walker's lstat; read capped at max_file_size
  → not UTF-8 → not indexable;  same sha256 → refresh size/mtime only
  → chunk → reuse vectors of chunks whose text hash is stored → embed the rest
  → Store::replace_file: record + all chunks in one transaction
  → after the walk: prune records the walk did not accept
```

A failed batch is retried file by file; files that still fail are counted as
errors and keep their previous entry. If no file of the batch embeds, a probe
text decides: the probe fails too → the backend is down and the run stops
(nothing pruned); the probe works → only those files are bad. `vec diff` runs
the same walker and filters without writing.

### Query (`vec "..."`, MCP `search`)

```
queries (≤ [search] max_queries)
  → open the index read-only; read its model identity
  → embed: daemon if it serves a compatible model, else in-process (loaded once
    per process, checked against the index)
  → per query: one streaming scan over one read snapshot, top-k heap;
    path prefix / excludes / access(R_OK) applied before the limit
  → rank: merge queries (best score per chunk), path boost, deterministic order
  → annotate: read each file once (O_NOFOLLOW, regular, capped): best line + snippet
```

Byte offsets and lines come from the index, text from the live file: everything
is clamped, so an edited or shrunk file never panics, it only gives a worse snippet.

## Database schema (version 1)

```sql
CREATE TABLE meta   (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE files  (id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL,
                     mtime REAL NOT NULL, size INTEGER, hash TEXT NOT NULL);
CREATE TABLE chunks (id INTEGER PRIMARY KEY,
                     file_id INTEGER REFERENCES files(id) ON DELETE CASCADE,
                     byte_offset INTEGER NOT NULL, byte_end INTEGER NOT NULL,
                     start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
                     text_hash TEXT,           -- sha of the chunk text, for vector reuse
                     embedding BLOB NOT NULL); -- L2-normalised f32 LE, dim × 4 bytes
CREATE INDEX idx_chunks_file ON chunks(file_id);
```

There is no content column. `meta` keys: `model_name`, `embedding_dim`,
`model_sha256`, the embedding canary (`canary.text`, `canary.vector`,
`canary.runtime`), the runtime history (`runtime.first_write`,
`runtime.last_write`), `updatedb.last_run`, and the watcher's `watch.*`
(`last_flush`, `last_success`, `last_error`, `last_paths`, `watched_dirs`,
`watch_failures`, `pending`, `overflows`, `rescans`, `reloads`).

**Versioning.** The version is `PRAGMA user_version`. Opening for writing
migrates in place, in one `IMMEDIATE` transaction: version 0 (pre-versioning) →
1 adds `files.size` and `chunks.text_hash` (NULL for old rows) and rewrites every
vector normalised. A process that finds the lock taken during an upgrade retries
for `[database] migration_wait_ms`. Read-only opens (queries) never migrate:
they ask for `vec updatedb`. A newer schema than the binary knows is refused.
Requires SQLite ≥ 3.35 (`RETURNING`).

## Model identity

`ModelIdentity { name, dim, sha256 }`: the configured model name, the vector
dimension, and the sha256 of the model file (ONNX) or the model digest the
server reports (HTTP: the entry of `[embed] model` in `http_tags_path`, Ollama
`/api/tags`, matched with `http_default_tag` when the name has no tag; fetched
once when the embedder is created). Two identities are compatible when name and
dimension are equal and, when both hashes are known, the hashes are equal. A
model re-pulled under the same tag with other weights has another digest, so
writers and queries refuse it like another model. A server that reports no
digest leaves `sha256` unknown (then name and dimension are the identity), and
an index recorded without one adopts it at the next write.

- **Writers** (`updatedb`, `watch`, the daemon) know the sha256: from the cache
  (`~/.cache/vec/model-sha256.toml`, keyed by path, size, mtime, device, inode)
  or by hashing the file. `Store::ensure_model` records the identity in an empty
  index, fills in a missing hash, or fails with a typed `ModelMismatch`.
  `replace_file` re-checks inside its transaction.
- **Readers** never hash: they check name and dimension, and the sha256 only when
  the cache already has it (ONNX) or the server reported a digest (HTTP). `[embed] model` differing from the index's model name
  is an error asking for `vec updatedb --full`.
- **Legacy index** (no recorded identity): adopted when its vectors have the
  model's dimension, refused otherwise.
- **`--full`** with the same model re-embeds every file while the old entries
  stay searchable. With a different model (only on a real `ModelMismatch`),
  the index is emptied right before the first successful write, so a model that
  cannot embed leaves it intact. A scoped `--full` (`--path`) with another model
  is refused.

### Embedding runtime and canary

The identity cannot see the same model computed differently: an Ollama upgrade
(e.g. a minor Ollama release), a tract upgrade, a quantization change behind the same
digest-less name. Those can shift every vector slightly, and new vectors then
compare worse with the stored ones. Two records cover it (`vec_store::canary`):

- **Runtime** (`Embedder::runtime`): `ollama <version>` (asked from
  `http_version_path` at each check, so a restarted server shows its new
  version), `tract <version> (vec <version>)` for ONNX (tract's version is read
  from `Cargo.lock` at build time; `vec <version>` alone without a lock file).
  Writers record `runtime.last_write` at their first write of a run and
  `runtime.first_write` when none is recorded, after a model reset and after a
  completed unscoped `--full`. Informational: `vec status`, `vec doctor`, MCP
  `index_status`.
- **Canary**: the vector of `[embed] canary_text`, stored with its runtime at the
  first write to an index (or the first write after a model reset). A check
  embeds the **stored** text again and takes the cosine with the stored vector:
  `>= canary_warn_cosine` ok; down to `canary_min_cosine` a one-line warning
  naming both runtimes; below, **drift**.

| Who | When | On drift |
|-----|------|----------|
| `vec updatedb`, `update_paths` (`Run::new`) | start of every run, after the model check, before any write | typed `vec_store::EmbeddingDrift` error asking for `vec updatedb --full`; nothing written |
| `vec watch` | start-up and every reload (logged), and every flush (through `update_paths`) | the flush fails, the changes are re-queued with backoff (`retry_max_ms`): indexing pauses and resumes once the runtime is back or the index was rebuilt |
| `vec daemon` | start-up | logged (it serves readers; never fatal) |
| `vec doctor` | always | FAIL |
| queries | never (one embedding per query stays one embedding) | |

An index without a canary (a new index, or one built before canaries existed)
records one at its next write; for a non-empty index this **adopts** the current
runtime as the reference, with a `note:` line in the log.

**`--full` and the canary.** An unscoped `--full` accepts a drifted canary (it
re-embeds every file) and replaces the canary, with `runtime.first_write`, only
**when the run completes**. A run that dies midway leaves the old canary, so the
index (now a mix of runtimes) keeps failing the check, and every writer keeps
refusing, until a `--full` completes. Recording the canary at the start would
make the next incremental run accept a half-rebuilt index. A scoped `--full`
(`--path`) cannot fix drift and is refused. After a model reset the index holds
only the new model's vectors from the first write on, so the reset records the
new canary at once (`Store::reset_model` removes the old one).

## Chunking

`chunk_file` (vec-index) cuts a UTF-8 file into half-open line windows:

1. A window ends `chunk_size` lines after its start, or at end of file.
2. An end before end of file moves to the nearest line within `±boundary_scan`
   that starts (after indentation) with a `boundary_patterns` entry; that line
   starts the next window. The last window always reaches the end of file.
3. The next window starts `chunk_overlap` lines before the end, also snapped to a
   boundary, but always after the previous start and never after the previous
   end, so there are no gaps.
4. A window with fewer than `min_chunk_lines` non-blank lines is merged into the
   previous chunk (or the next one if there is none).

So every line is in a chunk unless the whole file has fewer than
`min_chunk_lines` non-blank lines (then no chunk). Byte offsets come from
cumulative UTF-8 lengths. A property test checks coverage, ordering and offsets.

## Embedding

- **ONNX** (default): tract, pure Rust, in-process. Inputs are mapped by name;
  the output is `sentence_embedding` if present, else `last_hidden_state` /
  `token_embeddings` mean-pooled over the attention mask. One plan with symbolic
  batch and sequence dimensions; `onnx_batch_size` texts per pass, each batch
  padded only to its longest text, batches run in parallel (rayon). The pad id
  comes from the tokenizer.
- **HTTP** (`backend = "ollama"`): `POST {model, input: [...]}` to
  `embed_url + http_endpoint_path`; status, chunked encoding and numeric values
  are checked; `http_concurrency` requests in flight; plain HTTP only. At start
  one probe learns the dimension and one `GET http_tags_path` the model digest;
  `GET http_version_path` gives the runtime.

All vectors are L2-normalised; the store scores with a dot product.

## Embedding daemon

`vec daemon` loads the model once and serves vectors on `[daemon] socket`. It
uses a systemd-passed socket when there is one (`LISTEN_PID`/`LISTEN_FDS`),
otherwise binds the path itself (mode `socket_mode`) after removing a stale
socket; it refuses to replace a live socket or a non-socket. Start-up is eager:
model loaded and hashed before the first accept. One thread per connection up to
`max_connections`; `idle_exit_secs` ends the process after inactivity.

**Protocol v2** (all integers little-endian `u32`):

```
client hello    "VECD" version=2
daemon reply    status len payload      payload = JSON {"name","dim","sha256"}
client request  count, count × (len, UTF-8 text)
daemon reply    status len payload      payload = count × dim f32 LE
status 0 = ok, 1 = error (payload = UTF-8 message, e.g. "busy: ...")
```

Requests are bounded by `max_request_bytes` and `max_texts_per_request`; the
client splits larger batches. Timeouts: [CONFIG.md](CONFIG.md#daemon). A v1 daemon
answers the v2 hello with an error, which the client reports as a protocol
mismatch. The client (`vec_daemon::client`) checks the handshake identity
against the index's model; on any daemon problem `vec-query` warns once and
embeds in-process.

## Real-time watcher

`vec watch` loads config and embedder once (failure, or a model differing from
the index's, is a startup error) and runs four threads:

- **event thread**: drops events the walker would never index (decided from the
  path alone: outside include paths, DB files, hidden, `exclude_dirs`); records
  the rest in a de-duplicated set bounded by `max_pending` (collapse to parent
  directories, then overflow → rescan).
- **watch manager**: owns the non-recursive inotify watches and keeps them equal
  to the directories the walker would enter; new directories are watched then
  re-indexed; ignore-file changes re-decide their directory.
- **SIGHUP thread**: requests a reload. The manager loads config + embedder again;
  on success it swaps them, drops watches outside the new include paths and
  re-indexes the include paths (dropped ones too, which prunes them); on failure
  the old runtime stays.
- **main thread**: flushes after `debounce_ms` of quiet or `max_wait_ms` at most
  via `vec_index::update_paths` (walks and prunes only the changed paths). A
  failed flush is re-queued with exponential backoff up to `retry_max_ms`. Each
  flush writes the `watch.*` meta keys.

After the watcher settles, the index equals what `vec updatedb --full` would
produce: scoped and full runs share the walker and the prune rule
(`scoped_and_full_runs_agree`).

## Deployment

| | System-wide | Userland |
|-|-------------|----------|
| Config | `/etc/vec.conf` | `~/.config/vec/config.toml` |
| Index | `/var/lib/vec/vec.db` (644, root) | `~/.local/share/vec/vec.db` |
| Models | `/usr/share/vec/models/` | `~/.local/share/vec/models/` |
| Daemon socket | `/run/vec/embed.sock` (0666) | `~/.local/share/vec/embed.sock` (0600) |
| Units | `contrib/*.service`, `.timer`, `.socket` | `contrib/user/` (`%h` paths) |

The daily timer is the reconciliation safety net for the watcher.

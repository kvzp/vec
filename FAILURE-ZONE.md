# The Failure Zone

How vec behaves when things go wrong: what triggers each failure, what vec does,
what state the index is left in, how to see it and how to recover. Grounded in
the code; each section names the files to read for the details.

**General rules**

- A command that fails prints `Error: ...` (with the cause chain) on stderr and
  exits **1** (`crates/vec-cli/src/main.rs`). `vec doctor` and `vec config check`
  exit 1 when they find a problem.
- The index is never left half-written: each file's record and all its chunks
  change in one SQLite transaction (`Store::replace_file`), and pruning runs only
  after a walk completed (`crates/vec-store/src/lib.rs`, `crates/vec-index/src/lib.rs`).
- Queries never fail because of the daemon: any daemon problem means in-process
  embedding, with one warning line per process when the daemon exists but cannot
  be used (`crates/vec-query/src/lib.rs`).
- `vec updatedb` exits **0 even when some files failed**; read the final line
  (`errors=N`) and the `warn:` lines.

**Where to look**

| Tool | Shows |
|------|-------|
| `vec doctor` | config validity and layers, model file, model runtime, index schema/model vs configured model, index runtime history, **embedding canary** (re-embedded: ok / warn / FAIL), daemon identity (starts a socket-activated daemon) |
| `vec status [--daemon]` | files, chunks, schema version, index model vs configured, `updatedb.last_run`, embedding runtime of the first and last write, canary runtime (not re-embedded), watcher state |
| `vec config show` | effective values and the layer that set each |
| `vec explain file:line` | chunks of a line, indexed hash, `changed_since_indexed` |
| `vec diff` | what `updatedb` would add, change, remove |
| `journalctl -u vec-watch -u vec-updatedb -u vec-embed` | service logs (`--user` for user units) |
| `meta` table | `model_name`, `embedding_dim`, `model_sha256`, `canary.*`, `runtime.*`, `updatedb.last_run`, `watch.*` |

## Model

Files: `crates/vec-core/src/config.rs` (resolution), `crates/vec-embed/src/lib.rs`,
`crates/vec-store/src/lib.rs` (`ensure_model`, `check_model`, `ModelMismatch`),
`crates/vec-core/src/model_sha.rs`, `crates/vec-index/src/lib.rs` (`Run::new`, `check_canary`, `reset`),
`crates/vec-store/src/canary.rs` (`CanaryCheck`, `EmbeddingDrift`).

| Trigger | What vec does | Index state | Detect | Recover |
|---------|---------------|-------------|--------|---------|
| Model file missing | `[embed] model file not found` / list of searched paths; exit 1. No stub fallback | untouched | `vec doctor` (model FAIL), `vec status` `model_error` | install `vec-model-base` or `vec model download`; fix `model_search_path` |
| Model or tokenizer corrupt / unsupported graph | load error (decode, input/output names, output shape); exit 1; a socket-activated daemon fails to start | untouched | `vec doctor`, `systemctl status vec-embed` | replace the files |
| `[embed] model` changed | writers: `model mismatch: the index at ... was built with ...`, exit 1; queries: same error before any scan | untouched | `vec status` (`model_mismatch`), `vec doctor` | `vec updatedb --full` (or restore the old setting) |
| Model file replaced, same name and dimension | writers hash it: sha256 mismatch, same error as above. Readers check the sha256 only when their hash cache knows the current file, so **a query may run with the new file against old vectors** until a writer runs | untouched | `vec updatedb` or `vec doctor` | `vec updatedb --full` |
| Index built by an older vec (no recorded identity) | adopted if its vectors have the model's dimension, else `ModelMismatch` | untouched | as above | `vec updatedb --full` (required after upgrading, see README) |
| `vec updatedb --full`, same model | re-embeds every file; old entries stay searchable until replaced | consistent | | |
| `vec updatedb --full`, other model | the index is emptied right before the **first successful write**, then refilled. A model that cannot embed leaves the old index intact. `--full --path` with another model is refused | | | |
| Crash or kill during `--full` with another model | files written so far carry the new model; the rest are gone (no staging) | **partial but consistent** (one identity) | `vec status` file count | `vec updatedb` (incremental adds the missing files) |
| Crash during `--full` with the same model | files not yet reached keep their old (compatible) vectors | consistent | | re-run `vec updatedb --full` (incremental would skip them by size+mtime) |
| Ollama model re-pulled under the same tag with other weights | the server's digest (`http_tags_path`) differs from the recorded `model_sha256`: `ModelMismatch` for writers and queries, exit 1. A server that reports no digest: caught by the canary instead (rows below) | untouched | `vec doctor` (index model FAIL) | `vec updatedb --full` (or pull the old weights back) |
| Embedding runtime upgraded (Ollama, tract), vectors moved a little (canary cosine between `canary_min_cosine` and `canary_warn_cosine`) | one line `warn: embedding canary cosine 0.99… (…); runtime when recorded: ollama 0.40.5, now: ollama 0.40.16` per writer run / watcher start; indexing goes on | consistent enough (vectors of both runtimes, close) | `vec doctor` (canary warn), `vec status` (runtime first/last write differ) | optional `vec updatedb --full` |
| **Embedding drift detected** (canary cosine below `canary_min_cosine`: runtime upgrade, re-pull without digest, quantization change) | writers stop before writing: `embedding drift: … cosine … (minimum 0.98); runtime when recorded: …, now: … Re-embed everything with vec updatedb --full`, exit 1 (typed `EmbeddingDrift`). `vec watch` pauses (below). The daemon logs it at start-up. Queries are not checked (doctor/status only) | **untouched** | `vec doctor`: canary FAIL | `vec updatedb --full`, or restore the previous runtime |
| `vec updatedb --full` (unscoped) on a drifted index | warns, re-embeds every file, replaces the canary and `runtime.first_write` **when the run completes**. `--full --path` is refused (it would leave a mix) | rebuilt | | |
| Crash during a `--full` that fixes drift | the old canary stays: the half-rebuilt index keeps failing the check, every writer keeps refusing | mixed runtimes, flagged | `vec doctor` canary FAIL | re-run `vec updatedb --full` to the end |
| Index built before canaries existed (or canary removed) | the next write records one with the current runtime (`note: the index has no embedding canary …`): the current runtime is assumed to have built the index | unchanged | `vec status` (canary recorded with …) | if the runtime changed since the index was built: `vec updatedb --full` |
| Ollama has no model list / version endpoint (other server) | digest and runtime unknown (`None`): identity is name + dimension, runtime `not recorded`; the canary still works | | `vec status` | |

## Ollama / HTTP backend

File: `crates/vec-embed/src/lib.rs` (`Embedder::http`, `HttpClient`, `parse_embeddings`).

| Trigger | What vec does | Index state | Detect / recover |
|---------|---------------|-------------|------------------|
| Server down at start | the embedder probes the endpoint when it is created: `probing embed endpoint ...`, exit 1 (queries, `updatedb`, `watch` start-up) | untouched | start the server |
| Server dies during `updatedb` | a batch fails → files retried one by one → none embeds → probe fails → run stops: `embedding failed for every file of a batch, and for a probe text too`, exit 1 | files written before stay; **nothing pruned** | re-run `vec updatedb` |
| Slow server | connect bounded by `http_connect_timeout_ms`, each request by `http_read_timeout_ms` → error as above | as above | raise the timeouts, lower `batch_size` / `http_concurrency` |
| HTTP status not 2xx | `HTTP status N: <first http_error_preview_chars chars>` | as above | server log |
| Non-JSON body, no `embeddings` array, wrong count, non-numeric or non-finite value | rejected with a specific error (never stored) | as above | |
| One input the server rejects (others work) | that file counts as an error, keeps its previous entry; the run goes on | consistent | `errors=N` + `warn: embedding <file>: ...; its previous index entry is kept` |
| `embed_url` with `https://` | config error (no TLS client) | | use a local plain-HTTP endpoint or a TLS-terminating proxy |

## Indexing and the database

Files: `crates/vec-index/src/lib.rs` (`Run::flush`, `read_file`, `open_no_follow`),
`crates/vec-store/src/lib.rs` (`replace_file`, `migrate`, `open_read_only`).

| Trigger | What vec does | Index state | Detect | Recover |
|---------|---------------|-------------|--------|---------|
| Batch embed failure (ONNX or HTTP) | retry each file alone; failing files → `errors` and `warn:`; if no file embeds, a probe text tells "backend down" (stop, exit 1) from "bad files" (go on) | per-file atomic; nothing pruned on stop | `errors=N`, warn lines | fix the backend / file; re-run |
| Crash, `kill -9`, power loss mid-write | SQLite rolls back the open transaction (WAL, `synchronous=NORMAL`: the last commits may be lost on power loss, never torn) | every file either old or new, never mixed | | re-run `vec updatedb` |
| Disk full / I/O error writing | the transaction fails; the run stops with the error, exit 1 | committed files kept, nothing pruned | | free space, re-run |
| Lock held by another writer (`SQLITE_BUSY`) | waits `busy_timeout_ms` per statement, then `database is locked`, exit 1 (the watcher retries with backoff). Readers are never blocked in WAL mode | untouched | | re-run; raise `busy_timeout_ms` |
| Run killed midway (systemd `TimeoutStartSec` — 1 h in the user unit, 3 h in the system unit —, OOM, SIGTERM) | files written so far stay (one transaction each); the final prune, `updatedb.last_run` and the unit's `ExecStartPost` `vec gc` (vacuum) are skipped | consistent; stale rows and free pages remain until the next complete run | `journalctl -u vec-updatedb` (`Failed with result 'timeout'`), `vec status` (`Last updatedb`) | re-run (resumes where it stopped: unchanged files are skipped); for a first build or a large repair run `vec updatedb` by hand or raise `TimeoutStartSec` |
| Older schema, opened for writing | migrated in place in one transaction (rewrites every vector) | upgraded or untouched | | |
| Older schema, opened by a query (read-only) | `uses schema version N, this vec needs version 1; run vec updatedb`, exit 1 | untouched | `vec status`/`doctor` schema version | `vec updatedb` as the index owner |
| Two processes upgrading at once | the second prints one message and retries the lock for up to `migration_wait_ms`, then fails asking to run `vec updatedb` when idle | consistent | that message | wait, re-run |
| Newer schema than the binary | refused: `newer than this vec supports; upgrade vec` | untouched | | upgrade vec |
| No index yet (query) | `no index at ...: build it with vec updatedb`, exit 1 | | | `vec updatedb` |
| Corrupt row: vector of the wrong length | the search fails: `holds a vector of N bytes where M were expected`, exit 1. A non-finite score is skipped | untouched | the error | `vec updatedb --full` |

## Embedding daemon

Files: `crates/vec-daemon/src/server.rs`, `client.rs`, `deadline.rs`,
`activation.rs`, `protocol.rs`; fallback in `crates/vec-query/src/lib.rs`.

| Trigger | Daemon side | Client side (queries, MCP) | Detect / recover |
|---------|-------------|---------------------------|------------------|
| Not running (socket missing, refused, or permission denied) | | in-process, **silently** | `vec status` (`socket_exists`) |
| All `max_connections` busy | immediate `busy: ...` error reply to the hello, connection closed | warning, in-process | raise `max_connections` |
| Client stalls, trickles or stops reading | one deadline (`io_timeout_ms`) per whole request and per whole reply write → connection dropped. Inference itself is never cut short | | daemon log |
| Daemon slow to answer | | waits `reply_timeout_ms` for a reply to start, then `io_timeout_ms` for the rest → warning, in-process for the rest of the process | raise `reply_timeout_ms` |
| Socket-activated cold start | loads and hashes the model before the first accept | the starting query waits for it (bounded by `reply_timeout_ms`; on timeout it embeds in-process while the daemon keeps starting) | `vec doctor` |
| Daemon serves another model | | identity mismatch → warning, in-process | `vec doctor`: FAIL "serves X, not the index's model"; restart the daemon |
| Daemon starts on a drifted runtime | re-embeds the index's canary at start-up and logs `error: embedding canary cosine …` (keeps serving) | queries use its vectors (not checked per query) | `vec doctor` canary FAIL; `vec updatedb --full` |
| Old (protocol v1) daemon after an upgrade | answers the v2 hello with an error | protocol mismatch → warning, in-process | restart the daemon |
| Oversized / too many texts / invalid UTF-8 | error reply (`request too large`, `too many texts`, `not valid UTF-8`); clients split batches so their own requests never hit the limits | | |
| Embedding fails inside the daemon | `embedding failed (details in the daemon log)` | warning, in-process | daemon log |
| `idle_exit_secs` reached | exits; a socket it bound itself is removed; with systemd the socket stays and the next query starts it again (cold start) | | |
| Stale socket file (daemon crashed) | on start, removed if nothing listens; a live socket (`another daemon is already listening`) or a non-socket file is never replaced → start fails | silent in-process until then | `systemctl status` |
| `accept()` fails (e.g. `EMFILE`) | pause `accept_retry_ms`, retry | | daemon log |

## `vec watch`

File: `crates/vec-watch/src/lib.rs`.

| Trigger | What vec does | Index state | Detect | Recover |
|---------|---------------|-------------|--------|---------|
| Start-up: config, model or model mismatch | error before anything is watched; service fails (`Restart=on-failure`) | untouched | `systemctl status vec-watch` | fix, restart |
| inotify queue overflow | next flush rescans the include paths (watches and index) | converges | `watch.overflows`, `watch.rescans` | none needed |
| More than `max_pending` distinct changes | collapse into parent directories (never above an include path); if still too many, rescan | converges | `watch.overflows` | raise `max_pending` |
| Watch limit (`fs.inotify.max_user_watches`) exhausted | the directory is logged and counted, changes there are missed until the next `vec updatedb` | stale under it | `watch_failures` in `vec status` | raise the limit (`contrib/99-vec.conf`), restart |
| Embedding drift (e.g. Ollama restarted onto a runtime that moves vectors) | start-up / reload: logged (`error: embedding drift …`, not fatal). Each flush re-embeds the canary first and refuses: `error: indexing paused (attempt N), checking again in Xs`, the changes are re-queued with backoff (`debounce_ms` doubling up to `retry_max_ms`); indexing resumes by itself when the old runtime is back or the index was rebuilt | untouched (no mixed vectors) | `watch.last_error`, `watch.pending` in `vec status`, `vec doctor` canary FAIL | `vec updatedb --full` (the watcher resumes at its next retry) |
| Flush fails (DB locked, disk full, backend down) | re-queued, retried after `debounce_ms`, doubling up to `retry_max_ms`; never dropped | per-file atomic | `warn: reindex failed (attempt N) ...`, `watch.last_error`, `watch.last_success` | fix the cause |
| SIGHUP with a bad config or model | `error: reload failed, keeping the current configuration` | unchanged | journal; `watch.reloads` does not grow | fix, reload again |
| Watcher stopped for a while | `startup_rescan` re-indexes at start; the daily timer reconciles anyway | | | |

## Files

Files: `crates/vec-index/src/lib.rs` (`accept`, `read_file`), `crates/vec-core/src/util.rs`
(`read_text_capped`), `crates/vec-core/src/snippet.rs`.

| Trigger | Indexer | Query side |
|---------|---------|------------|
| Unreadable file | `warn: read(...)`, previous record **kept** | dropped by `access()` |
| Unreadable directory | warning, `errors` +1, records under it kept unless the file is definitely gone | |
| Symlink | never followed (not indexed) | snippets/`context` refuse symlinks |
| File or directory swapped for a symlink between walk and read | `O_NOFOLLOW` per component + `(dev, inode)` check → not read | `O_NOFOLLOW` |
| FIFO / device | skipped by type; a FIFO swapped in does not block (`O_NONBLOCK`) | refused (not a regular file) |
| Larger than `max_file_size` / smaller than `min_file_size` | not indexed; an old record is pruned | snippet missing (result shown at chunk start) |
| Non-UTF-8 **name** | skipped, `warn:`, counted (`skipped_non_utf8`) | |
| Non-UTF-8 **content** (binary) | not indexed; an old record is pruned | |
| Too few non-blank lines | no chunk | |
| File edited or shrunk since indexing | | offsets clamped to the live text and to a character boundary: no panic, possibly a wrong snippet or line; `vec explain` shows `changed_since_indexed` |
| Content changed but **size and mtime unchanged** (`touch -r`, some restore tools) | skipped unread: the change is **not seen** | stale vectors | `vec updatedb --full` |
| Same content, new mtime | hash equal → only size/mtime refreshed, vectors reused | | |

## Configuration

File: `crates/vec-core/src/config.rs`.

| Trigger | What vec does | Recover |
|---------|---------------|---------|
| Unknown key or section, in any layer | load error naming the file; every command exits 1 (`vec doctor` reports it) | `vec config check <file>` |
| Invalid value (`chunk_overlap >= chunk_size`, relative path, `min_score` out of range, `reply_timeout_ms < io_timeout_ms`, ...) | `invalid configuration:` with every problem and the file(s) that set the key; exit 1 | fix; [CONFIG.md](CONFIG.md) lists each rule |
| `[embed] daemon_socket` | warning, value used as `[daemon] socket` (ignored with a warning if the same file sets `[daemon] socket`) | move it |
| Running service and a bad edit | `vec watch` keeps its running config on reload; the next `vec-updatedb` run fails | `vec config check` before reloading |

## MCP server

File: `crates/vec-mcp/src/lib.rs`. Every failure is a tool result with
`isError: true` and the message; the server keeps running.

| Trigger | Result |
|---------|--------|
| `context` on a file not in the index, unreadable, a symlink, not regular, over `max_file_size`, or a line outside the file | refused with the reason |
| `similar` / `explain` on an unreadable file | refused |
| `search` with no query, or more than `max_queries` | refused |
| Model mismatch, no index, older schema | the same messages as the CLI (e.g. asks for `vec updatedb --full`) |
| Daemon problems | in-process fallback; the warning goes to the server's stderr |

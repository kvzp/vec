# Known Bugs & Limitations

How each failure behaves and how to recover: [FAILURE-ZONE.md](FAILURE-ZONE.md).

## Indexing

- **`--full` with a new model has no staging area.** The old index is emptied at
  the first successful write; a crash or kill before the run ends leaves a partial
  (but consistent, single-model) index. Recover with `vec updatedb`.
- **Size+mtime skip.** A file whose content changes while its size and mtime stay
  the same (`touch -r`, some restore tools) is not re-read. Use `vec updatedb --full`.
- **`vec updatedb` exits 0 when some files failed** to read or embed; only the
  summary (`errors=N`) and `warn:` lines say so.
- **Not indexed by design:** symlinks (never followed), files with non-UTF-8
  names (skipped and counted), non-UTF-8 content, files outside
  `[min_file_size, max_file_size]`, hidden entries.
- **`onnx_batch_size > 1`** changes the vectors of dynamically quantized (int8)
  models slightly (cosine about 0.98 to the unbatched vector), so index and query
  vectors are no longer bit-identical. The default (1) is deterministic.

## Model identity

- **Queries do not hash the model file.** A model file replaced in place with the
  same name and dimension is detected by writers (`vec updatedb`, `vec watch`,
  `vec daemon`) and by queries only once the per-user sha256 cache knows the new
  file; until a writer runs, queries can compare new-model vectors with old ones.

## Search and diagnostics

- `vec doctor` does not count stale chunk offsets (files edited since indexing);
  `vec explain` shows it per file (`changed_since_indexed`).
- Snippets of files edited since indexing may point at the wrong lines until the
  file is re-indexed (they never panic).

## Backends

- The HTTP (Ollama) backend speaks **plain HTTP only** (`embed_url` must start
  with `http://`): use it on localhost or a trusted network, or behind a local
  TLS-terminating proxy.
- `vec model download` prints `curl` commands instead of downloading (no TLS
  client in the binary).

## `vec watch`

- **Lost-event recovery is a full rescan.** An inotify queue overflow, or more
  than `[watch] max_pending` distinct changes, makes the next flush re-walk the
  include paths: correct, but every file whose size or mtime changed is read again.
- **Watch limit.** A directory that cannot be watched
  (`fs.inotify.max_user_watches` exhausted) is reported (`watch_failures` in
  `vec status`); changes there are only seen by the next `vec updatedb`.
- **The kernel queue overflow path is covered by unit tests only**; forcing a real
  overflow needs root (`fs.inotify.max_queued_events`).

## Untested platforms

- **macOS:** never tested (no hardware). `vec watch` would use FSEvents/kqueue via
  `notify`; no launchd units are provided. A `cargo build && cargo test` report is
  welcome.
- **BSD:** not tested; see [PACKAGING.md](PACKAGING.md#bsd).
- **Windows:** not a target. A few `cfg(not(unix))` fallbacks exist (no daemon,
  open-for-read instead of `access()`), untested.

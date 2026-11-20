# Security Model

## Core properties

- **The index holds no source text.** It stores paths, byte offsets, line
  numbers, file/chunk hashes and embedding vectors (`chunks` has no content
  column; check with `sqlite3 vec.db .schema`). A stolen DB reveals which files
  exist, their sha256 and vectors, not their text. Vectors are not designed to be
  inverted, but they do carry semantic information: treat the DB as you would a
  `locate` database plus a hint of each file's topic.
- **The filesystem is the ACL.** Every search, `similar`, `explain` and `context`
  result is checked with `access(path, R_OK)` for the querying user (real
  UID/GID) before it is shown; unreadable results are dropped silently, before
  the limit is applied. Revoke read access and the result disappears on the next
  query; no re-index needed.
- **Snippets come from the live file**, opened with `O_NOFOLLOW | O_NONBLOCK`,
  required to be a regular file, read up to `[index] max_file_size` bytes.

```
vec "query" (as the user)
  → embed query → scan vectors → keep results with access(path, R_OK)
  → read snippet from the live file → stdout
```

## Indexer

The system units run `vec updatedb` / `vec watch` as root so they can read
protected paths, like `updatedb` for `locate`. Privilege is used only to read:

- `CapabilityBoundingSet=CAP_DAC_READ_SEARCH` (read anything, nothing else),
  `NoNewPrivileges`, `ProtectSystem=strict` with `StateDirectory=vec` as the
  only writable path, `ProtectHome=read-only`, `PrivateDevices`, kernel/cgroup/
  clock protections, `MemoryDenyWriteExecute`, `SystemCallFilter=@system-service`.
  `PrivateTmp` is deliberately **not** set: it would hide `/tmp` and `/var/tmp`
  and their records would be pruned. `AF_INET`/`AF_INET6` are allowed only for the
  optional HTTP backend.
- **Symlink-safe reads.** Symlinks are never followed by the walker. Each file is
  opened component by component below its include path with
  `openat(O_NOFOLLOW)`, and the open file's `(st_dev, st_ino)` must equal the
  walker's `lstat`: swapping a directory or file for a symlink between walk and
  read cannot make a root indexer read another file. `O_NONBLOCK` means a FIFO
  swapped in cannot block it; reads are capped at `max_file_size`.
- Default `exclude_files` skip secret material (`.env`, `.env.*`, `*.key`,
  `*.pem`, `*.p12`, `*.pfx`). Add your own (`secrets.yaml`, `credentials.json`);
  a path appearing in results can leak information even without text.

**Configuration trust.** System services read `/etc/vec.conf` and, as for any
user, root's own per-user file (`~root/.config/vec/config.toml`) unless
`--config` is given. Keep both admin-controlled. A user's per-user config only
affects that user's own commands (their queries still pass `access()`).

## Embedding daemon

Optional (`vec-embed.socket`). It returns vectors for texts a client sends, and
nothing else, so the default socket mode `0666` lets any local user connect
(use `SocketMode=0660` + `SocketGroup=` to restrict). The threat is resource
abuse by a local client; the limits:

| Threat | Mitigation |
|--------|------------|
| Huge request | `max_request_bytes` (headers included), `max_texts_per_request`; checked before reading the body |
| Many cheap texts (CPU, reply size) | `max_texts_per_request`: each text costs one inference and `dim × 4` reply bytes |
| Slowloris (idle, trickling or non-reading client) | one deadline (`io_timeout_ms`) for each whole request and each whole reply write, recomputed before every read/write |
| Connection flood | `max_connections` threads; extra clients get an immediate "busy" reply |
| `accept()` failure loop (EMFILE) | `accept_retry_ms` pause |
| Information leak in errors | clients get "embedding failed (details in the daemon log)"; invalid UTF-8 is rejected |
| Socket hijack | a stale socket is removed only if nothing listens; a live socket or a non-socket file is never replaced |
| Hung/malicious daemon (client side) | `reply_timeout_ms` / `io_timeout_ms` whole-read deadlines, replies bounded by what was asked, identity checked; any problem → in-process embedding |

The system unit runs with `DynamicUser=yes`, `PrivateNetwork=yes`,
`RestrictAddressFamilies=AF_UNIX`, `ProtectHome=yes`, `PrivateTmp=yes`, empty
capability set and the same syscall filter (`systemd-analyze security`: 0.9).
User units use only the seccomp-based options (namespacing needs unprivileged
user namespaces). `vec status` never connects to it unless asked (`--daemon`).

## MCP server

`vec serve` runs as the user who started it, on stdio (no network port). A
prompt-injected client must not turn it into a file reader, so:

- `context` serves only files **in the index**, readable by the user, regular
  files (symlinks not followed), at most `max_file_size` bytes.
- `similar` and `explain` require read access to the file.
- `search` results pass the same `access()` filter as the CLI.

Errors are returned as tool errors (`isError: true`) with a message.

## Embedding backends

ONNX runs in-process: no network, no runtime download. With
`backend = "ollama"`, chunk text is sent over **plain HTTP** to `embed_url`:
use it only on a trusted host or network.

## Out of scope

Encryption at rest (use disk encryption), authentication beyond Unix
credentials, audit logging.

## Supported versions

| Version | Branch | Security fixes |
|---------|--------|----------------|
| 0.1.x | `0.1` | yes |
| pre-release builds (March 2026) | — | no: upgrade to 0.1 |

Fixes are released as patch versions on the version branch ([CONTRIBUTING.md](CONTRIBUTING.md#versions-and-branches)).

## Reporting a vulnerability

Use [GitHub Security Advisories](https://github.com/kvzp/vec/security/advisories/new),
not a public issue. Include what you found, how to reproduce it and the impact.
You will get a response within 7 days.

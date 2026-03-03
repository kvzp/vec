# Packaging Guide

This document is for Linux distribution packagers. It covers everything needed to produce correct, well-integrated packages for `vec`.

---

## Package Split

vec ships as **two or three packages**:

| Package | Content | Type |
|---------|---------|------|
| `vec` | Binary, systemd units, sysctl config, `/etc/vec.conf` | Architecture-dependent |
| `vec-model-base` | `gte-multilingual-base` ONNX + tokenizer (~90MB, 50+ languages) | Architecture-independent (noarch/all) |

`vec` recommends `vec-model-base`. The package is optional — `vec` works without it if you point `model` at an absolute path to a custom ONNX file.

This mirrors how other data-heavy tools are packaged: `tesseract-ocr` + language packs, `hunspell` + dictionaries.

---

## Building `vec`

### Build dependencies

| Distro | Packages |
|--------|---------|
| Debian / Ubuntu | `cargo`, `rustc`, `libsqlite3-dev`, `pkg-config` |
| Fedora / RHEL | `cargo`, `rust`, `sqlite-devel`, `pkg-config` |
| Arch | `rust`, `sqlite`, `pkg-config` |

No `protobuf-compiler`. No Python. No HuggingFace tooling. No network access at build time.

### Build command

```bash
cargo build --release --features system-sqlite
```

| Feature | Effect |
|---------|--------|
| `system-sqlite` | Links against system `libsqlite3` instead of bundling one. **Required for distro packages.** |

The default build (no features) bundles SQLite — fine for development and binary releases, not for distro packages.

### Installed files

```
/usr/bin/vec
/etc/vec.conf                                 # from contrib/vec.conf (all defaults commented)
/etc/systemd/system/vec-updatedb.service      # from contrib/vec-updatedb.service
/etc/systemd/system/vec-updatedb.timer        # from contrib/vec-updatedb.timer
/etc/systemd/system/vec-watch.service         # from contrib/vec-watch.service
/etc/systemd/system/vec-embed.service         # from contrib/vec-embed.service (optional daemon)
/etc/sysctl.d/99-vec.conf                     # from contrib/99-vec.conf (inotify limit)
/usr/share/doc/vec/
```

Install the binary from `target/release/vec`.
Install systemd units from `contrib/`.
Install `/etc/vec.conf` from `contrib/vec.conf`.

### Runtime dependencies

| Distro | Packages |
|--------|---------|
| Debian / Ubuntu | `libsqlite3` |
| Fedora / RHEL | `sqlite-libs` |
| Arch | `sqlite` |

`vec` does **not** hard-depend on a model package — users may point `model` at an absolute path to a custom ONNX file in `/etc/vec.conf`.

---

## Building `vec-model-base`

This package contains pre-built ONNX model files. There is **no compilation step**.

### Source

Download from the vec GitHub Releases page (pinned, versioned, checksummed):

```
https://github.com/kvzp/vec/releases/download/model-base-v<VERSION>/gte-multilingual-base.tar.gz
```

Contents:
- `model_int8.onnx` — int8-quantized ONNX, ~90MB
- `tokenizer.json` — HuggingFace tokenizer config

Verify the SHA-256 checksum published alongside the release.

### Installed files

```
/usr/share/vec/models/gte-multilingual-base/model_int8.onnx
/usr/share/vec/models/gte-multilingual-base/tokenizer.json
```

### Notes

- `noarch` / `all` — ONNX runs on any architecture vec supports.
- Independent version from the `vec` binary.
- The directory name must match the `model` setting in `/etc/vec.conf` (default: `gte-multilingual-base`).

---

## Post-Install Setup

### `vec` postinst

Enable the systemd services for automatic indexing:

```bash
# Enable daily timer (reconciliation + first run at next boot if missed)
systemctl enable vec-updatedb.timer || true
systemctl start  vec-updatedb.timer || true

# Enable real-time watcher
systemctl enable vec-watch.service || true
systemctl start  vec-watch.service || true

# Raise inotify watch limit
sysctl --system || true

# DO NOT enable vec-embed.service here. It is optional — see below.
```

With `Persistent=true` in the timer unit, the first `vec updatedb` run fires at next boot even if the timer was not running when first due.

---

## Optional: Embedding Daemon (`vec-embed.service`)

**`vec-embed.service` must NOT be auto-enabled by the package installer.**

Ship it installed but disabled. Users enable it explicitly if they want it.

### What it does

`vec daemon` loads the ONNX model once and keeps the compiled graph in
resident memory, serving embed requests over `/run/vec/embed.sock`.
`vec <query>` tries the socket first (near-instant) and silently falls back
to in-process model loading if the socket is absent — **no change in
behaviour either way**.

### The cost

| | Without daemon | With daemon |
|--|--|--|
| `vec "query"` startup | 2–5 s (model compile) | ~50 ms (socket) |
| Permanent RAM | 0 MB | **150–300 MB** |

### Who should enable it

Only users who:
1. Run `vec <query>` interactively many times per day, **and**
2. Have RAM to spare (150–300 MB permanently resident is acceptable)

**Do not enable on:** servers with tight memory budgets, containers,
systems where `vec` is used only occasionally.

### User instructions

```bash
# Enable
sudo systemctl enable --now vec-embed.service

# Disable
sudo systemctl disable --now vec-embed.service
```

---

## System Configuration

`/etc/vec.conf` is installed with all settings commented out. Packagers may uncomment and adjust defaults appropriate for their distribution. Do not hardcode values that users may need to override.

---

## Verifying the Package

After installing, verify end-to-end:

```bash
# Binary in place
vec --version

# Model found (or HTTP backend configured)
vec status

# Manual index
vec updatedb

# Search
vec "hello world"
```

`vec status` will report `model not found` if the model directory is missing.

---

## BSD Packaging

vec is BSD-compatible. Portability differences are handled via conditional compilation — no patches needed.

### FreeBSD Ports

Build deps: `lang/rust`, `databases/sqlite3`, `devel/pkgconf`

```bash
cargo build --release --features system-sqlite
```

Install prefix: `/usr/local/`

```
/usr/local/bin/vec
/usr/local/etc/vec.conf
/usr/local/share/vec/models/
/usr/local/share/doc/vec/
```

Data directory: `/var/db/vec/` (BSD convention; configure via `/usr/local/etc/vec.conf`).

`vec-model-en` and `vec-model-multilingual` are separate ports under `textproc/`.

### OpenBSD Ports

Build deps: `lang/rust`, `databases/sqlite3`

```bash
cargo build --release --features system-sqlite
```

Notes:
- No `ionice` equivalent — IO scheduling config is silently ignored
- No per-process CPU affinity — silently ignored

### BSD automatic indexing

Use crontab instead of systemd:

```cron
@hourly  nice -n 19 vec updatedb 2>/dev/null
```

Or a per-user `periodic` entry on FreeBSD.

### BSD file path summary

| Path | Linux | BSD |
|------|-------|-----|
| Binaries | `/usr/bin/` | `/usr/local/bin/` |
| Config | `/etc/vec.conf` | `/usr/local/etc/vec.conf` |
| Models | `/usr/share/vec/models/` | `/usr/local/share/vec/models/` |
| DB | `/var/lib/vec/vec.db` | `/var/db/vec/vec.db` |
| Init | systemd units | crontab / periodic |

---

## Contributor Packaging Files

Ready-to-use packaging templates in `contrib/`:

```
contrib/
├── vec.conf                   # /etc/vec.conf template
├── vec-updatedb.service       # systemd oneshot service
├── vec-updatedb.timer         # daily timer
├── vec-watch.service          # inotify real-time watcher
├── vec-embed.service          # persistent embedding daemon (optional, speeds up queries)
├── 99-vec.conf                # sysctl inotify limit
├── vec.spec                   # RPM spec (binary)
├── vec-model-base.spec        # RPM spec (base model package)
└── debian/                    # Debian packaging for all packages
```

# Packaging Guide

For distribution packagers. Ready-made files: `contrib/debian/`, `contrib/vec.spec`,
`contrib/vec-model-base.spec`.

## Package split

| Package | Content | Arch |
|---------|---------|------|
| `vec` | binary, systemd units, sysctl drop-in, `/etc/vec.conf`, userland units as docs | any |
| `vec-model-base` | `gte-multilingual-base` int8 ONNX + `tokenizer.json` (~90 MB) | all / noarch |

`vec` recommends `vec-model-base` (users may instead point `[embed] model` at
any ONNX file, or use the HTTP backend).

## Building `vec`

| Distro | Build dependencies |
|--------|--------------------|
| Debian / Ubuntu | `cargo`, `rustc`, `libsqlite3-dev`, `pkg-config` |
| Fedora / RHEL | `cargo`, `rust`, `sqlite-devel`, `pkg-config` |
| Arch | `rust`, `sqlite`, `pkg-config` |

**Rust ≥ 1.89** (`rust-version` in `Cargo.toml`); Debian 13 ships 1.85, so use a
newer toolchain there. No Python, no protoc, no network at build time beyond
fetching crates.

```bash
cargo build --release -p vec-cli --no-default-features --features system-sqlite
ldd target/release/vec | grep sqlite      # must list libsqlite3.so.0
```

| Feature | Effect |
|---------|--------|
| `bundled-sqlite` (default) | SQLite compiled in; for development and binary releases |
| `system-sqlite` | links the system `libsqlite3` so its security updates apply. **Use for distro packages** (needs `--no-default-features`) |

Exactly one must be enabled (compile error otherwise). SQLite ≥ 3.35 is required
(`RETURNING`); CI also tests against Ubuntu 24.04's 3.45.1.

**Nothing else to install for configuration**: every default
(`crates/vec-core/defaults.toml`) and the config file locations
(`crates/vec-core/locations.toml`: `/etc/vec.conf`, the per-user path, the
userland data paths, the sha256 cache) are compiled into the binary. To change
a location (e.g. `/usr/local/etc/vec.conf` on BSD), edit `locations.toml` before
building.

## Installed files (`vec`)

```
/usr/bin/vec
/etc/vec.conf                                   contrib/vec.conf (conffile)
/usr/lib/systemd/system/vec-updatedb.service    enabled via the timer only
/usr/lib/systemd/system/vec-updatedb.timer      enable + start
/usr/lib/systemd/system/vec-watch.service       enable + start
/usr/lib/systemd/system/vec-embed.socket        ship DISABLED (optional daemon)
/usr/lib/systemd/system/vec-embed.service       ship DISABLED
/usr/lib/sysctl.d/99-vec.conf                   inotify watch limit
/usr/share/doc/vec/contrib/user/                userland units for users to copy
```

Runtime dependencies: `libsqlite3-0` (Debian), `sqlite-libs` (Fedora),
`sqlite` (Arch). `/var/lib/vec` is created by `StateDirectory=vec`.

## Service policy

- **Enable** `vec-updatedb.timer` (daily, `Persistent=true`) and `vec-watch.service`.
- **Do not enable or start** `vec-updatedb.service` (the timer runs it) or
  `vec-embed.socket` / `vec-embed.service`. The daemon holds the model in RAM
  (150–300 MB) while running; users opt in with
  `systemctl enable --now vec-embed.socket`. Queries work identically without it.
- `vec-watch.service` supports `systemctl reload` (SIGHUP re-reads the config).
- The units are hardened (see [SECURITY.md](SECURITY.md)); keep them as shipped.
  In particular do not add `PrivateTmp=` to the indexer units.
- `contrib/debian/rules` does this with `dh_installsystemd` overrides;
  `contrib/vec.spec` with `%systemd_post` for the timer and watcher only.

## Upgrades

- **From 0.1.x:** the index must be rebuilt. Old versions stored wrong vectors
  and an older schema; until `vec updatedb --full` has run, queries fail with a
  model-mismatch error and `vec-watch` fails at start-up. Tell users (NEWS /
  release notes), or run it from the maintainer script on upgrade from < 0.2 in
  the background, as `vec-model-base` does on model changes. See
  [README.md](README.md#upgrading) and [CHANGELOG.md](CHANGELOG.md).
- **Schema upgrades** happen in place the first time a writer opens the index;
  running `vec updatedb` once before restarting the services avoids services
  waiting on each other (`[database] migration_wait_ms`).
- **Daemon protocol v2:** restart `vec-embed.service` on upgrade (the spec's
  `%systemd_postun_with_restart` does); an old daemon is not used by new clients.

## Building `vec-model-base`

No compilation. Package `model_int8.onnx` and `tokenizer.json` from the
upstream model repository (the URLs are `[embed] model_download_url` /
`model_download_files` in `defaults.toml`; `vec model download` prints them),
pinned by checksum, into:

```
/usr/share/vec/models/gte-multilingual-base/model_int8.onnx
/usr/share/vec/models/gte-multilingual-base/tokenizer.json
```

The directory name must match `[embed] model`. Versioned independently of
`vec`. The model's sha256 is part of the index identity: after a model upgrade
the index must be rebuilt (`vec updatedb --full`), which the provided
postinst/posttrans start in the background.

## Verify

```bash
vec --version
vec doctor            # config, model, index, daemon; exit 1 on a problem
sudo vec updatedb
vec "hello world"
```

## BSD

Portable code paths are `cfg`-gated; not yet tested on BSD. Build with the same
command (`lang/rust`, `databases/sqlite3`, `devel/pkgconf`). Set the BSD
locations in `locations.toml` (`/usr/local/etc/vec.conf`) and the defaults in
the shipped `vec.conf` (`db_path = "/var/db/vec/vec.db"`,
`model_search_path = ["/usr/local/share/vec/models"]`). Use cron instead of
systemd: `@daily nice -n 19 vec updatedb`. `vec watch` uses kqueue through the
`notify` crate (untested).

## `contrib/` layout

```
vec.conf                      /etc/vec.conf template (hand-maintained; a test checks it is a valid layer)
vec-updatedb.{service,timer}  daily index + gc
vec-watch.service             real-time watcher
vec-embed.{socket,service}    optional socket-activated embedding daemon
user/                         the same units for systemd --user (%h paths, socket mode 0600)
99-vec.conf                   sysctl: fs.inotify.max_user_watches
vec.spec, vec-model-base.spec RPM
debian/                       Debian (both packages)
```

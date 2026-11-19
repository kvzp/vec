# Known Bugs & Limitations

## Known Limitations

### `vec watch`

- **Lost-event recovery is a full rescan.** An inotify queue overflow (the kernel drops events) or more than `[watch] max_pending` distinct changes makes the next flush re-walk all include paths: correct, but every file is read again.
- **Watch limit.** A directory that cannot be watched (`fs.inotify.max_user_watches` exhausted) is reported in the log and by `vec status` (`watch_failures`); changes there are only seen by the next `vec updatedb`.
- **`exclude_dirs` / `include_paths` edits need a restart** of `vec watch` (the config is read once). The next incremental `vec updatedb` prunes newly excluded files; `--full` is no longer needed for that.
- **The kernel queue overflow path is covered by unit tests only.** A 40 000-event burst did not overflow the default queue in the end-to-end test; forcing it needs root (`fs.inotify.max_queued_events`).

## Known Bugs

### Packaging

- **`vec-store/system-sqlite` is a no-op.** The feature is empty and `rusqlite` is always built with `bundled`, so `cargo build --features vec-store/system-sqlite` (the PACKAGING.md distro recipe) still embeds its own SQLite instead of linking the system `libsqlite3`. Distro packages therefore do not pick up the distro's SQLite security updates; they get SQLite fixes only through vec releases.

## Untested Platforms

### macOS

vec has **never been tested on macOS**. No Apple hardware is available for testing.

The code is pure Rust with no platform-specific dependencies beyond the `nix` crate (gated behind `cfg(unix)`), so it *should* compile and run. Known risks:

- **inotify → kqueue**: `vec watch` uses the `notify` crate which abstracts over platform-specific APIs. On macOS this means FSEvents/kqueue instead of inotify. Untested — debounce timing and event semantics may differ.
- **`access(path, R_OK)`**: Used for per-user permission checks via the `nix` crate. Should work on macOS (POSIX) but has not been verified.
- **systemd user units**: Not applicable on macOS. Users would need launchd plists instead (not provided).
- **Model loading (tract-onnx)**: tract supports macOS including Apple Silicon, but the ONNX patching in `embed.rs` (clearing `value_info` for symbolic dims) has only been tested on x86_64 Linux.

**Contributors with macOS hardware welcome** — even a `cargo build && cargo test` report would be valuable.

### Windows

Not a target platform. The code has `cfg(not(unix))` fallbacks for config paths (`C:\ProgramData\vec\`) but these are placeholders — no testing has been done.

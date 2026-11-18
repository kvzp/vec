# Known Bugs & Limitations

## Known Limitations

### `vec watch`

- **Excluded directories still hold inotify watches.** `vec watch` places one recursive watch per include path, so excluded trees (`target/`, `node_modules/`) still count toward `fs.inotify.max_user_watches`. Their events are dropped at intake, so they cost no memory or indexing work. Watching only non-excluded directories would need manual tracking of newly created directories (#2).
- **Ignore-rule changes are not applied retroactively.** Editing a `.gitignore` (or `exclude_dirs`) does not index newly un-ignored files, or remove newly ignored ones, until the next `vec updatedb` run.
- **Pending-set size is not shown in `vec status`.** It is logged at each flush only.

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

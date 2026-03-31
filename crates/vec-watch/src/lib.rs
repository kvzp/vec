// src/watch.rs — Real-time filesystem watcher for `vec watch`.
//
// Uses inotify (via the `notify` crate) to watch configured include_paths
// for file changes.  Events are debounced: a 3-second quiet window after
// the last event triggers re-indexing of only the changed files.
//
// Architecture:
//   - notify::RecommendedWatcher sends raw events over a std channel
//   - A tokio task drains the channel, debounces, and calls run_updatedb
//     per changed path
//   - Deletions are handled by run_updatedb's normal delete_missing logic
//
// Inotify watch limit: /proc/sys/fs/inotify/max_user_watches defaults to
// 8192 on many kernels.  The package postinst should set:
//   fs.inotify.max_user_watches = 65536
// via /etc/sysctl.d/99-vec.conf.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};

use vec_core::config::Config;
use vec_index::run_updatedb;
use vec_store::Store;

/// Debounce window: wait this long after the last event before re-indexing.
const DEBOUNCE_SECS: u64 = 3;

/// Run the real-time watcher. Blocks until killed (SIGTERM/SIGINT).
pub fn run_watch() -> Result<()> {
    let cfg = Config::load(None).context("loading config")?;

    anstream::eprintln!(
        "vec watch: monitoring {} path(s) with {}s debounce",
        cfg.index.include_paths.len(),
        DEBOUNCE_SECS,
    );

    // Channel: notify → our event loop.
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();

    let mut watcher = notify::RecommendedWatcher::new(tx, notify::Config::default())
        .context("creating filesystem watcher")?;

    for path in &cfg.index.include_paths {
        if path.exists() {
            watcher
                .watch(path, RecursiveMode::Recursive)
                .with_context(|| format!("watching {}", path.display()))?;
            anstream::eprintln!("  watching: {}", path.display());
        } else {
            anstream::eprintln!("  warn: path does not exist, skipping: {}", path.display());
        }
    }

    // Resolve the DB directory so we can filter out self-triggered events.
    let db_dir = cfg
        .database
        .db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/"))
        .to_path_buf();

    let mut pending: HashSet<PathBuf> = HashSet::new();
    let debounce = Duration::from_secs(DEBOUNCE_SECS);

    loop {
        // Block until an event arrives, then drain any that follow quickly.
        match rx.recv_timeout(debounce) {
            Ok(Ok(event)) => {
                collect_paths(&event, &mut pending, &db_dir);
                // Drain any additional events that arrive within 100ms.
                while let Ok(Ok(ev)) = rx.recv_timeout(Duration::from_millis(100)) {
                    collect_paths(&ev, &mut pending, &db_dir);
                }
                // Keep looping — more events may arrive within the debounce window.
            }
            Ok(Err(e)) => {
                anstream::eprintln!("watch error: {e}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Debounce window expired — process pending paths now.
                if !pending.is_empty() {
                    let paths: Vec<PathBuf> = pending.drain().collect();
                    index_paths(&cfg, &paths);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anstream::eprintln!("watch channel closed, exiting");
                break;
            }
        }
    }

    Ok(())
}

/// Extract relevant file paths from a notify event.
fn collect_paths(event: &Event, pending: &mut HashSet<PathBuf>, db_dir: &Path) {
    match event.kind {
        // We care about creates, modifications, and removes.
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
            for path in &event.paths {
                // Skip events from the vec database directory — writing to the
                // DB triggers inotify events which would cause a feedback loop.
                if path.starts_with(db_dir) {
                    continue;
                }
                // Skip directories — we index files only.
                if path.is_file() || matches!(event.kind, EventKind::Remove(_)) {
                    pending.insert(path.clone());
                }
            }
        }
        _ => {}
    }
}

/// Re-index the given list of changed files.
fn index_paths(cfg: &Config, paths: &[PathBuf]) {
    // Deduplicate by parent directory — if many files in the same dir changed
    // at once (e.g. git checkout), index the dir once rather than per-file.
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    for path in paths {
        if let Some(parent) = path.parent() {
            dirs.insert(parent.to_path_buf());
        } else {
            dirs.insert(path.clone());
        }
    }

    for dir in &dirs {
        anstream::eprintln!("reindexing: {}", dir.display());

        let mut store = match Store::open(&cfg.database.db_path, cfg.database.wal) {
            Ok(s) => s,
            Err(e) => {
                anstream::eprintln!("warn: could not open store: {e}");
                continue;
            }
        };

        let embedder = vec_core::load_embedder(cfg);

        match run_updatedb(
            &mut store,
            &embedder,
            cfg,
            false,     // not a full re-index
            Some(dir), // scope to this directory
            |msg| anstream::eprintln!("{msg}"),
        ) {
            Ok(stats) => {
                if stats.files_updated > 0 || stats.files_deleted > 0 {
                    anstream::eprintln!(
                        "  updated={} deleted={} chunks_added={}",
                        stats.files_updated,
                        stats.files_deleted,
                        stats.chunks_added,
                    );
                }
            }
            Err(e) => {
                anstream::eprintln!("warn: updatedb error for {}: {e}", dir.display());
            }
        }
    }
}

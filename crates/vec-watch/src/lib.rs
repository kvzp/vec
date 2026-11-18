// vec-watch — Real-time filesystem watcher for `vec watch`.
//
// Uses inotify (via the `notify` crate) to watch configured include_paths.
//
// Architecture:
//   - notify's event thread runs our handler closure. It drops events the
//     indexer would ignore anyway (exclude_dirs, exclude_files, hidden,
//     .gitignore, the DB directory, paths outside include_paths) and inserts
//     the rest into a shared, de-duplicated `Pending` set. Memory is bounded
//     by the number of distinct changed paths (capped by [watch] max_pending),
//     never by the number of events, and intake never waits on indexing.
//   - The main thread sleeps on a condvar and flushes the set when either no
//     event arrived for [watch] debounce_ms, or the oldest pending change is
//     [watch] max_wait_ms old — so constant churn cannot starve indexing.
//   - A flush opens the store and loads the embedder once, then calls
//     `vec_index::update_paths`, which walks and prunes only the changed paths.
//
// Inotify watch limit: /proc/sys/fs/inotify/max_user_watches defaults to
// 8192 on many kernels. The package postinst sets 65536 via
// /etc/sysctl.d/99-vec.conf. Excluded directories still receive a watch
// (one recursive watch per include path) — see BUGS.md.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};

use vec_core::config::{Config, WatchConfig};
use vec_index::{IndexFilter, update_paths};
use vec_store::Store;

/// Run the real-time watcher. Blocks until killed (SIGTERM/SIGINT).
pub fn run_watch() -> Result<()> {
    let cfg = Config::load(None).context("loading config")?;
    let wcfg = cfg.watch.clone();

    anstream::eprintln!(
        "vec watch: monitoring {} path(s), debounce {} ms, max wait {} ms, max pending {}",
        cfg.index.include_paths.len(),
        wcfg.debounce_ms,
        wcfg.max_wait_ms,
        wcfg.max_pending,
    );

    let shared = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
    let mut filter = IndexFilter::new(&cfg, &|msg| anstream::eprintln!("{msg}"));

    let handler_shared = Arc::clone(&shared);
    let max_pending = wcfg.max_pending;
    let handler = move |res: notify::Result<Event>| match res {
        Ok(event) => {
            let paths = relevant_paths(&event, &mut filter);
            if paths.is_empty() {
                return;
            }
            let (lock, cvar) = &*handler_shared;
            let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            for p in paths {
                pending.insert(p, now, max_pending);
            }
            cvar.notify_one();
        }
        Err(e) => anstream::eprintln!("watch error: {e}"),
    };

    let mut watcher = notify::RecommendedWatcher::new(handler, notify::Config::default())
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

    loop {
        let batch = wait_for_batch(&shared, &wcfg);
        anstream::eprintln!(
            "reindexing {} path(s) from {} event(s), oldest change {:.1}s ago",
            batch.paths.len(),
            batch.events,
            batch.age.as_secs_f64(),
        );
        index_batch(&cfg, &batch.paths);
    }
}

/// A set of changed paths ready to be indexed.
struct Batch {
    paths: Vec<PathBuf>,
    events: u64,
    age: Duration,
}

/// Block until the pending set is due, then take it.
fn wait_for_batch(shared: &(Mutex<Pending>, Condvar), wcfg: &WatchConfig) -> Batch {
    let (lock, cvar) = shared;
    let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let now = Instant::now();
        pending = match pending.time_until_due(now, wcfg) {
            None => cvar.wait(pending).unwrap_or_else(|e| e.into_inner()),
            Some(d) if d.is_zero() => return pending.take(now),
            Some(d) => {
                cvar.wait_timeout(pending, d)
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            }
        };
    }
}

/// Paths from `event` that the indexer could care about.
fn relevant_paths(event: &Event, filter: &mut IndexFilter) -> Vec<PathBuf> {
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for path in &event.paths {
        // Ignore rules changed: drop cached matchers. (Files un-ignored by the
        // change are picked up by the next `vec updatedb`.)
        if path.file_name().is_some_and(|n| n == ".gitignore")
            || path.ends_with(".git/info/exclude")
        {
            filter.invalidate();
        }
        // No stat here: directories are walked, missing paths are pruned.
        if !filter.is_excluded(path, false) {
            out.push(path.clone());
        }
    }
    out
}

/// Re-index one batch: one store, one embedder, one scoped update.
fn index_batch(cfg: &Config, paths: &[PathBuf]) {
    let mut store = match Store::open(&cfg.database.db_path, cfg.database.wal) {
        Ok(s) => s,
        Err(e) => {
            anstream::eprintln!("warn: could not open store: {e}");
            return;
        }
    };
    let embedder = vec_core::load_embedder(cfg);

    match update_paths(&mut store, &embedder, cfg, paths, |msg| {
        anstream::eprintln!("{msg}")
    }) {
        Ok(stats) => anstream::eprintln!(
            "  visited={} updated={} deleted={} chunks_added={} errors={}",
            stats.files_visited,
            stats.files_updated,
            stats.files_deleted,
            stats.chunks_added,
            stats.errors,
        ),
        Err(e) => anstream::eprintln!("warn: update error: {e:#}"),
    }
}

// ---------------------------------------------------------------------------
// Pending — the bounded, de-duplicated set of changed paths
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Pending {
    paths: HashSet<PathBuf>,
    first_seen: Option<Instant>,
    last_event: Option<Instant>,
    events: u64,
}

impl Pending {
    fn insert(&mut self, path: PathBuf, now: Instant, max_pending: usize) {
        self.paths.insert(path);
        self.first_seen.get_or_insert(now);
        self.last_event = Some(now);
        self.events += 1;
        if self.paths.len() > max_pending {
            self.collapse(max_pending);
        }
    }

    /// Replace paths by their parent directories until at most `max` remain
    /// (or nothing collapses further). Indexing a parent re-walks it, and
    /// unchanged files are skipped by hash, so this trades CPU for memory.
    fn collapse(&mut self, max: usize) {
        while self.paths.len() > max {
            let parents: HashSet<PathBuf> = self
                .paths
                .iter()
                .map(|p| p.parent().map_or_else(|| p.clone(), PathBuf::from))
                .collect();
            if parents.len() == self.paths.len() {
                break;
            }
            self.paths = parents;
        }
    }

    /// `None`: nothing pending. Otherwise how long until a flush is due
    /// (zero = now): after `debounce_ms` of quiet, or `max_wait_ms` after the
    /// first pending change, whichever comes first.
    fn time_until_due(&self, now: Instant, w: &WatchConfig) -> Option<Duration> {
        let (first, last) = (self.first_seen?, self.last_event?);
        let quiet = last + Duration::from_millis(w.debounce_ms);
        let cap = first + Duration::from_millis(w.max_wait_ms);
        Some(quiet.min(cap).saturating_duration_since(now))
    }

    fn take(&mut self, now: Instant) -> Batch {
        let age = self
            .first_seen
            .map_or(Duration::ZERO, |f| now.saturating_duration_since(f));
        let taken = std::mem::take(self);
        Batch {
            paths: taken.paths.into_iter().collect(),
            events: taken.events,
            age,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, EventKind};

    fn wcfg() -> WatchConfig {
        WatchConfig {
            debounce_ms: 3_000,
            max_wait_ms: 30_000,
            max_pending: 4,
        }
    }

    #[test]
    fn debounce_fires_after_quiet_window() {
        let w = wcfg();
        let t0 = Instant::now();
        let mut p = Pending::default();
        assert_eq!(p.time_until_due(t0, &w), None, "empty set is never due");
        p.insert("/a/x".into(), t0, w.max_pending);
        assert!(
            !p.time_until_due(t0 + Duration::from_secs(2), &w)
                .unwrap()
                .is_zero()
        );
        assert!(
            p.time_until_due(t0 + Duration::from_secs(3), &w)
                .unwrap()
                .is_zero()
        );
    }

    #[test]
    fn constant_churn_still_flushes_at_max_wait() {
        let w = wcfg();
        let t0 = Instant::now();
        let mut p = Pending::default();
        // An event every second, forever: the quiet window never opens.
        let mut flushed_at = None;
        for s in 0..60u64 {
            let now = t0 + Duration::from_secs(s);
            p.insert(format!("/a/{s}").into(), now, 1_000);
            if p.time_until_due(now, &w).unwrap().is_zero() {
                flushed_at = Some(s);
                break;
            }
        }
        assert_eq!(flushed_at, Some(30), "max_wait must bound latency");
        let batch = p.take(t0 + Duration::from_secs(30));
        assert_eq!(batch.events, 31);
        assert_eq!(p.time_until_due(t0, &w), None, "take() resets the set");
    }

    #[test]
    fn many_events_on_one_path_use_one_slot() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        for _ in 0..100_000 {
            p.insert("/a/same.rs".into(), t0, 4);
        }
        assert_eq!(p.paths.len(), 1);
        assert_eq!(p.events, 100_000);
    }

    #[test]
    fn pending_cap_collapses_to_parents() {
        let t0 = Instant::now();
        let mut p = Pending::default();
        for d in ["a", "b"] {
            for f in 0..10 {
                p.insert(format!("/r/{d}/{f}.rs").into(), t0, 4);
            }
        }
        assert!(p.paths.len() <= 4, "cap respected: {:?}", p.paths);
        // Nothing is lost: every changed file is still covered by a pending path.
        for d in ["a", "b"] {
            for f in 0..10 {
                let file = PathBuf::from(format!("/r/{d}/{f}.rs"));
                assert!(
                    p.paths.iter().any(|q| file.starts_with(q)),
                    "{} not covered by {:?}",
                    file.display(),
                    p.paths
                );
            }
        }
    }

    fn test_cfg(dir: &tempfile::TempDir) -> Config {
        let mut cfg = Config::load(None).unwrap();
        cfg.index.include_paths = vec![dir.path().to_path_buf()];
        cfg.index.exclude_dirs = vec!["target".into()];
        cfg.index.exclude_files = vec!["*.lock".into()];
        cfg.index.gitignore = true;
        cfg.database.db_path = dir.path().join("db/vec.db");
        cfg
    }

    fn create(path: PathBuf) -> Event {
        Event::new(EventKind::Create(CreateKind::File)).add_path(path)
    }

    #[test]
    fn excluded_paths_never_reach_pending() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join(".gitignore"),
            "builds/
",
        )
        .unwrap();
        let cfg = test_cfg(&dir);
        let mut filter = IndexFilter::new(&cfg, &|_| {});
        let root = dir.path();

        for excluded in [
            root.join("target/debug/deps/x.o"),           // exclude_dirs
            root.join("sub/target/y.rs"),                 // exclude_dirs, nested
            root.join("Cargo.lock"),                      // exclude_files
            root.join(".cache/z"),                        // hidden
            root.join("builds/out/a.txt"),                // .gitignore
            root.join("db/vec.db-wal"),                   // database directory
            PathBuf::from("/definitely/not/included.rs"), // outside include_paths
        ] {
            let ev = create(excluded.clone());
            assert!(
                relevant_paths(&ev, &mut filter).is_empty(),
                "{} should be filtered",
                excluded.display()
            );
        }
        let kept = root.join("src/main.rs");
        assert_eq!(
            relevant_paths(&create(kept.clone()), &mut filter),
            vec![kept]
        );
    }
}

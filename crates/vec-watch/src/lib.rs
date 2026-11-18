// vec-watch — Real-time filesystem watcher for `vec watch`.
//
// Correctness rule: the indexer's walker is the only judge of what belongs in
// the index. Nothing here re-implements its rules in a way that could
// disagree; shortcuts are only taken where they are exact.
//
// Architecture (three threads):
//   - notify's event thread runs `Handler::handle`. It drops events the
//     walker can never index, decided exactly from the path alone
//     (`IndexFilter`: outside include_paths, DB directory, hidden component,
//     exclude_dirs), and records the rest in a de-duplicated, bounded
//     `Pending` set. Directory-shaped events, and changes to .gitignore /
//     .ignore / .git / .git/info/exclude, go to the reconcile queue.
//   - The watch manager thread reconciles watches: for a directory it walks
//     it with the real walker (`vec_index::visit_dirs`) and makes the set of
//     non-recursive inotify watches under it equal the set of directories the
//     walker would enter — adding new ones, removing deleted, moved-out and
//     newly ignored ones. Excluded and ignored trees never get a watch. After
//     watching a new directory, or after an ignore-rule change, it queues the
//     directory for re-indexing, so files written before the watch existed
//     (or newly (un)ignored files) are picked up.
//   - The main thread flushes `Pending` after [watch] debounce_ms of quiet or
//     at most [watch] max_wait_ms after the first change, and runs
//     `vec_index::update_paths`, which walks and prunes exactly the changed
//     paths. A failed flush is re-queued, never dropped.
//
// Lost information is never ignored: an inotify queue overflow, or more
// distinct changes than [watch] max_pending, degrades to a rescan of the
// include paths (watches and index).
//
// Inotify watch limit: /proc/sys/fs/inotify/max_user_watches defaults to
// 8192 on many kernels. The package postinst sets 65536 via
// /etc/sysctl.d/99-vec.conf. Watch failures are reported by `vec status`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{Event, EventKind, RecursiveMode, Watcher};

use vec_core::config::{Config, WatchConfig};
use vec_index::{IndexFilter, update_paths, visit_dirs};
use vec_store::{Store, is_gone};

/// Run the real-time watcher. Blocks until killed (SIGTERM/SIGINT).
/// `cfg_path` is the `--config` override, as for every other subcommand.
pub fn run_watch(cfg_path: Option<&Path>) -> Result<()> {
    let cfg = Config::load(cfg_path).context("loading config")?;
    let wcfg = cfg.watch.clone();
    let log = |msg: &str| anstream::eprintln!("{msg}");

    anstream::eprintln!(
        "vec watch: monitoring {} path(s), debounce {} ms, max wait {} ms, max pending {}",
        cfg.index.include_paths.len(),
        wcfg.debounce_ms,
        wcfg.max_wait_ms,
        wcfg.max_pending,
    );

    let pending: Shared<Pending> = shared(Pending::new(cfg.index.include_paths.clone()));
    let queue: Shared<ReconcileQueue> = shared(ReconcileQueue::default());

    let handler = Handler {
        filter: IndexFilter::new(&cfg, &log),
        pending: Arc::clone(&pending),
        queue: Arc::clone(&queue),
        max_pending: wcfg.max_pending,
    };
    let watcher = notify::RecommendedWatcher::new(
        move |res: notify::Result<Event>| handler.handle(res),
        notify::Config::default(),
    )
    .context("creating filesystem watcher")?;
    let watches = Arc::new(Mutex::new(Watches::new(watcher)));

    // Initial watches, before anything else, so no change can slip between
    // the startup rescan and the first watch.
    let started = Instant::now();
    for path in &cfg.index.include_paths {
        if !path.exists() {
            anstream::eprintln!("  warn: path does not exist, skipping: {}", path.display());
        }
    }
    let r = lock(&watches).reconcile(&cfg, &cfg.index.include_paths);
    anstream::eprintln!(
        "  watching {} directories ({} failed) in {:.1}s",
        r.added,
        r.failed,
        started.elapsed().as_secs_f64()
    );

    // Changes made while the watcher was not running are unknown: rescan.
    if wcfg.startup_rescan {
        let (lock_p, cvar) = &*pending;
        lock(lock_p).force_rescan(Instant::now());
        cvar.notify_one();
    }

    {
        let cfg = cfg.clone();
        let watches = Arc::clone(&watches);
        let pending = Arc::clone(&pending);
        let queue = Arc::clone(&queue);
        std::thread::Builder::new()
            .name("vec-watch-manager".into())
            .spawn(move || watch_manager(&cfg, &watches, &queue, &pending))
            .context("spawning watch manager thread")?;
    }

    let mut failures: u32 = 0;
    loop {
        let batch = wait_for_batch(&pending, &wcfg);
        let paths = if batch.rescan {
            anstream::eprintln!("rescanning include paths");
            cfg.index.include_paths.clone()
        } else {
            batch.paths.clone()
        };
        anstream::eprintln!(
            "reindexing {} path(s) from {} event(s), oldest change {:.1}s ago",
            paths.len(),
            batch.events,
            batch.age.as_secs_f64(),
        );
        let outcome = index_batch(&cfg, &paths);
        let (watched, failed) = {
            let w = lock(&watches);
            (w.dirs.len(), w.failed.len())
        };
        if let Err(e) = &outcome {
            // Never drop changes: put them back and retry with exponential
            // backoff (a broken database must not become a busy loop).
            failures = failures.saturating_add(1);
            let delay = retry_delay(&wcfg, failures);
            anstream::eprintln!(
                "warn: reindex failed (attempt {failures}), retrying in {:.1}s: {e:#}",
                delay.as_secs_f64()
            );
            let (lock_p, cvar) = &*pending;
            let now = Instant::now();
            lock(lock_p).requeue(batch, now, wcfg.max_pending, now + delay);
            cvar.notify_one();
        } else {
            failures = 0;
        }
        write_status(&cfg, &paths, outcome.as_ref().err(), watched, failed);
    }
}

/// Delay before retry number `failures` (1-based): `debounce_ms` doubled per
/// failure, capped at `retry_max_ms`.
fn retry_delay(w: &WatchConfig, failures: u32) -> Duration {
    let factor = 1u64 << failures.saturating_sub(1).min(32);
    Duration::from_millis(w.debounce_ms.saturating_mul(factor).min(w.retry_max_ms))
}

type Shared<T> = Arc<(Mutex<T>, Condvar)>;

fn shared<T>(v: T) -> Shared<T> {
    Arc::new((Mutex::new(v), Condvar::new()))
}

/// Lock, recovering from a poisoned mutex (the data stays consistent: every
/// critical section here is a few plain field updates).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Event intake (notify's thread)
// ---------------------------------------------------------------------------

struct Handler {
    filter: IndexFilter,
    pending: Shared<Pending>,
    queue: Shared<ReconcileQueue>,
    max_pending: usize,
}

/// What one event asks for.
#[derive(Default, Debug, PartialEq)]
struct Triage {
    /// Paths whose index entries may have changed.
    reindex: Vec<PathBuf>,
    /// `(directory, reindex afterwards)` whose watches must be reconciled.
    reconcile: Vec<(PathBuf, bool)>,
    /// Events were lost: rescan everything.
    rescan: bool,
}

impl Handler {
    fn handle(&self, res: notify::Result<Event>) {
        let t = match res {
            Ok(event) => triage(&event, &self.filter),
            Err(e) => {
                // An error from the backend may mean lost events.
                anstream::eprintln!("watch error: {e} — scheduling a rescan");
                Triage {
                    rescan: true,
                    ..Default::default()
                }
            }
        };
        let now = Instant::now();
        if t.rescan || !t.reconcile.is_empty() {
            let (l, cvar) = &*self.queue;
            let mut q = lock(l);
            if t.rescan {
                q.overflow = true;
            }
            for (dir, reindex) in t.reconcile {
                q.insert(dir, reindex, self.max_pending);
            }
            cvar.notify_one();
        }
        if t.rescan || !t.reindex.is_empty() {
            let (l, cvar) = &*self.pending;
            let mut p = lock(l);
            if t.rescan {
                p.force_rescan(now);
            }
            for path in t.reindex {
                p.insert(path, now, self.max_pending);
            }
            cvar.notify_one();
        }
    }
}

/// Decide what an event requires. Pure apart from `stat` calls on
/// directory-shaped events.
fn triage(event: &Event, filter: &IndexFilter) -> Triage {
    let mut t = Triage::default();
    if event.need_rescan() {
        // The kernel queue overflowed: events were lost.
        t.rescan = true;
        return t;
    }
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    ) {
        return t;
    }
    for path in &event.paths {
        // Ignore-rule changes: re-decide the directory they govern.
        if let Some(dir) = ignore_rule_scope(path) {
            if !filter.is_excluded(&dir) {
                t.reconcile.push((dir, true));
            }
            continue;
        }
        if filter.is_excluded(path) {
            continue;
        }
        t.reindex.push(path.clone());
        if may_be_dir_change(&event.kind, path) {
            t.reconcile.push((path.clone(), false));
        }
    }
    t
}

/// If `path` is a file whose change alters the walker's rules, the
/// directory whose contents those rules govern.
fn ignore_rule_scope(path: &Path) -> Option<PathBuf> {
    if path.ends_with(".git/info/exclude") || path.ends_with(".git/info") {
        // <repo>/.git/info[/exclude] governs <repo>.
        return path
            .ancestors()
            .find(|a| a.file_name().is_some_and(|n| n == ".git"))
            .and_then(Path::parent)
            .map(Path::to_path_buf);
    }
    let name = path.file_name()?;
    // .git: creating or removing a repository switches .gitignore on or off.
    (name == ".gitignore" || name == ".ignore" || name == ".git")
        .then(|| path.parent().map(Path::to_path_buf))
        .flatten()
}

/// Could this event have added or removed a directory at `path`?
fn may_be_dir_change(kind: &EventKind, path: &Path) -> bool {
    match kind {
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => true,
        EventKind::Create(CreateKind::File) | EventKind::Remove(RemoveKind::File) => false,
        EventKind::Create(_) => path.is_dir(),
        EventKind::Remove(_) => true,
        // A rename: the old name is gone (may have been a directory), the new
        // name may be a directory.
        EventKind::Modify(ModifyKind::Name(_)) | EventKind::Any => is_gone(path) || path.is_dir(),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Watch manager
// ---------------------------------------------------------------------------

/// Directories waiting for reconciliation. Bounded: past the cap it degrades
/// to "reconcile the include paths".
#[derive(Default)]
struct ReconcileQueue {
    dirs: HashMap<PathBuf, bool>,
    overflow: bool,
}

impl ReconcileQueue {
    fn insert(&mut self, dir: PathBuf, reindex: bool, max: usize) {
        if self.overflow {
            return;
        }
        *self.dirs.entry(dir).or_insert(false) |= reindex;
        if self.dirs.len() > max {
            self.dirs.clear();
            self.overflow = true;
        }
    }

    fn is_empty(&self) -> bool {
        self.dirs.is_empty() && !self.overflow
    }
}

fn watch_manager(
    cfg: &Config,
    watches: &Mutex<Watches>,
    queue: &(Mutex<ReconcileQueue>, Condvar),
    pending: &(Mutex<Pending>, Condvar),
) {
    let (lq, cq) = queue;
    loop {
        let job = {
            let mut q = lock(lq);
            while q.is_empty() {
                q = cq.wait(q).unwrap_or_else(|e| e.into_inner());
            }
            std::mem::take(&mut *q)
        };
        let (dirs, reindex): (Vec<PathBuf>, Vec<PathBuf>) = if job.overflow {
            anstream::eprintln!("re-walking include paths to reconcile watches");
            let roots = cfg.index.include_paths.clone();
            (roots.clone(), roots)
        } else {
            let reindex = job
                .dirs
                .iter()
                .filter(|(_, r)| **r)
                .map(|(d, _)| d.clone())
                .collect();
            (job.dirs.into_keys().collect(), reindex)
        };

        let r = lock(watches).reconcile(cfg, &dirs);
        if r.added + r.removed + r.failed > 0 {
            anstream::eprintln!(
                "  watches: +{} -{} ({} failed)",
                r.added,
                r.removed,
                r.failed
            );
        }

        // Re-index what may have been missed: newly watched directories
        // (files written before their watch existed) and directories whose
        // ignore rules changed.
        let mut again: Vec<PathBuf> = r.new_dirs;
        again.extend(reindex);
        if !again.is_empty() {
            let (lp, cp) = pending;
            let mut p = lock(lp);
            let now = Instant::now();
            for d in again {
                p.insert(d, now, cfg.watch.max_pending);
            }
            cp.notify_one();
        }
    }
}

struct Watches {
    watcher: notify::RecommendedWatcher,
    /// Directories currently watched. Ordered so that a directory's
    /// descendants form one contiguous range.
    dirs: BTreeSet<PathBuf>,
    /// Directories the walker would enter but that could not be watched.
    failed: BTreeSet<PathBuf>,
}

#[derive(Default, Debug)]
struct ReconcileResult {
    added: usize,
    removed: usize,
    failed: usize,
    /// Directories that got a watch for the first time.
    new_dirs: Vec<PathBuf>,
}

impl Watches {
    fn new(watcher: notify::RecommendedWatcher) -> Self {
        Self {
            watcher,
            dirs: BTreeSet::new(),
            failed: BTreeSet::new(),
        }
    }

    /// Make the watches under each of `scopes` equal to the set of
    /// directories the walker would enter there.
    fn reconcile(&mut self, cfg: &Config, scopes: &[PathBuf]) -> ReconcileResult {
        let scopes = outermost(scopes);
        let mut want: BTreeSet<PathBuf> = BTreeSet::new();
        visit_dirs(cfg, &scopes, &|msg| anstream::eprintln!("{msg}"), |d| {
            want.insert(d.to_path_buf());
        });

        let mut r = ReconcileResult::default();
        for scope in &scopes {
            let stale: Vec<PathBuf> = under(&self.dirs, scope)
                .filter(|d| !want.contains(*d))
                .cloned()
                .collect();
            for d in stale {
                let _ = self.watcher.unwatch(&d); // the kernel may have dropped it already
                self.dirs.remove(&d);
                r.removed += 1;
            }
            let gone_failures: Vec<PathBuf> = under(&self.failed, scope)
                .filter(|d| !want.contains(*d))
                .cloned()
                .collect();
            for d in gone_failures {
                self.failed.remove(&d);
            }
        }
        let mut first_err = None;
        for d in want {
            if self.dirs.contains(&d) {
                continue;
            }
            match self.watcher.watch(&d, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.failed.remove(&d);
                    r.added += 1;
                    r.new_dirs.push(d.clone());
                    self.dirs.insert(d);
                }
                // Vanished since the walk: its own remove/rename event
                // reconciles it. Not a failure.
                Err(_) if is_gone(&d) => {}
                Err(e) => {
                    r.failed += 1;
                    first_err.get_or_insert_with(|| format!("{}: {e}", d.display()));
                    self.failed.insert(d);
                }
            }
        }
        if let Some(e) = first_err {
            anstream::eprintln!(
                "error: could not watch {} director(ies) — changes there are NOT seen \
                 until the next `vec updatedb` (raise fs.inotify.max_user_watches?). \
                 First: {e}",
                r.failed
            );
        }
        r
    }
}

/// Members of `set` equal to or under `scope` (a contiguous range).
fn under<'a>(set: &'a BTreeSet<PathBuf>, scope: &'a Path) -> impl Iterator<Item = &'a PathBuf> {
    set.range(scope.to_path_buf()..)
        .take_while(move |d| d.starts_with(scope))
}

/// `paths` without the ones nested inside another.
fn outermost(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut sorted: Vec<PathBuf> = paths.to_vec();
    sorted.sort();
    let mut kept: Vec<PathBuf> = Vec::with_capacity(sorted.len());
    for p in sorted {
        if !kept.last().is_some_and(|k| p.starts_with(k)) {
            kept.push(p);
        }
    }
    kept
}

// ---------------------------------------------------------------------------
// Flushing (main thread)
// ---------------------------------------------------------------------------

/// A set of changed paths ready to be indexed.
struct Batch {
    paths: Vec<PathBuf>,
    /// Re-index all include paths instead of `paths`.
    rescan: bool,
    events: u64,
    age: Duration,
}

/// Block until the pending set is due, then take it.
fn wait_for_batch(shared: &(Mutex<Pending>, Condvar), wcfg: &WatchConfig) -> Batch {
    let (l, cvar) = shared;
    let mut pending = lock(l);
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

/// Re-index one batch: one store, one embedder, one scoped update.
fn index_batch(cfg: &Config, paths: &[PathBuf]) -> Result<()> {
    let mut store =
        Store::open(&cfg.database.db_path, cfg.database.wal).context("opening store")?;
    let embedder = vec_core::load_embedder(cfg);
    let stats = update_paths(&mut store, &embedder, cfg, paths, |msg| {
        anstream::eprintln!("{msg}")
    })?;
    anstream::eprintln!(
        "  visited={} updated={} deleted={} chunks_added={} errors={}",
        stats.files_visited,
        stats.files_updated,
        stats.files_deleted,
        stats.chunks_added,
        stats.errors,
    );
    Ok(())
}

/// Record the watcher's state in the DB `meta` table for `vec status`.
fn write_status(
    cfg: &Config,
    paths: &[PathBuf],
    err: Option<&anyhow::Error>,
    watched: usize,
    failed: usize,
) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let result = (|| -> Result<()> {
        let mut store = Store::open(&cfg.database.db_path, cfg.database.wal)?;
        store.set_meta("watch.last_flush", &now.to_string())?;
        store.set_meta("watch.last_paths", &paths.len().to_string())?;
        store.set_meta(
            "watch.last_error",
            &err.map(|e| format!("{e:#}")).unwrap_or_default(),
        )?;
        store.set_meta("watch.watched_dirs", &watched.to_string())?;
        store.set_meta("watch.watch_failures", &failed.to_string())?;
        Ok(())
    })();
    if let Err(e) = result {
        anstream::eprintln!("warn: could not record watcher status: {e:#}");
    }
}

// ---------------------------------------------------------------------------
// Pending — the bounded, de-duplicated set of changed paths
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Pending {
    /// Collapsing never goes above these (`[index] include_paths`): a path
    /// above them would be ignored by the indexer, losing the change.
    roots: Vec<PathBuf>,
    paths: HashSet<PathBuf>,
    /// Too many distinct directories to track, or events were lost: the next
    /// flush re-indexes the include paths, and inserts are O(1) until then.
    overflow: bool,
    first_seen: Option<Instant>,
    last_event: Option<Instant>,
    /// After a failed flush: do not flush again before this instant.
    not_before: Option<Instant>,
    events: u64,
}

impl Pending {
    fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            ..Default::default()
        }
    }

    fn touch(&mut self, now: Instant) {
        self.first_seen.get_or_insert(now);
        self.last_event = Some(now);
        self.events += 1;
    }

    fn insert(&mut self, path: PathBuf, now: Instant, max_pending: usize) {
        self.touch(now);
        if self.overflow || self.covered(&path) {
            return;
        }
        self.paths.insert(path);
        if self.paths.len() > max_pending {
            self.collapse(max_pending);
        }
    }

    /// Schedule a rescan of the include paths.
    fn force_rescan(&mut self, now: Instant) {
        self.touch(now);
        self.overflow = true;
        self.paths.clear();
    }

    /// Put a failed batch back; the next flush waits until `retry_at`.
    fn requeue(&mut self, batch: Batch, now: Instant, max_pending: usize, retry_at: Instant) {
        self.not_before = Some(retry_at);
        self.events += batch.events.saturating_sub(1);
        if batch.rescan {
            self.force_rescan(now);
        }
        for p in batch.paths {
            self.insert(p, now, max_pending);
        }
    }

    /// `true` when `path` or one of its ancestors is already pending (a
    /// pending directory is walked, which covers everything under it).
    fn covered(&self, path: &Path) -> bool {
        path.ancestors().any(|a| self.paths.contains(a))
    }

    /// Replace paths by their parent directories until at most half of `max`
    /// remain, so at least `max / 2` inserts happen before the next collapse
    /// (amortised O(1) per event). Never collapses above an include path. If
    /// the set cannot shrink that far, switch to overflow. Indexing a parent
    /// re-walks it, and unchanged files are skipped by hash, so this trades
    /// CPU for memory without losing a change.
    fn collapse(&mut self, max: usize) {
        let target = max / 2;
        while self.paths.len() > target {
            let parents: HashSet<PathBuf> = self
                .paths
                .iter()
                .map(|p| match p.parent() {
                    Some(parent) if self.roots.iter().any(|r| parent.starts_with(r)) => {
                        parent.to_path_buf()
                    }
                    _ => p.clone(), // already an include path: cannot go higher
                })
                .collect();
            if parents.len() == self.paths.len() {
                self.paths.clear();
                self.overflow = true;
                return;
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
        let due = quiet.min(cap).max(self.not_before.unwrap_or(now));
        Some(due.saturating_duration_since(now))
    }

    fn take(&mut self, now: Instant) -> Batch {
        let age = self
            .first_seen
            .map_or(Duration::ZERO, |f| now.saturating_duration_since(f));
        let fresh = Pending::new(std::mem::take(&mut self.roots));
        let taken = std::mem::replace(self, fresh);
        Batch {
            paths: taken.paths.into_iter().collect(),
            rescan: taken.overflow,
            events: taken.events,
            age,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, Flag};

    fn wcfg() -> WatchConfig {
        WatchConfig {
            debounce_ms: 3_000,
            max_wait_ms: 30_000,
            max_pending: 4,
            startup_rescan: true,
            retry_max_ms: 60_000,
        }
    }

    fn root_pending() -> Pending {
        Pending::new(vec!["/".into()])
    }

    #[test]
    fn debounce_fires_after_quiet_window() {
        let w = wcfg();
        let t0 = Instant::now();
        let mut p = root_pending();
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
        let mut p = root_pending();
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
        let mut p = root_pending();
        for _ in 0..100_000 {
            p.insert("/a/same.rs".into(), t0, 4);
        }
        assert_eq!(p.paths.len(), 1);
        assert_eq!(p.events, 100_000);
    }

    #[test]
    fn pending_cap_collapses_without_losing_changes() {
        let t0 = Instant::now();
        let mut p = root_pending();
        for d in ["a", "b"] {
            for f in 0..10 {
                p.insert(format!("/r/{d}/{f}.rs").into(), t0, 4);
            }
        }
        assert!(p.paths.len() <= 4, "cap respected: {:?}", p.paths);
        for d in ["a", "b"] {
            for f in 0..10 {
                let file = PathBuf::from(format!("/r/{d}/{f}.rs"));
                assert!(
                    p.overflow || p.paths.iter().any(|q| file.starts_with(q)),
                    "{} not covered by {:?}",
                    file.display(),
                    p.paths
                );
            }
        }
    }

    #[test]
    fn uncollapsible_set_overflows_to_rescan() {
        let t0 = Instant::now();
        // Each changed path is itself an include path: collapsing would
        // leave the include paths, so the set cannot shrink.
        let roots: Vec<PathBuf> = (0..5).map(|i| format!("/r/d{i}").into()).collect();
        let mut p = Pending::new(roots.clone());
        for r in roots {
            p.insert(r, t0, 4);
        }
        assert!(p.overflow, "a set that cannot collapse must overflow");
        assert!(p.paths.is_empty(), "overflow frees the set");
        for i in 0..100_000 {
            p.insert(format!("/r/d1/e{i}").into(), t0, 4); // O(1) each
        }
        assert!(p.paths.is_empty());
        let batch = p.take(t0);
        assert!(batch.rescan);
        assert_eq!(batch.events, 100_005);
        assert!(!p.overflow, "take() resets overflow");
        assert_eq!(p.roots.len(), 5, "take() keeps the include paths");
    }

    #[test]
    fn collapse_stops_at_include_paths() {
        let t0 = Instant::now();
        let mut p = Pending::new(vec!["/home/u/proj".into()]);
        for i in 0..10 {
            p.insert(format!("/home/u/proj/d{i}/f.rs").into(), t0, 4);
        }
        assert!(
            p.paths.iter().all(|q| q.starts_with("/home/u/proj")),
            "collapsed above the include path: {:?}",
            p.paths
        );
    }

    #[test]
    fn path_under_pending_directory_is_not_stored() {
        let t0 = Instant::now();
        let mut p = root_pending();
        p.insert("/r/a".into(), t0, 100);
        p.insert("/r/a/b/c.rs".into(), t0, 100);
        assert_eq!(p.paths.len(), 1);
    }

    #[test]
    fn failed_batch_is_requeued() {
        let t0 = Instant::now();
        let mut p = root_pending();
        p.insert("/r/a.rs".into(), t0, 100);
        let batch = p.take(t0);
        let retry_at = t0 + Duration::from_secs(20);
        p.requeue(batch, t0, 100, retry_at);
        assert!(p.paths.contains(Path::new("/r/a.rs")));
        // Not before the retry time, even though max_wait (30 s) and the
        // debounce (3 s) would allow an earlier flush; new events don't
        // bring the retry forward either.
        p.insert("/r/b.rs".into(), t0 + Duration::from_secs(10), 100);
        assert_eq!(
            p.time_until_due(t0 + Duration::from_secs(15), &wcfg()),
            Some(Duration::from_secs(5))
        );
        assert!(p.time_until_due(retry_at, &wcfg()).unwrap().is_zero());

        let mut q = root_pending();
        q.force_rescan(t0);
        let b = q.take(t0);
        q.requeue(b, t0, 100, t0);
        assert!(q.overflow, "a failed rescan is retried as a rescan");
    }

    #[test]
    fn retry_backoff_doubles_up_to_the_cap() {
        let w = WatchConfig {
            debounce_ms: 300,
            retry_max_ms: 5_000,
            ..wcfg()
        };
        let ms: Vec<u128> = (1..=7).map(|n| retry_delay(&w, n).as_millis()).collect();
        assert_eq!(ms, vec![300, 600, 1200, 2400, 4800, 5000, 5000]);
        assert_eq!(retry_delay(&w, u32::MAX).as_millis(), 5000, "no overflow");
    }

    #[test]
    fn reconcile_queue_overflow_is_bounded() {
        let mut q = ReconcileQueue::default();
        for i in 0..10 {
            q.insert(format!("/n{i}").into(), false, 4);
        }
        assert!(q.overflow && q.dirs.is_empty() && !q.is_empty());
    }

    #[test]
    fn reconcile_queue_merges_reindex_flag() {
        let mut q = ReconcileQueue::default();
        q.insert("/a".into(), false, 10);
        q.insert("/a".into(), true, 10);
        q.insert("/a".into(), false, 10);
        assert_eq!(q.dirs.get(Path::new("/a")), Some(&true));
    }

    #[test]
    fn outermost_and_under() {
        let o = outermost(&["/a/b".into(), "/a".into(), "/a-b".into(), "/a/c/d".into()]);
        assert_eq!(o, vec![PathBuf::from("/a"), PathBuf::from("/a-b")]);
        let set: BTreeSet<PathBuf> = ["/a", "/a/b", "/a/b/c", "/a-b", "/a.x", "/b"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let got: Vec<_> = under(&set, Path::new("/a")).cloned().collect();
        assert_eq!(
            got,
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/a/b"),
                PathBuf::from("/a/b/c")
            ]
        );
    }

    fn test_cfg(dir: &tempfile::TempDir) -> Config {
        let mut cfg = Config::load(None).unwrap();
        cfg.index.include_paths = vec![dir.path().to_path_buf()];
        cfg.index.exclude_dirs = vec!["target".into()];
        cfg.index.exclude_files = vec![];
        cfg.index.gitignore = true;
        cfg.database.db_path = dir.path().join("db/vec.db");
        cfg
    }

    fn ev(kind: EventKind, path: PathBuf) -> Event {
        Event::new(kind).add_path(path)
    }

    fn create_file(path: PathBuf) -> Event {
        ev(EventKind::Create(CreateKind::File), path)
    }

    #[test]
    fn triage_drops_only_exactly_excluded_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir);
        let filter = IndexFilter::new(&cfg, &|_| {});
        let root = dir.path();

        for excluded in [
            root.join("target/debug/deps/x.o"),           // exclude_dirs
            root.join("sub/target/y.rs"),                 // exclude_dirs, nested
            root.join(".cache/z"),                        // hidden
            root.join("db/vec.db-wal"),                   // database directory
            PathBuf::from("/definitely/not/included.rs"), // outside include_paths
        ] {
            assert_eq!(
                triage(&create_file(excluded.clone()), &filter),
                Triage::default(),
                "{} should be dropped",
                excluded.display()
            );
        }
        // Rules the filter cannot decide exactly from the path are left to
        // the walker: the event must reach it.
        for kept in [
            root.join("src/main.rs"),
            root.join("Cargo.lock"),
            root.join("builds/a.txt"),
        ] {
            let t = triage(&create_file(kept.clone()), &filter);
            assert_eq!(t.reindex, vec![kept]);
            assert!(
                t.reconcile.is_empty(),
                "a file creation needs no watch change"
            );
        }
        // Reads are not changes.
        let access = ev(EventKind::Access(AccessKind::Any), root.join("src/main.rs"));
        assert_eq!(triage(&access, &filter), Triage::default());
    }

    #[test]
    fn triage_routes_ignore_rule_and_directory_changes() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir);
        let filter = IndexFilter::new(&cfg, &|_| {});
        let root = dir.path().to_path_buf();

        for (path, scope) in [
            (root.join("sub/.gitignore"), root.join("sub")),
            (root.join("sub/.ignore"), root.join("sub")),
            (root.join("sub/.git"), root.join("sub")),
            (root.join("sub/.git/info/exclude"), root.join("sub")),
        ] {
            let t = triage(&create_file(path.clone()), &filter);
            assert_eq!(t.reconcile, vec![(scope, true)], "{}", path.display());
            assert!(t.reindex.is_empty());
        }
        // An ignore file inside an excluded tree changes nothing indexable.
        let t = triage(&create_file(root.join("target/.gitignore")), &filter);
        assert_eq!(t, Triage::default());

        let newdir = root.join("newdir");
        let t = triage(
            &ev(EventKind::Create(CreateKind::Folder), newdir.clone()),
            &filter,
        );
        assert_eq!(t.reconcile, vec![(newdir.clone(), false)]);
        assert_eq!(t.reindex, vec![newdir.clone()]);

        // Moved away or deleted (unknown type): may have been a directory.
        let t = triage(
            &ev(
                EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::From)),
                root.join("gone"),
            ),
            &filter,
        );
        assert_eq!(t.reconcile, vec![(root.join("gone"), false)]);
    }

    #[test]
    fn triage_rescans_when_events_were_lost() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_cfg(&dir);
        let filter = IndexFilter::new(&cfg, &|_| {});
        let overflow = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert!(triage(&overflow, &filter).rescan);
    }

    #[test]
    fn watches_follow_the_walker() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        for d in ["src/a", "target/debug", "builds/out", ".git/info", ".cache"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join(".gitignore"), "builds/\n").unwrap();
        let cfg = test_cfg(&dir);
        let watcher = notify::RecommendedWatcher::new(|_| {}, notify::Config::default()).unwrap();
        let mut w = Watches::new(watcher);
        let rel = |w: &Watches| -> Vec<String> {
            w.dirs
                .iter()
                .map(|d| d.strip_prefix(&root).unwrap().display().to_string())
                .collect()
        };

        w.reconcile(&cfg, std::slice::from_ref(&root));
        assert_eq!(rel(&w), ["", ".git/info", "src", "src/a"]);

        // A new directory tree gets watches, and is reported for re-indexing.
        std::fs::create_dir_all(root.join("new/deep")).unwrap();
        let r = w.reconcile(&cfg, &[root.join("new")]);
        assert_eq!(r.added, 2);
        assert_eq!(r.new_dirs.len(), 2);

        // Ignoring it drops its watches; un-ignoring builds/ adds them.
        std::fs::write(root.join(".gitignore"), "new/\n").unwrap();
        w.reconcile(&cfg, std::slice::from_ref(&root));
        assert_eq!(
            rel(&w),
            ["", ".git/info", "builds", "builds/out", "src", "src/a"]
        );

        // Deleting a directory drops its watches.
        std::fs::remove_dir_all(root.join("src")).unwrap();
        let r = w.reconcile(&cfg, &[root.join("src")]);
        assert_eq!(r.removed, 2);
        assert_eq!(rel(&w), ["", ".git/info", "builds", "builds/out"]);
    }
}

// vec-watch — Real-time filesystem watcher for `vec watch`.
//
// Correctness rule: the indexer's walker is the only judge of what belongs in
// the index. Nothing here re-implements its rules in a way that could
// disagree; shortcuts are only taken where they are exact.
//
// Startup: the configuration and the embedder are loaded once (a model that
// cannot be loaded, or that differs from the index's, is an error before
// anything is watched). The pair, with the event filter built from the same
// configuration, is the `Runtime`; SIGHUP replaces it (see below).
//
// Architecture (four threads):
//   - notify's event thread runs `Handler::handle`. It drops events the
//     walker can never index, decided exactly from the path alone
//     (`IndexFilter`: outside include_paths, DB directory, hidden component,
//     exclude_dirs), and records the rest in a de-duplicated, bounded
//     `Pending` set. Directory-shaped events, and changes to .gitignore /
//     .ignore / .git / .git/info/exclude, go to the reconcile queue.
//   - The watch manager thread owns the inotify watches. For a directory it
//     walks it with the real walker (`vec_index::visit_dirs`) and makes the
//     set of non-recursive inotify watches under it equal the set of
//     directories the walker would enter — adding new ones, removing deleted,
//     moved-out and newly ignored ones. Excluded and ignored trees never get a
//     watch. After watching a new directory, or after an ignore-rule change,
//     it queues the directory for re-indexing, so files written before the
//     watch existed (or newly (un)ignored files) are picked up. No lock is
//     held during a walk; the counters `vec status` shows are atomics.
//   - The SIGHUP thread turns the signal into a reload request for the
//     manager, which loads the configuration and the embedder again. On
//     success the new runtime replaces the old one, watches outside the new
//     include paths are dropped, the include paths are re-walked and
//     re-indexed, and include paths that were dropped are re-indexed too
//     (which prunes their records). On failure the old runtime stays.
//   - The main thread flushes `Pending` after [watch] debounce_ms of quiet or
//     at most [watch] max_wait_ms after the first change, and runs
//     `vec_index::update_paths`, which walks and prunes exactly the changed
//     paths. A failed flush is re-queued with exponential backoff, never
//     dropped. Each flush records the watcher's state in the index `meta`
//     table (keys `watch.*`, read by `vec status`).
//
// Lost information is never ignored: an inotify queue overflow, or more
// distinct changes than [watch] max_pending, degrades to a rescan of the
// include paths (watches and index).
//
// Inotify watch limit: /proc/sys/fs/inotify/max_user_watches defaults to
// 8192 on many kernels. The package postinst sets 65536 via
// /etc/sysctl.d/99-vec.conf. Watch failures are reported by `vec status`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{Event, EventKind, RecursiveMode, Watcher};

use vec_core::config::{Config, WatchConfig};
use vec_embed::Embedder;
use vec_index::{IndexFilter, update_paths, visit_dirs};
use vec_store::{Store, StoreOptions, is_gone};

/// Run the real-time watcher. Blocks until killed (SIGTERM/SIGINT); SIGHUP
/// reloads the configuration. `cfg_path` is the `--config` override, as for
/// every other subcommand.
///
/// # Errors
///
/// When the configuration or the embedder cannot be loaded, the index holds
/// vectors of another model, or the watcher cannot be set up.
pub fn run_watch(cfg_path: Option<&Path>) -> Result<()> {
    let cfg_path: Option<PathBuf> = cfg_path.map(Path::to_path_buf);
    let load = move || Runtime::load(cfg_path.as_deref());
    let rt = load()?;
    #[cfg(unix)]
    let hup = reload_signal::install()?;

    let wcfg = &rt.cfg.watch;
    anstream::eprintln!(
        "vec watch: model {}, monitoring {} path(s), debounce {} ms, max wait {} ms, max pending {}",
        rt.embedder.identity(),
        rt.cfg.index.include_paths.len(),
        wcfg.debounce_ms,
        wcfg.max_wait_ms,
        wcfg.max_pending,
    );
    for path in &rt.cfg.index.include_paths {
        if !path.exists() {
            anstream::eprintln!("  warn: path does not exist, skipping: {}", path.display());
        }
    }

    let pending: Shared<Pending> = shared(Pending::new(rt.cfg.index.include_paths.clone()));
    let queue: Shared<ReconcileQueue> = shared(ReconcileQueue::default());
    let stats = Arc::new(WatchStats::default());
    let current: Current = Arc::new(RwLock::new(Arc::new(rt)));

    let handler = Handler {
        current: Arc::clone(&current),
        pending: Arc::clone(&pending),
        queue: Arc::clone(&queue),
    };
    let watcher = notify::RecommendedWatcher::new(
        move |res: notify::Result<Event>| handler.handle(res),
        notify::Config::default(),
    )
    .context("creating filesystem watcher")?;
    let mut watches = Watches::new(watcher);

    // Initial watches, before anything else, so no change can slip between
    // the startup rescan and the first watch.
    let started = Instant::now();
    let rt = snapshot(&current);
    let r = watches.reconcile(&rt.cfg, &rt.cfg.index.include_paths);
    stats.record_watches(&watches);
    anstream::eprintln!(
        "  watching {} directories ({} failed) in {:.1}s",
        r.added,
        r.failed,
        started.elapsed().as_secs_f64()
    );

    // Changes made while the watcher was not running are unknown: rescan.
    if rt.cfg.watch.startup_rescan {
        let (lock_p, cvar) = &*pending;
        lock(lock_p).force_rescan(Instant::now());
        cvar.notify_one();
    }
    drop(rt);

    {
        let current = Arc::clone(&current);
        let pending = Arc::clone(&pending);
        let queue = Arc::clone(&queue);
        let stats = Arc::clone(&stats);
        std::thread::Builder::new()
            .name("vec-watch-manager".into())
            .spawn(move || watch_manager(watches, &current, &queue, &pending, &stats, &load))
            .context("spawning watch manager thread")?;
    }
    #[cfg(unix)]
    {
        let queue = Arc::clone(&queue);
        std::thread::Builder::new()
            .name("vec-watch-sighup".into())
            .spawn(move || reload_signal::forward(hup, &queue))
            .context("spawning SIGHUP thread")?;
    }

    let mut failures: u32 = 0;
    loop {
        let batch = wait_for_batch(&pending, &current);
        let rt = snapshot(&current);
        flush(
            &rt,
            batch,
            &pending,
            &stats,
            &mut failures,
            |store, paths| index_batch(store, &rt, paths),
        );
    }
}

/// Everything a reload replaces: the configuration, the embedder loaded from
/// it, and the event filter built from it.
struct Runtime {
    cfg: Config,
    embedder: Embedder,
    filter: IndexFilter,
}

impl Runtime {
    /// Load the configuration (`--config` override) and build a runtime.
    fn load(cfg_path: Option<&Path>) -> Result<Self> {
        let cfg = Config::load(cfg_path).context("loading config")?;
        Self::new(cfg)
    }

    /// Load the embedder and check it against the index. Any failure is an
    /// error: never index with a model that is missing or differs.
    fn new(cfg: Config) -> Result<Self> {
        let embedder = vec_core::load_embedder(&cfg)?;
        let cache = vec_core::model_sha::ShaCache::for_user()?;
        let identity = vec_core::model_sha::writer_identity(&embedder, cache.as_ref())
            .context("computing the model identity")?;
        open_store(&cfg)?.check_model(&identity)?;
        let filter = IndexFilter::new(&cfg, &|msg| anstream::eprintln!("{msg}"));
        Ok(Self {
            cfg,
            embedder,
            filter,
        })
    }
}

/// The runtime in use, replaced as a whole on reload. Readers take a
/// snapshot (`Arc` clone) and never hold the lock while working.
type Current = Arc<RwLock<Arc<Runtime>>>;

fn snapshot(current: &Current) -> Arc<Runtime> {
    Arc::clone(&current.read().unwrap_or_else(|e| e.into_inner()))
}

fn open_store(cfg: &Config) -> Result<Store> {
    Store::open(
        &cfg.database.db_path,
        &StoreOptions::from_config(&cfg.database, false),
    )
    .with_context(|| format!("opening the index {}", cfg.database.db_path.display()))
}

/// Counters shown by `vec status`, updated without locks.
#[derive(Default)]
struct WatchStats {
    watched_dirs: AtomicUsize,
    watch_failures: AtomicUsize,
    rescans: AtomicU64,
    reloads: AtomicU64,
}

impl WatchStats {
    fn record_watches(&self, w: &Watches) {
        self.watched_dirs.store(w.dirs.len(), Ordering::Relaxed);
        self.watch_failures.store(w.failed.len(), Ordering::Relaxed);
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
    current: Current,
    pending: Shared<Pending>,
    queue: Shared<ReconcileQueue>,
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
        let rt = snapshot(&self.current);
        let t = match res {
            Ok(event) => triage(&event, &rt.filter),
            Err(e) => {
                // An error from the backend may mean lost events.
                anstream::eprintln!("watch error: {e} — scheduling a rescan");
                Triage {
                    rescan: true,
                    ..Default::default()
                }
            }
        };
        let max_pending = rt.cfg.watch.max_pending;
        drop(rt);
        let now = Instant::now();
        if t.rescan || !t.reconcile.is_empty() {
            let (l, cvar) = &*self.queue;
            let mut q = lock(l);
            if t.rescan {
                q.overflow = true;
            }
            for (dir, reindex) in t.reconcile {
                q.insert(dir, reindex, max_pending);
            }
            cvar.notify_one();
        }
        if t.rescan || !t.reindex.is_empty() {
            let (l, cvar) = &*self.pending;
            let mut p = lock(l);
            if t.rescan {
                p.lost_events(now);
            }
            for path in t.reindex {
                p.insert(path, now, max_pending);
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
    /// SIGHUP: reload the configuration first.
    reload: bool,
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
        self.dirs.is_empty() && !self.overflow && !self.reload
    }
}

fn watch_manager(
    mut watches: Watches,
    current: &Current,
    queue: &(Mutex<ReconcileQueue>, Condvar),
    pending: &(Mutex<Pending>, Condvar),
    stats: &WatchStats,
    load: &dyn Fn() -> Result<Runtime>,
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
        let mut rewalk_all = job.overflow;
        if job.reload && apply_reload(load(), current, &mut watches, pending) {
            stats.reloads.fetch_add(1, Ordering::Relaxed);
            rewalk_all = true;
        }
        let rt = snapshot(current);
        let (dirs, reindex): (Vec<PathBuf>, Vec<PathBuf>) = if rewalk_all {
            anstream::eprintln!("re-walking include paths to reconcile watches");
            let roots = rt.cfg.index.include_paths.clone();
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

        let r = watches.reconcile(&rt.cfg, &dirs);
        stats.record_watches(&watches);
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
                p.insert(d, now, rt.cfg.watch.max_pending);
            }
            cp.notify_one();
        }
    }
}

/// Install a freshly loaded runtime. `false` (old runtime kept) when loading
/// failed. On success the caller re-walks and re-indexes the include paths;
/// here the watches outside them are dropped, and include paths that were
/// dropped are queued for re-indexing, which prunes their records.
fn apply_reload(
    loaded: Result<Runtime>,
    current: &Current,
    watches: &mut Watches,
    pending: &(Mutex<Pending>, Condvar),
) -> bool {
    let rt = match loaded {
        Ok(rt) => rt,
        Err(e) => {
            anstream::eprintln!("error: reload failed, keeping the current configuration: {e:#}");
            return false;
        }
    };
    let roots = rt.cfg.index.include_paths.clone();
    let max_pending = rt.cfg.watch.max_pending;
    let old = std::mem::replace(
        &mut *current.write().unwrap_or_else(|e| e.into_inner()),
        Arc::new(rt),
    );
    let removed = watches.retain_under(&roots);

    let (lp, cp) = pending;
    let mut p = lock(lp);
    let now = Instant::now();
    p.set_roots(roots.clone());
    for dropped in old
        .cfg
        .index
        .include_paths
        .iter()
        .filter(|o| !roots.iter().any(|r| o.starts_with(r)))
    {
        p.insert(dropped.clone(), now, max_pending);
    }
    // Wake the flusher: [watch] timings may have changed.
    cp.notify_one();
    anstream::eprintln!(
        "configuration reloaded: {} include path(s), {removed} watch(es) outside them dropped",
        roots.len()
    );
    true
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

    /// Drop the watches (and failures) not under any of `roots`. Returns
    /// how many watches were removed.
    fn retain_under(&mut self, roots: &[PathBuf]) -> usize {
        let inside = |d: &PathBuf| roots.iter().any(|r| d.starts_with(r));
        let outside: Vec<PathBuf> = self.dirs.iter().filter(|d| !inside(d)).cloned().collect();
        for d in &outside {
            let _ = self.watcher.unwatch(d); // the kernel may have dropped it already
            self.dirs.remove(d);
        }
        self.failed.retain(inside);
        outside.len()
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

/// Block until the pending set is due, then take it. The [watch] timings are
/// read from the current runtime at each wake-up, so a reload applies at once.
fn wait_for_batch(shared: &(Mutex<Pending>, Condvar), current: &Current) -> Batch {
    let (l, cvar) = shared;
    let mut pending = lock(l);
    loop {
        let wcfg = snapshot(current).cfg.watch.clone();
        let now = Instant::now();
        pending = match pending.time_until_due(now, &wcfg) {
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

/// Index one batch with `index` (one store for indexing and status), put it
/// back with a backoff delay if that fails, and record the watcher's state.
fn flush(
    rt: &Runtime,
    batch: Batch,
    pending: &(Mutex<Pending>, Condvar),
    stats: &WatchStats,
    failures: &mut u32,
    index: impl FnOnce(&mut Store, &[PathBuf]) -> Result<()>,
) {
    let paths = if batch.rescan {
        anstream::eprintln!("rescanning include paths");
        stats.rescans.fetch_add(1, Ordering::Relaxed);
        rt.cfg.index.include_paths.clone()
    } else {
        batch.paths.clone()
    };
    anstream::eprintln!(
        "reindexing {} path(s) from {} event(s), oldest change {:.1}s ago",
        paths.len(),
        batch.events,
        batch.age.as_secs_f64(),
    );
    let (store, outcome) = match open_store(&rt.cfg) {
        Ok(mut s) => {
            let outcome = index(&mut s, &paths);
            (Some(s), outcome)
        }
        Err(e) => (None, Err(e)),
    };

    let (lp, cvar) = pending;
    let (pending_paths, overflows) = {
        let mut p = lock(lp);
        if let Err(e) = &outcome {
            // Never drop changes: put them back and retry with exponential
            // backoff (a broken database must not become a busy loop).
            *failures = failures.saturating_add(1);
            let delay = retry_delay(&rt.cfg.watch, *failures);
            anstream::eprintln!(
                "warn: reindex failed (attempt {failures}), retrying in {:.1}s: {e:#}",
                delay.as_secs_f64()
            );
            let now = Instant::now();
            p.requeue(batch, now, rt.cfg.watch.max_pending, now + delay);
            cvar.notify_one();
        } else {
            *failures = 0;
        }
        (p.paths.len(), p.overflows)
    };

    let Some(mut store) = store else {
        anstream::eprintln!("warn: could not record watcher status (index not open)");
        return;
    };
    let status = Status {
        time: unix_now(),
        paths: paths.len(),
        error: outcome.as_ref().err().map(|e| format!("{e:#}")),
        watched_dirs: stats.watched_dirs.load(Ordering::Relaxed),
        watch_failures: stats.watch_failures.load(Ordering::Relaxed),
        pending: pending_paths,
        overflows,
        rescans: stats.rescans.load(Ordering::Relaxed),
        reloads: stats.reloads.load(Ordering::Relaxed),
    };
    if let Err(e) = status.write(&mut store) {
        anstream::eprintln!("warn: could not record watcher status: {e:#}");
    }
}

/// Re-index one batch with the runtime's embedder.
fn index_batch(store: &mut Store, rt: &Runtime, paths: &[PathBuf]) -> Result<()> {
    let stats = update_paths(store, &rt.embedder, &rt.cfg, paths, |msg| {
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

/// Keys of the watcher's state in the index `meta` table (see [`Status`]'s
/// table), read by `vec status`.
pub mod meta_keys {
    /// Unix time of the last flush attempt.
    pub const LAST_FLUSH: &str = "watch.last_flush";
    /// Unix time of the last successful flush.
    pub const LAST_SUCCESS: &str = "watch.last_success";
    /// Error of the last flush, empty on success.
    pub const LAST_ERROR: &str = "watch.last_error";
    /// Paths in the last flush.
    pub const LAST_PATHS: &str = "watch.last_paths";
    /// inotify watches held.
    pub const WATCHED_DIRS: &str = "watch.watched_dirs";
    /// Directories that could not be watched.
    pub const WATCH_FAILURES: &str = "watch.watch_failures";
    /// Changed paths still waiting.
    pub const PENDING: &str = "watch.pending";
    /// Times changes were lost or too many (degraded to a rescan).
    pub const OVERFLOWS: &str = "watch.overflows";
    /// Flushes that re-indexed every include path.
    pub const RESCANS: &str = "watch.rescans";
    /// Successful SIGHUP reloads.
    pub const RELOADS: &str = "watch.reloads";
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The watcher's state after a flush, as recorded in the index `meta` table.
///
/// | key | value |
/// |---|---|
/// | `watch.last_flush` | unix time of the last flush attempt |
/// | `watch.last_success` | unix time of the last successful flush (kept on failure) |
/// | `watch.last_paths` | paths in the last flush |
/// | `watch.last_error` | error of the last flush, empty on success |
/// | `watch.watched_dirs` / `watch.watch_failures` | inotify watches held / failed |
/// | `watch.pending` | changed paths still waiting (e.g. after a failure) |
/// | `watch.overflows` | times changes were lost or too many, degrading to a rescan |
/// | `watch.rescans` | flushes that re-indexed every include path |
/// | `watch.reloads` | successful SIGHUP reloads |
///
/// The counters count since the watcher started.
struct Status {
    time: u64,
    paths: usize,
    error: Option<String>,
    watched_dirs: usize,
    watch_failures: usize,
    pending: usize,
    overflows: u64,
    rescans: u64,
    reloads: u64,
}

impl Status {
    fn write(&self, store: &mut Store) -> Result<()> {
        use meta_keys as k;
        let time = self.time.to_string();
        store.set_meta(k::LAST_FLUSH, &time)?;
        if self.error.is_none() {
            store.set_meta(k::LAST_SUCCESS, &time)?;
        }
        store.set_meta(k::LAST_ERROR, self.error.as_deref().unwrap_or_default())?;
        for (key, value) in [
            (k::LAST_PATHS, self.paths as u64),
            (k::WATCHED_DIRS, self.watched_dirs as u64),
            (k::WATCH_FAILURES, self.watch_failures as u64),
            (k::PENDING, self.pending as u64),
            (k::OVERFLOWS, self.overflows),
            (k::RESCANS, self.rescans),
            (k::RELOADS, self.reloads),
        ] {
            store.set_meta(key, &value.to_string())?;
        }
        Ok(())
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
    /// No member is under another one. Ordered, so a directory's
    /// descendants form one contiguous range.
    paths: BTreeSet<PathBuf>,
    /// Too many distinct directories to track, or events were lost: the next
    /// flush re-indexes the include paths, and inserts are O(1) until then.
    overflow: bool,
    first_seen: Option<Instant>,
    last_event: Option<Instant>,
    /// After a failed flush: do not flush again before this instant.
    not_before: Option<Instant>,
    events: u64,
    /// Times the set switched to a rescan because changes were lost or too
    /// many (kept across `take`).
    overflows: u64,
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
        // Walking `path` covers everything under it.
        let descendants: Vec<PathBuf> = under(&self.paths, &path).cloned().collect();
        for d in descendants {
            self.paths.remove(&d);
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

    /// Events were lost: rescan, and count it.
    fn lost_events(&mut self, now: Instant) {
        self.overflows += 1;
        self.force_rescan(now);
    }

    /// New include paths (after a reload).
    fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = roots;
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
            let parents: Vec<PathBuf> = self
                .paths
                .iter()
                .map(|p| match p.parent() {
                    Some(parent) if self.roots.iter().any(|r| parent.starts_with(r)) => {
                        parent.to_path_buf()
                    }
                    _ => p.clone(), // already an include path: cannot go higher
                })
                .collect();
            let parents = outermost(&parents);
            if parents.len() == self.paths.len() {
                self.paths.clear();
                self.overflow = true;
                self.overflows += 1;
                return;
            }
            self.paths = parents.into_iter().collect();
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
        let fresh = Pending {
            roots: std::mem::take(&mut self.roots),
            overflows: self.overflows,
            ..Default::default()
        };
        let taken = std::mem::replace(self, fresh);
        Batch {
            paths: taken.paths.into_iter().collect(),
            rescan: taken.overflow,
            events: taken.events,
            age,
        }
    }
}

// ---------------------------------------------------------------------------
// SIGHUP
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod reload_signal {
    use std::io::{ErrorKind, PipeReader, Read, Write};
    use std::os::fd::AsRawFd;
    use std::sync::Condvar;
    use std::sync::Mutex;

    use anyhow::{Context, Result};
    use nix::fcntl::{FcntlArg, OFlag, fcntl};

    use super::{ReconcileQueue, lock};

    /// Make SIGHUP write a byte to a pipe and return its read end. The
    /// handler runs in whatever thread receives the signal (the CLI's tokio
    /// threads included), so it only does an async-signal-safe write.
    pub(super) fn install() -> Result<PipeReader> {
        let (reader, writer) = std::io::pipe().context("creating the SIGHUP pipe")?;
        // Never block in the handler: a full pipe already holds a request.
        fcntl(writer.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
            .context("making the SIGHUP pipe non-blocking")?;
        // SAFETY: the action only calls write(2) on a non-blocking pipe: no
        // allocation, no lock, never blocks, so it is async-signal-safe.
        unsafe {
            signal_hook_registry::register(libc::SIGHUP, move || {
                let _ = (&writer).write(&[1]);
            })
        }
        .context("installing the SIGHUP handler")?;
        Ok(reader)
    }

    /// Turn each SIGHUP into a reload request for the watch manager.
    pub(super) fn forward(mut hup: PipeReader, queue: &(Mutex<ReconcileQueue>, Condvar)) {
        // Several signals in a row are one reload.
        let mut buf = [0u8; 64];
        loop {
            match hup.read(&mut buf) {
                Ok(0) => return,
                Ok(_) => {
                    anstream::eprintln!("SIGHUP: reloading the configuration");
                    let (l, cvar) = queue;
                    lock(l).reload = true;
                    cvar.notify_one();
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    anstream::eprintln!("error: SIGHUP listener stopped: {e}");
                    return;
                }
            }
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
        // Hermetic: the compiled-in defaults only, never /etc/vec.conf.
        let mut cfg = Config::defaults().unwrap();
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

    #[test]
    fn pending_directory_prunes_its_descendants() {
        let t0 = Instant::now();
        let mut p = root_pending();
        p.insert("/r/a/b.rs".into(), t0, 100);
        p.insert("/r/a/c/d.rs".into(), t0, 100);
        p.insert("/r/ab.rs".into(), t0, 100);
        p.insert("/r/a".into(), t0, 100);
        let got: Vec<&Path> = p.paths.iter().map(PathBuf::as_path).collect();
        assert_eq!(got, [Path::new("/r/a"), Path::new("/r/ab.rs")]);
    }

    #[test]
    fn overflows_are_counted_and_survive_take() {
        let t0 = Instant::now();
        let mut p = Pending::new(vec!["/r/d0".into(), "/r/d1".into()]);
        p.lost_events(t0);
        assert_eq!(p.overflows, 1);
        p.take(t0);
        for r in ["/r/d0", "/r/d1"] {
            p.insert(r.into(), t0, 1); // cannot collapse above include paths
        }
        assert!(p.overflow);
        assert_eq!(p.overflows, 2);
        p.take(t0);
        assert_eq!(p.overflows, 2, "a counter, not a state");
    }

    /// A runtime with a stub embedder (tests only: never used by vec watch).
    fn stub_runtime(cfg: Config) -> Runtime {
        Runtime {
            filter: IndexFilter::new(&cfg, &|_| {}),
            embedder: Embedder::stub(8),
            cfg,
        }
    }

    fn meta(cfg: &Config, key: &str) -> Option<String> {
        open_store(cfg).unwrap().get_meta(key).unwrap()
    }

    fn one_change(dir: &tempfile::TempDir) -> Shared<Pending> {
        let pending = shared(Pending::new(vec![dir.path().to_path_buf()]));
        lock(&pending.0).insert(dir.path().join("a.rs"), Instant::now(), 100);
        pending
    }

    #[test]
    fn failing_flush_is_requeued_with_backoff_and_reported() {
        let dir = tempfile::TempDir::new().unwrap();
        let rt = stub_runtime(test_cfg(&dir));
        let pending = one_change(&dir);
        let stats = WatchStats::default();
        let mut failures = 0;

        let batch = lock(&pending.0).take(Instant::now());
        let before = Instant::now();
        flush(&rt, batch, &pending, &stats, &mut failures, |_, _| {
            anyhow::bail!("embedding backend unavailable")
        });

        assert_eq!(failures, 1);
        let p = lock(&pending.0);
        assert!(p.paths.contains(&dir.path().join("a.rs")), "change kept");
        let retry = p.not_before.expect("backoff set");
        assert!(retry >= before + Duration::from_millis(rt.cfg.watch.debounce_ms));
        drop(p);
        let err = meta(&rt.cfg, "watch.last_error").unwrap();
        assert!(err.contains("embedding backend unavailable"), "{err}");
        assert_eq!(meta(&rt.cfg, "watch.pending").as_deref(), Some("1"));
        assert_eq!(meta(&rt.cfg, "watch.last_success"), None);

        // The retry succeeds: counters reset, success recorded.
        let batch = lock(&pending.0).take(Instant::now());
        assert_eq!(batch.paths, vec![dir.path().join("a.rs")]);
        flush(&rt, batch, &pending, &stats, &mut failures, |_, _| Ok(()));
        assert_eq!(failures, 0);
        assert_eq!(meta(&rt.cfg, "watch.last_error").as_deref(), Some(""));
        assert!(meta(&rt.cfg, "watch.last_success").is_some());
        assert_eq!(meta(&rt.cfg, "watch.pending").as_deref(), Some("0"));
    }

    #[test]
    fn flush_with_an_unopenable_index_is_requeued() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_cfg(&dir);
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        cfg.database.db_path = blocker.join("vec.db");
        let rt = stub_runtime(cfg);
        let pending = one_change(&dir);
        let mut failures = 0;
        let batch = lock(&pending.0).take(Instant::now());
        flush(
            &rt,
            batch,
            &pending,
            &WatchStats::default(),
            &mut failures,
            |_, _| unreachable!("no store, no indexing"),
        );
        assert_eq!(failures, 1);
        assert_eq!(lock(&pending.0).paths.len(), 1);
    }

    #[test]
    fn rescan_flush_indexes_the_include_paths_and_is_counted() {
        let dir = tempfile::TempDir::new().unwrap();
        let rt = stub_runtime(test_cfg(&dir));
        let pending = shared(Pending::new(rt.cfg.index.include_paths.clone()));
        lock(&pending.0).lost_events(Instant::now());
        let stats = WatchStats::default();
        let batch = lock(&pending.0).take(Instant::now());
        let mut seen = Vec::new();
        flush(&rt, batch, &pending, &stats, &mut 0, |_, paths| {
            seen = paths.to_vec();
            Ok(())
        });
        assert_eq!(seen, rt.cfg.index.include_paths);
        assert_eq!(meta(&rt.cfg, "watch.rescans").as_deref(), Some("1"));
        assert_eq!(meta(&rt.cfg, "watch.overflows").as_deref(), Some("1"));
    }

    #[test]
    fn missing_model_is_an_error_at_startup() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_cfg(&dir);
        cfg.embed.model_search_path = vec![dir.path().join("no-models")];
        let err = Runtime::new(cfg).err().expect("must not start");
        assert!(format!("{err:#}").contains("not found"), "{err:#}");
    }

    #[test]
    fn reload_switches_include_paths_and_keeps_the_old_runtime_on_failure() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        for d in ["a/x", "b/y"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let mut cfg_a = test_cfg(&dir);
        cfg_a.index.include_paths = vec![root.join("a")];
        let mut cfg_b = cfg_a.clone();
        cfg_b.index.include_paths = vec![root.join("b")];

        let current: Current = Arc::new(RwLock::new(Arc::new(stub_runtime(cfg_a.clone()))));
        let watcher = notify::RecommendedWatcher::new(|_| {}, notify::Config::default()).unwrap();
        let mut w = Watches::new(watcher);
        w.reconcile(&cfg_a, &cfg_a.index.include_paths);
        assert_eq!(w.dirs.len(), 2);
        let pending = shared(Pending::new(cfg_a.index.include_paths.clone()));

        // A failed load changes nothing.
        assert!(!apply_reload(
            Err(anyhow::anyhow!("bad config")),
            &current,
            &mut w,
            &pending
        ));
        assert_eq!(snapshot(&current).cfg.index.include_paths, [root.join("a")]);
        assert_eq!(w.dirs.len(), 2);

        assert!(apply_reload(
            Ok(stub_runtime(cfg_b.clone())),
            &current,
            &mut w,
            &pending
        ));
        assert_eq!(snapshot(&current).cfg.index.include_paths, [root.join("b")]);
        assert!(w.dirs.is_empty(), "watches outside the new paths dropped");
        let p = lock(&pending.0);
        assert_eq!(p.roots, [root.join("b")]);
        assert!(
            p.paths.contains(&root.join("a")),
            "the dropped include path is re-indexed, which prunes it"
        );
        drop(p);
        // The manager then re-walks the new include paths.
        w.reconcile(&cfg_b, &cfg_b.index.include_paths);
        assert!(w.dirs.iter().all(|d| d.starts_with(root.join("b"))));
        assert_eq!(w.dirs.len(), 2);
    }

    #[test]
    fn sighup_becomes_a_reload_request() {
        let hup = reload_signal::install().unwrap();
        let queue: Shared<ReconcileQueue> = shared(ReconcileQueue::default());
        {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || reload_signal::forward(hup, &queue));
        }
        // SAFETY: raise() is always safe to call; the handler is installed.
        assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);
        let (l, cvar) = &*queue;
        let q = cvar
            .wait_timeout_while(lock(l), Duration::from_secs(10), |q| !q.reload)
            .unwrap()
            .0;
        assert!(q.reload, "SIGHUP must request a reload");
    }
}

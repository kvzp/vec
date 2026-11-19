//! `vec status` and `vec doctor`.

use std::path::Path;

use anyhow::{Result, bail};
use vec_core::config::Config;
use vec_embed::ModelIdentity;
use vec_query::Searcher;
use vec_query::status::{DaemonStatus, StatusReport, WatcherStatus, daemon_status};

use super::{fmt_size, fmt_unix_ts, load_config};

/// `vec status [--daemon]`. Without `--daemon` the daemon is not contacted
/// (that would start a socket-activated one): only its socket's existence is
/// shown.
pub fn status(cfg_path: Option<&Path>, probe_daemon: bool) -> Result<()> {
    let cfg = load_config(cfg_path)?;
    let store = Searcher::new(cfg.clone()).open_store()?;
    print_status(&vec_query::status::status(&cfg, &store, probe_daemon)?);
    Ok(())
}

fn when(ts: Option<u64>) -> String {
    ts.map_or_else(|| "never".to_owned(), fmt_unix_ts)
}

fn print_status(r: &StatusReport) {
    println!("DB path:       {}", r.db_path.display());
    println!(
        "DB size:       {}",
        r.db_size_bytes
            .map_or_else(|| "unknown".to_owned(), fmt_size)
    );
    println!(
        "Schema:        version {} (this vec: {})",
        r.schema_version, r.supported_schema_version
    );
    println!("Files:         {}", r.files);
    println!("Chunks:        {}", r.chunks);
    println!("Last updatedb: {}", when(r.last_updatedb));
    println!(
        "Index model:   {}",
        r.index_model.as_ref().map_or_else(
            || "none recorded (empty index)".to_owned(),
            ModelIdentity::to_string
        )
    );
    println!(
        "Config model:  {} (backend {})",
        r.configured_model, r.backend
    );
    if r.model_mismatch {
        println!(
            "               WARNING: the index was built with another model; queries fail \
             until 'vec updatedb --full'"
        );
    }
    match (&r.model_path, &r.model_error) {
        (Some(p), _) => println!("Model file:    {}", p.display()),
        (None, Some(e)) => println!("Model file:    {e}"),
        (None, None) => {}
    }
    print_watcher(r.watcher.as_ref());
    print_daemon(&r.daemon);
}

fn print_watcher(w: Option<&WatcherStatus>) {
    let Some(w) = w else {
        println!("Watcher:       never ran on this index");
        return;
    };
    let n = |v: Option<u64>| v.map_or_else(|| "?".to_owned(), |v| v.to_string());
    println!(
        "Watcher:       last flush {}, last success {}",
        when(w.last_flush),
        when(w.last_success)
    );
    println!(
        "               {} dir(s) watched, {} path(s) pending, {} overflow(s), {} rescan(s), \
         {} reload(s)",
        n(w.watched_dirs),
        n(w.pending),
        n(w.overflows),
        n(w.rescans),
        n(w.reloads)
    );
    if w.watch_failures.is_some_and(|f| f > 0) {
        println!(
            "               WARNING: {} director(ies) could not be watched — changes there \
             are only seen by 'vec updatedb' (raise fs.inotify.max_user_watches)",
            n(w.watch_failures)
        );
    }
    if let Some(e) = &w.last_error {
        println!("               WARNING: last flush failed (retrying): {e}");
    }
}

fn print_daemon(d: &DaemonStatus) {
    match d {
        DaemonStatus::NotProbed { socket_exists } => println!(
            "Daemon:        not asked; socket {} (vec status --daemon asks, which starts a \
             socket-activated daemon)",
            if *socket_exists { "exists" } else { "missing" }
        ),
        DaemonStatus::NotRunning => {
            println!("Daemon:        not running (queries load the model in-process)");
        }
        DaemonStatus::Running { identity, usable } => {
            println!("Daemon:        running, {identity}");
            if !usable {
                println!(
                    "               WARNING: not the index's model, queries embed in-process; \
                     restart the daemon (or rebuild the index)"
                );
            }
        }
        DaemonStatus::Error { message } => println!("Daemon:        error: {message}"),
        DaemonStatus::Unsupported => println!("Daemon:        not supported on this platform"),
    }
}

/// One finding of `vec doctor`.
struct Report {
    problems: usize,
}

impl Report {
    fn ok(&self, what: &str, detail: impl std::fmt::Display) {
        println!("[ ok ] {what}: {detail}");
    }
    fn warn(&self, what: &str, detail: impl std::fmt::Display) {
        println!("[warn] {what}: {detail}");
    }
    fn fail(&mut self, what: &str, detail: impl std::fmt::Display) {
        println!("[FAIL] {what}: {detail}");
        self.problems += 1;
    }
}

/// `vec doctor`: configuration, model, index, daemon. An error exit when any
/// check fails.
pub fn doctor(cfg_path: Option<&Path>) -> Result<()> {
    let mut report = Report { problems: 0 };
    let cfg = match Config::load_unvalidated(cfg_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            report.fail("config", format!("{e:#}"));
            bail!("vec doctor: the configuration cannot be loaded");
        }
    };
    let layers: Vec<String> = cfg.layers().iter().map(ToString::to_string).collect();
    match cfg.validate() {
        Ok(()) => report.ok("config", layers.join(", ")),
        Err(e) => report.fail("config", format!("{e:#}")),
    }

    let model = check_model(&cfg, &mut report);
    let index_model = check_index(&cfg, model.as_ref(), &mut report);
    println!(
        "[info] daemon: asking the daemon at {} (with systemd socket activation, this starts it)",
        cfg.daemon.socket.display()
    );
    match daemon_status(&cfg, index_model.as_ref()) {
        // Never returned when asking.
        DaemonStatus::NotProbed { .. } => {}
        DaemonStatus::NotRunning => report.ok(
            "daemon",
            format!(
                "not running at {} (queries embed in-process)",
                cfg.daemon.socket.display()
            ),
        ),
        DaemonStatus::Running {
            identity,
            usable: true,
        } => report.ok("daemon", identity),
        DaemonStatus::Running {
            identity,
            usable: false,
        } if index_model.is_none() => {
            report.warn("daemon", format!("{identity}; the index has no model yet"));
        }
        DaemonStatus::Running { identity, .. } => report.fail(
            "daemon",
            format!(
                "serves {identity}, not the index's model (queries embed in-process): restart it"
            ),
        ),
        DaemonStatus::Error { message } => report.fail("daemon", message),
        DaemonStatus::Unsupported => report.ok("daemon", "not supported on this platform"),
    }

    if report.problems > 0 {
        bail!("vec doctor: {} problem(s) found", report.problems);
    }
    println!("No problem found.");
    Ok(())
}

/// Resolve and load the configured model; its identity (with sha256).
fn check_model(cfg: &Config, report: &mut Report) -> Option<ModelIdentity> {
    if cfg.embed.backend == vec_core::config::EmbedBackend::Onnx {
        match cfg.resolve_model_path() {
            Ok(p) => report.ok("model file", p.display()),
            Err(e) => {
                report.fail("model file", format!("{e:#}"));
                return None;
            }
        }
    }
    let cache = vec_core::model_sha::ShaCache::for_user().ok().flatten();
    let loaded = vec_core::load_embedder(cfg)
        .and_then(|e| vec_core::model_sha::writer_identity(&e, cache.as_ref()));
    match loaded {
        Ok(id) => {
            report.ok("model", format!("loads: {id}"));
            Some(id)
        }
        Err(e) => {
            report.fail("model", format!("{e:#}"));
            None
        }
    }
}

/// Open the index read-only and compare its model with `model`; the index's
/// model.
fn check_index(
    cfg: &Config,
    model: Option<&ModelIdentity>,
    report: &mut Report,
) -> Option<ModelIdentity> {
    let store = match Searcher::new(cfg.clone()).open_store() {
        Ok(s) => s,
        Err(e) => {
            report.fail("index", format!("{e:#}"));
            return None;
        }
    };
    match (store.schema_version(), store.stats()) {
        (Ok(v), Ok((files, chunks, _))) => report.ok(
            "index",
            format!(
                "{} (schema {v}): {files} files, {chunks} chunks",
                cfg.database.db_path.display()
            ),
        ),
        (Err(e), _) | (_, Err(e)) => report.fail("index", format!("{e:#}")),
    }
    let stored = match store.model() {
        Ok(m) => m,
        Err(e) => {
            report.fail("index model", format!("{e:#}"));
            return None;
        }
    };
    match (&stored, model) {
        (None, _) => report.warn("index model", "none recorded: run 'vec updatedb'"),
        (Some(s), Some(m)) if s.is_compatible_with(m) => report.ok("index model", s),
        (Some(s), Some(m)) => report.fail(
            "index model",
            format!("the index was built with {s}, the configured model is {m}: run 'vec updatedb --full'"),
        ),
        (Some(s), None) => report.warn("index model", format!("{s} (configured model not loaded)")),
    }
    stored
}

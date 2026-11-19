//! What `vec status` and the MCP `index_status` tool report.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;
use vec_core::config::Config;
use vec_embed::ModelIdentity;
use vec_store::Store;
use vec_watch::meta_keys;

/// The state of the index, the configured model, the watcher and the daemon.
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    /// `[database] db_path`.
    pub db_path: PathBuf,
    /// Size of the database file, when it can be read.
    pub db_size_bytes: Option<u64>,
    /// Schema version of the database.
    pub schema_version: i64,
    /// Schema version this vec reads and writes.
    pub supported_schema_version: i64,
    /// Indexed files.
    pub files: usize,
    /// Indexed chunks (vectors).
    pub chunks: usize,
    /// The model the index was built with (`None` for an empty index).
    pub index_model: Option<ModelIdentity>,
    /// `[embed] backend`.
    pub backend: String,
    /// `[embed] model`.
    pub configured_model: String,
    /// ONNX: the resolved model file.
    pub model_path: Option<PathBuf>,
    /// Why `model_path` is `None`.
    pub model_error: Option<String>,
    /// `true` when the index model's name differs from `[embed] model`:
    /// queries fail until `vec updatedb --full`.
    pub model_mismatch: bool,
    /// End of the last successful `vec updatedb` (Unix seconds).
    pub last_updatedb: Option<u64>,
    /// Embedding runtime (e.g. `ollama 0.40.16`, `tract 0.22.5 (vec 0.1.0)`)
    /// of the first write since the index was created, reset for another
    /// model or fully rebuilt.
    pub runtime_first_write: Option<String>,
    /// Embedding runtime of the most recent write.
    pub runtime_last_write: Option<String>,
    /// The stored embedding canary (`None`: not recorded yet). `vec doctor`
    /// re-embeds it; status only shows it.
    pub canary: Option<CanaryInfo>,
    /// State recorded by `vec watch` (`None` when it never ran on this index).
    pub watcher: Option<WatcherStatus>,
    /// The embedding daemon.
    pub daemon: DaemonStatus,
}

/// The index's embedding canary, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CanaryInfo {
    /// The text whose vector is stored.
    pub text: String,
    /// The runtime that computed the stored vector.
    pub runtime: Option<String>,
}

/// The watcher's state, as recorded at its last flush.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WatcherStatus {
    /// Last flush attempt (Unix seconds).
    pub last_flush: Option<u64>,
    /// Last successful flush (Unix seconds).
    pub last_success: Option<u64>,
    /// Error of the last flush (`None` when it succeeded).
    pub last_error: Option<String>,
    /// Paths re-indexed by the last flush.
    pub last_paths: Option<u64>,
    /// Directories watched.
    pub watched_dirs: Option<u64>,
    /// Directories that could not be watched.
    pub watch_failures: Option<u64>,
    /// Changed paths still waiting.
    pub pending: Option<u64>,
    /// Times changes were lost or too many (degraded to a rescan).
    pub overflows: Option<u64>,
    /// Flushes that re-indexed every include path.
    pub rescans: Option<u64>,
    /// Successful configuration reloads.
    pub reloads: Option<u64>,
}

/// Whether the embedding daemon answers, and with which model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DaemonStatus {
    /// The daemon was not asked: connecting to a systemd socket starts the
    /// daemon (which loads the model), so plain status reports only whether
    /// `[daemon] socket` exists. Ask with `vec status --daemon`.
    NotProbed {
        /// `[daemon] socket` exists (a daemon or a systemd socket may be
        /// listening there; a stale file is possible too).
        socket_exists: bool,
    },
    /// Nobody listens on `[daemon] socket` (queries embed in-process).
    NotRunning,
    /// The daemon answered the handshake.
    Running {
        /// Its model.
        identity: ModelIdentity,
        /// Whether queries may use it (compatible with the index's model).
        usable: bool,
    },
    /// The daemon did not answer properly.
    Error {
        /// What went wrong.
        message: String,
    },
    /// No daemon support on this platform.
    Unsupported,
}

/// Collect the status of `store` under `cfg`.
///
/// With `probe_daemon`, the daemon is asked for its identity (bounded by
/// `[daemon] reply_timeout_ms`). That connection starts a socket-activated
/// daemon, which loads the model; without it, only the existence of
/// `[daemon] socket` is reported ([`DaemonStatus::NotProbed`]).
///
/// # Errors
///
/// A store error.
pub fn status(cfg: &Config, store: &Store, probe_daemon: bool) -> Result<StatusReport> {
    let (files, chunks, _) = store.stats().context("reading index statistics")?;
    let index_model = store.model().context("reading the index's model")?;
    let runtimes = store
        .runtime_history()
        .context("reading the index's runtime history")?;
    let canary = store
        .canary()
        .context("reading the index's embedding canary")?
        .map(|c| CanaryInfo {
            text: c.text,
            runtime: c.runtime,
        });
    let (model_path, model_error) = match cfg.embed.backend {
        vec_core::config::EmbedBackend::Onnx => match cfg.resolve_model_path() {
            Ok(p) => (Some(p), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        },
        vec_core::config::EmbedBackend::Ollama => (
            None,
            Some("no model file: the ollama backend embeds over HTTP".into()),
        ),
    };
    let meta = |key: &str| -> Result<Option<String>> {
        Ok(store.get_meta(key)?.filter(|v| !v.is_empty()))
    };
    let number = |key: &str| -> Result<Option<u64>> { Ok(meta(key)?.and_then(|v| v.parse().ok())) };
    let watcher = match number(meta_keys::LAST_FLUSH)? {
        None => None,
        Some(last_flush) => Some(WatcherStatus {
            last_flush: Some(last_flush),
            last_success: number(meta_keys::LAST_SUCCESS)?,
            last_error: meta(meta_keys::LAST_ERROR)?,
            last_paths: number(meta_keys::LAST_PATHS)?,
            watched_dirs: number(meta_keys::WATCHED_DIRS)?,
            watch_failures: number(meta_keys::WATCH_FAILURES)?,
            pending: number(meta_keys::PENDING)?,
            overflows: number(meta_keys::OVERFLOWS)?,
            rescans: number(meta_keys::RESCANS)?,
            reloads: number(meta_keys::RELOADS)?,
        }),
    };
    Ok(StatusReport {
        db_path: cfg.database.db_path.clone(),
        db_size_bytes: std::fs::metadata(&cfg.database.db_path)
            .ok()
            .map(|m| m.len()),
        schema_version: store.schema_version()?,
        supported_schema_version: vec_store::SCHEMA_VERSION,
        files,
        chunks,
        model_mismatch: index_model
            .as_ref()
            .is_some_and(|m| m.name != cfg.embed.model),
        index_model: index_model.clone(),
        backend: cfg.embed.backend.to_string(),
        configured_model: cfg.embed.model.clone(),
        model_path,
        model_error,
        last_updatedb: number(vec_index::META_LAST_UPDATEDB)?,
        runtime_first_write: runtimes.first_write,
        runtime_last_write: runtimes.last_write,
        canary,
        watcher,
        daemon: if probe_daemon {
            daemon_status(cfg, index_model.as_ref())
        } else {
            DaemonStatus::NotProbed {
                socket_exists: cfg.daemon.socket.exists(),
            }
        },
    })
}

/// Ask the daemon for its identity; `usable` when it matches `index_model`.
/// This connects to `[daemon] socket`: with systemd socket activation it
/// starts the daemon.
#[must_use]
pub fn daemon_status(cfg: &Config, index_model: Option<&ModelIdentity>) -> DaemonStatus {
    #[cfg(unix)]
    {
        match vec_daemon::client::identity(&cfg.daemon) {
            Ok(None) => DaemonStatus::NotRunning,
            Ok(Some(identity)) => DaemonStatus::Running {
                usable: index_model.is_some_and(|m| m.is_compatible_with(&identity)),
                identity,
            },
            Err(e) => DaemonStatus::Error {
                message: format!("{e:#}"),
            },
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (cfg, index_model);
        DaemonStatus::Unsupported
    }
}

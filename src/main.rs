// src/main.rs — CLI entry point for `vec`.
//
// Usage:
//   vec "query"              — semantic search
//   vec updatedb             — rebuild/update the index
//   vec status               — show index stats and config
//   vec serve                — start MCP server on stdio
//   vec model download       — show where to get the model
//   vec init                 — write a default config file

mod config;
mod daemon;
mod embed;
mod index;
mod mcp;
mod store;
mod util;
mod watch;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::embed::Embedder;
use crate::index::run_updatedb;
use crate::store::Store;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "vec", about = "Semantic file search — find files by meaning", version)]
struct Cli {
    /// Search query (the main use case — runs a semantic search)
    query: Option<String>,

    /// Number of results (default from config)
    #[arg(short, long)]
    limit: Option<usize>,

    /// Show snippet inline with each result
    #[arg(long)]
    snippet: bool,

    /// Restrict search to this path prefix
    #[arg(long)]
    path: Option<PathBuf>,

    /// Minimum cosine similarity score (0.0–1.0); results below this are suppressed
    #[arg(long)]
    min_score: Option<f32>,

    /// Path to a config file (overrides /etc/vec.conf; useful for userland installs)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Rebuild/update the index
    Updatedb {
        /// Force full re-index (wipes existing index and rebuilds from scratch)
        #[arg(long)]
        full: bool,
        /// Only index files under this path
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Show index stats and config
    Status,
    /// Start MCP server (for AI assistant integration) on stdio
    Serve,
    /// Download the default embedding model
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Print a starter config to stdout (redirect to install)
    Init {
        /// Generate a userland config (~/.local paths) instead of system-wide (/etc, /usr/share)
        #[arg(long)]
        user: bool,
    },
    /// Watch configured paths and re-index on changes (real-time mode)
    Watch,
    /// Start the embedding daemon — loads the ONNX model once and serves
    /// embed requests over a Unix socket so interactive `vec` queries skip
    /// the expensive model-compilation step.
    Daemon,
}

#[derive(Subcommand)]
enum ModelAction {
    /// Show where to download the default model and where to place it
    Download,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let cfg_path = cli.config.as_deref();

    match cli.command {
        None => {
            // Default: run a search query if one was provided; otherwise print
            // help (clap handles the no-argument case via require_equals or we
            // just print a short message).
            if let Some(query) = cli.query {
                cmd_search(&query, cli.limit, cli.snippet, cli.path.as_deref(), cli.min_score, cfg_path).await
            } else {
                // No query and no subcommand — print usage hint.
                eprintln!("Usage: vec \"<query>\"  (or `vec --help` for all options)");
                std::process::exit(1);
            }
        }
        Some(Command::Updatedb { full, path }) => {
            cmd_updatedb(full, path.as_deref(), cfg_path).await
        }
        Some(Command::Status) => cmd_status(cfg_path).await,
        Some(Command::Serve) => crate::mcp::run_server().await,
        Some(Command::Model { action: ModelAction::Download }) => cmd_model_download(cfg_path),
        Some(Command::Init { user }) => cmd_init(user),
        Some(Command::Watch) => crate::watch::run_watch(),
        Some(Command::Daemon) => cmd_daemon(cfg_path).await,
    }
}

// ---------------------------------------------------------------------------
// vec <query>
// ---------------------------------------------------------------------------

async fn cmd_search(
    query: &str,
    limit: Option<usize>,
    show_snippet: bool,
    path_filter: Option<&std::path::Path>,
    min_score: Option<f32>,
    cfg_path: Option<&std::path::Path>,
) -> Result<()> {
    let cfg = Config::load(cfg_path).context("loading config")?;

    let mut store = Store::open(&cfg.database.db_path, cfg.database.wal)
        .context("opening store")?;

    let limit = limit.unwrap_or(cfg.search.default_limit);
    let min_score = min_score.unwrap_or(0.0);

    // Embed the query. Try the daemon socket first (avoids the expensive
    // ONNX graph compilation step on every interactive query). Fall back to
    // loading the model in-process if the daemon is not running.
    let _ = &mut store; // suppress "unused mut" if model check is skipped
    let embedding = embed_query(&cfg, query).context("embedding query")?;

    let results = store
        .search(&embedding, limit, min_score, path_filter)
        .context("searching index")?;

    if results.is_empty() {
        anstream::eprintln!("No results.");
        return Ok(());
    }

    for result in &results {
        // Security: check read permission before displaying.
        // Silently skip results the current user cannot read.
        if !crate::util::can_read(&result.path) {
            continue;
        }

        if !show_snippet {
            println!("{}:{}", result.path.display(), result.start_line);
        } else {
            println!(
                "{}:{} (score: {:.3})",
                result.path.display(),
                result.start_line,
                result.score
            );
            // Read the file and slice the relevant bytes.
            match std::fs::read(&result.path) {
                Ok(bytes) => {
                    let end = result.byte_end.min(bytes.len());
                    let start = result.byte_offset.min(end);
                    let slice = &bytes[start..end];
                    let text = String::from_utf8_lossy(slice);
                    for line in text.lines() {
                        println!("    {}", line);
                    }
                }
                Err(e) => {
                    anstream::eprintln!("warn: could not read {}: {}", result.path.display(), e);
                }
            }
            println!();
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// vec updatedb
// ---------------------------------------------------------------------------

async fn cmd_updatedb(full: bool, path_filter: Option<&std::path::Path>, cfg_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = Config::load(cfg_path).context("loading config")?;

    let mut store = Store::open(&cfg.database.db_path, cfg.database.wal)
        .context("opening store")?;

    let mut embedder = load_embedder(&cfg);

    if full {
        anstream::eprintln!("Full re-index...");
    }

    let stats = run_updatedb(
        &mut store,
        &mut embedder,
        &cfg,
        full,
        path_filter,
        |msg| anstream::eprintln!("{msg}"),
    )
    .context("running updatedb")?;

    println!(
        "Done. visited={} updated={} unchanged={} deleted={} chunks_added={} errors={}",
        stats.files_visited,
        stats.files_updated,
        stats.files_unchanged,
        stats.files_deleted,
        stats.chunks_added,
        stats.errors,
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// vec status
// ---------------------------------------------------------------------------

async fn cmd_status(cfg_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = Config::load(cfg_path).context("loading config")?;

    let store = Store::open(&cfg.database.db_path, cfg.database.wal)
        .context("opening store")?;

    let (file_count, chunk_count, last_mtime) = store.stats().context("reading stats")?;

    let db_size = std::fs::metadata(&cfg.database.db_path)
        .map(|m| fmt_size(m.len()))
        .unwrap_or_else(|_| "unknown".into());

    println!("DB path:      {}", cfg.database.db_path.display());
    println!("DB size:      {}", db_size);
    println!("Files:        {}", file_count);
    println!("Chunks:       {}", chunk_count);
    if let Some(mtime) = last_mtime {
        // Convert Unix timestamp to a human-readable date.
        let secs = mtime as u64;
        println!("Last indexed: {} (unix {})", fmt_unix_ts(secs), secs);
    } else {
        println!("Last indexed: never");
    }

    println!("Model:        {}", cfg.embed.model);
    match cfg.resolve_model_path() {
        Ok(p) => println!("Model path:   {} (found)", p.display()),
        Err(_) => println!("Model path:   not found — run 'vec model download'"),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// vec model download
// ---------------------------------------------------------------------------

fn cmd_model_download(cfg_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = Config::load(cfg_path).context("loading config")?;

    // Always use the first (system) search path — models are system-installed.
    let dest_dir = cfg.embed.model_search_path[0].clone();
    let model_dir = dest_dir.join(&cfg.embed.model);

    println!("Place model files in: {}", model_dir.display());
    println!();
    println!("  mkdir -p {dir}", dir = model_dir.display());
    println!(
        "  curl -L -o {dir}/model_int8.onnx \\",
        dir = model_dir.display()
    );
    println!("    https://huggingface.co/onnx-community/gte-multilingual-base/resolve/main/onnx/model_int8.onnx");
    println!(
        "  curl -L -o {dir}/tokenizer.json \\",
        dir = model_dir.display()
    );
    println!("    https://huggingface.co/onnx-community/gte-multilingual-base/resolve/main/tokenizer.json");
    println!();
    println!("Then run: vec updatedb");

    Ok(())
}

// ---------------------------------------------------------------------------
// vec init
// ---------------------------------------------------------------------------

fn cmd_init(user: bool) -> Result<()> {
    if user {
        // Warn if a system-wide vec is already installed — userland is redundant in that case.
        let system_bins = ["/usr/bin/vec", "/usr/local/bin/vec"];
        for p in &system_bins {
            if std::path::Path::new(p).exists() {
                eprintln!(
                    "warning: system-wide vec found at {}.\n\
                     A userland install is only needed when no system installation exists.\n\
                     If you still want a separate userland config, proceed — but be aware\n\
                     that PATH order determines which binary runs.",
                    p
                );
                break;
            }
        }

        let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("~"));
        let local_model_path = home.join(".local/share/vec/models");
        let db_path          = home.join(".local/share/vec/vec.db");
        let socket           = home.join(".local/share/vec/embed.sock");

        // If system models exist and are readable, include them first so the
        // user doesn't have to re-download a model that's already on the machine.
        let system_model_path = std::path::Path::new("/usr/share/vec/models");
        let model_search_path = if system_model_path.is_dir() {
            format!(
                "[\"{}\", \"{}\"]",
                system_model_path.display(),
                local_model_path.display()
            )
        } else {
            format!("[\"{}\"]", local_model_path.display())
        };

        print!(
r#"# vec userland config — no root required
# Install: mkdir -p ~/.config/vec && vec init --user > ~/.config/vec/config.toml
# Use:     vec --config ~/.config/vec/config.toml "your query"

[embed]
model = "gte-multilingual-base"
model_search_path = {model_search_path}
# batch_size = 16
# max_tokens = 128
daemon_socket = "{socket}"

[index]
# Scope to your own directories — don't index the whole system
include_paths = ["{home}"]
# chunk_size    = 40
# chunk_overlap = 10
# gitignore = true

[search]
# default_limit = 10
# snippet_lines = 6

[database]
db_path = "{db_path}"
# wal = true
"#,
            home              = home.display(),
            model_search_path = model_search_path,
            db_path           = db_path.display(),
            socket            = socket.display(),
        );
    } else {
        // Print a starter /etc/vec.conf to stdout.
        // Usage: vec init | sudo tee /etc/vec.conf
        print!(r#"# /etc/vec.conf — system-wide vec configuration
# Install: vec init | sudo tee /etc/vec.conf
# All values shown are compiled-in defaults; uncomment and change as needed.

[embed]
# model = "gte-multilingual-base"
# model_search_path = ["/usr/share/vec/models"]
# batch_size = 16
# max_tokens = 128

[index]
# chunk_size    = 40    # lines per chunk
# chunk_overlap = 10    # overlap lines between adjacent chunks
# max_file_size = 10485760  # 10 MB
# min_file_size = 50
# min_chunk_lines = 5
# gitignore = true
# include_paths = ["/"]
# exclude_dirs  = [".git", "node_modules", "target", "dist", "build",
#                  "__pycache__", ".venv", ".cache", ".cargo",
#                  "proc", "sys", "dev", "run",
#                  "ssl", "certs",
#                  "dpkg", "apt", "rpm", "cloud", "alternatives",
#                  "dist-packages", "site-packages",
#                  "apparmor.d", "iproute2", "abi"]
# exclude_files = ["*.lock", "*.min.js", "*.map", "*.pyc",
#                  ".env", ".env.*", "*.key", "*.pem", "*.p12",
#                  "*.pfx", "*.onnx", "*.bin", "*.so", "*.dylib"]

[search]
# default_limit = 10
# snippet_lines = 6

[database]
# db_path = "/var/lib/vec/vec.db"
# wal     = true
"#);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// vec daemon
// ---------------------------------------------------------------------------

async fn cmd_daemon(cfg_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = Config::load(cfg_path).context("loading config")?;

    // Require a real model — stub makes no sense for a persistent daemon.
    let model_path = cfg
        .resolve_model_path()
        .context("resolving model path for daemon")?;

    anstream::eprintln!(
        "Loading model from {} …",
        model_path.display()
    );
    let embedder = Embedder::load(&model_path, cfg.embed.max_tokens)
        .context("loading embedding model")?;

    #[cfg(unix)]
    return crate::daemon::run_daemon(embedder, &cfg.embed.daemon_socket);

    #[cfg(not(unix))]
    {
        let _ = embedder;
        anstream::eprintln!("vec daemon is not supported on this platform.");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Embed a query string.
///
/// Tries the running `vec daemon` Unix socket first — this is nearly instant
/// because the ONNX graph compilation already happened at daemon startup.
/// Falls back to loading the model in-process if the daemon is not running.
fn embed_query(cfg: &Config, text: &str) -> Result<Vec<f32>> {
    // Fast path: daemon is running.
    #[cfg(unix)]
    if let Some(v) = try_daemon_embed(&cfg.embed.daemon_socket, text) {
        return Ok(v);
    }

    // Slow path: compile and run the model in-process.
    let mut embedder = load_embedder(cfg);
    embedder.embed_one(text).context("embedding query")
}

/// Attempt to embed `text` via the running daemon.
///
/// Returns `None` silently if the daemon is not listening (connection refused,
/// socket missing, etc.).  Any I/O error during an established connection is
/// also suppressed and treated as a fallback trigger.
#[cfg(unix)]
fn try_daemon_embed(socket_path: &std::path::Path, text: &str) -> Option<Vec<f32>> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path).ok()?;

    let bytes = text.as_bytes();
    stream.write_all(&(bytes.len() as u32).to_le_bytes()).ok()?;
    stream.write_all(bytes).ok()?;

    let mut status_buf = [0u8; 4];
    stream.read_exact(&mut status_buf).ok()?;
    let status = u32::from_le_bytes(status_buf);

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).ok()?;
    let data_len = u32::from_le_bytes(len_buf) as usize;

    let mut data = vec![0u8; data_len];
    stream.read_exact(&mut data).ok()?;

    if status != 0 {
        return None; // Daemon returned an error — fall back to local.
    }

    let floats = crate::store::unpack_f32(&data);
    if floats.is_empty() {
        return None;
    }
    Some(floats)
}

/// Load the embedder from the configured model path.
///
/// If the model cannot be found or loaded, falls back to a deterministic stub
/// embedder (dim=768) and prints a warning to stderr.
pub(crate) fn load_embedder(cfg: &Config) -> Embedder {
    match cfg.resolve_model_path() {
        Ok(model_path) => {
            match Embedder::load(&model_path, cfg.embed.max_tokens) {
                Ok(e) => return e,
                Err(err) => {
                    anstream::eprintln!(
                        "warn: could not load model at {}: {:?}\n\
                         Falling back to stub embedder (not semantically meaningful).",
                        model_path.display(),
                        err,
                    );
                }
            }
        }
        Err(_) => {
            anstream::eprintln!(
                "warn: model '{}' not found in search path.\n\
                 Run `vec model download` for installation instructions.\n\
                 Using stub embedder (not semantically meaningful).",
                cfg.embed.model
            );
        }
    }
    Embedder::stub(768)
}

/// Format a byte count as a human-readable size string.
fn fmt_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Very simple Unix timestamp → human readable date (YYYY-MM-DD HH:MM).
///
/// This avoids pulling in a full `chrono` or `time` dependency just for
/// `vec status` output.  The algorithm is a straightforward proleptic
/// Gregorian decomposition.
fn fmt_unix_ts(secs: u64) -> String {
    // Days since Unix epoch.
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hh = time_of_day / 3600;
    let mm = (time_of_day % 3600) / 60;

    // Gregorian calendar decomposition.
    // Algorithm adapted from the public-domain "days_to_ymd" approach.
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02} {:02}:{:02}", y, m, d, hh, mm)
}

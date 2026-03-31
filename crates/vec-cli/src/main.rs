// src/main.rs — CLI entry point for `vec`.
//
// Usage:
//   vec "query"              — semantic search
//   vec updatedb             — rebuild/update the index
//   vec status               — show index stats and config
//   vec serve                — start MCP server on stdio
//   vec model download       — show where to get the model
//   vec init                 — write a default config file

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};

use vec_core::config::Config;
use vec_embed::Embedder;
use vec_index::run_updatedb;
use vec_store::Store;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "vec",
    about = "Semantic file search — find files by meaning",
    version
)]
struct Cli {
    /// Search query (the main use case — runs a semantic search).
    /// Multiple queries are supported: results are merged and deduplicated.
    query: Vec<String>,

    /// Number of results (default from config)
    #[arg(short, long)]
    limit: Option<usize>,

    /// Show snippet inline with each result (±3 lines around best match)
    #[arg(long)]
    snippet: bool,

    /// Output results as JSON
    #[arg(long)]
    json: bool,

    /// Restrict search to this path prefix
    #[arg(long)]
    path: Option<PathBuf>,

    /// Exclude results under these directories
    #[arg(long)]
    exclude: Vec<PathBuf>,

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
    /// Show source lines around a file:line location
    Context {
        /// file:line (e.g. src/main.rs:42)
        file_line: String,
        /// Lines of context above and below (default: 10)
        #[arg(long, default_value = "10")]
        window: usize,
    },
    /// Find code similar to a given file:line (uses stored embedding, no model needed)
    Similar {
        /// file:line (e.g. src/main.rs:42)
        file_line: String,
        /// Number of results
        #[arg(short, long)]
        limit: Option<usize>,
    },
    /// Interactive search — loads the model once and accepts queries in a loop
    Repl,
    /// Garbage collect: remove orphaned entries and compact the database
    Gc,
    /// Show which chunks cover a given file:line and their stats
    Explain {
        /// file:line (e.g. src/main.rs:42)
        file_line: String,
    },
    /// Show files changed since last index (new, modified, deleted)
    Diff,
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },
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
            if !cli.query.is_empty() {
                cmd_search(
                    &cli.query,
                    cli.limit,
                    cli.snippet,
                    cli.json,
                    cli.path.as_deref(),
                    &cli.exclude,
                    cli.min_score,
                    cfg_path,
                )
                .await
            } else {
                eprintln!("Usage: vec \"<query>\"  (or `vec --help` for all options)");
                std::process::exit(1);
            }
        }
        Some(Command::Updatedb { full, path }) => {
            cmd_updatedb(full, path.as_deref(), cfg_path).await
        }
        Some(Command::Status) => cmd_status(cfg_path).await,
        Some(Command::Serve) => vec_mcp::run_server().await,
        Some(Command::Model {
            action: ModelAction::Download,
        }) => cmd_model_download(cfg_path),
        Some(Command::Init { user }) => cmd_init(user),
        Some(Command::Watch) => vec_watch::run_watch(),
        Some(Command::Daemon) => cmd_daemon(cfg_path).await,
        Some(Command::Context { file_line, window }) => cmd_context(&file_line, window),
        Some(Command::Similar { file_line, limit }) => cmd_similar(&file_line, limit, cfg_path),
        Some(Command::Repl) => cmd_repl(cfg_path),
        Some(Command::Gc) => cmd_gc(cfg_path),
        Some(Command::Explain { file_line }) => cmd_explain(&file_line, cfg_path),
        Some(Command::Diff) => cmd_diff(cfg_path),
        Some(Command::Completions { shell }) => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "vec",
                &mut std::io::stdout(),
            );
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// vec <query>
// ---------------------------------------------------------------------------

async fn cmd_search(
    queries: &[String],
    limit: Option<usize>,
    show_snippet: bool,
    json: bool,
    path_filter: Option<&std::path::Path>,
    exclude: &[PathBuf],
    min_score: Option<f32>,
    cfg_path: Option<&std::path::Path>,
) -> Result<()> {
    let cfg = Config::load(cfg_path).context("loading config")?;

    let store =
        Store::open(&cfg.database.db_path, cfg.database.wal).context("opening store")?;

    let limit = limit.unwrap_or(cfg.search.default_limit);
    let min_score = min_score.unwrap_or(0.0);

    // Embed and search each query, merge results (dedup by path+start_line, keep highest score).
    use std::collections::HashMap;
    let mut merged: HashMap<(String, usize), vec_store::SearchResult> = HashMap::new();

    for query in queries {
        let embedding = embed_query(&cfg, query).context("embedding query")?;
        let hits = store
            .search(&embedding, limit, min_score, path_filter)
            .context("searching index")?;
        for hit in hits {
            let key = (hit.path.to_string_lossy().into_owned(), hit.start_line);
            let existing = merged.get(&key);
            if existing.is_none() || existing.unwrap().score < hit.score {
                merged.insert(key, hit);
            }
        }
    }

    // Sort by score descending, take top `limit`.
    let mut results: Vec<_> = merged.into_values().collect();
    // Path-weighted re-ranking: boost results where the file path contains query keywords.
    let path_boost = cfg.search.path_boost;
    if path_boost > 0.0 {
        let all_words: Vec<String> = queries
            .iter()
            .flat_map(|q| q.split_whitespace())
            .map(|w| w.to_lowercase())
            .collect();
        for r in &mut results {
            let path_lower = r.path.to_string_lossy().to_lowercase();
            let matches = all_words.iter().filter(|w| path_lower.contains(w.as_str())).count();
            if matches > 0 {
                r.score += path_boost * matches as f32;
            }
        }
    }

    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit);

    // Apply --exclude filter.
    if !exclude.is_empty() {
        results.retain(|r| !exclude.iter().any(|e| r.path.starts_with(e)));
    }

    // Security: filter unreadable files.
    results.retain(|r| vec_core::util::can_read(&r.path));

    if results.is_empty() {
        anstream::eprintln!("No results.");
        return Ok(());
    }

    // Pre-compute query keywords for best-line matching.
    let query_words: Vec<String> = queries
        .iter()
        .flat_map(|q| q.split_whitespace())
        .map(|w| w.to_lowercase())
        .collect();

    // Cache file contents to avoid re-reading the same file for best_line + snippet.
    let mut file_cache: std::collections::HashMap<PathBuf, String> = std::collections::HashMap::new();

    // Helper: get cached file content.
    let read_cached = |cache: &mut std::collections::HashMap<PathBuf, String>, path: &std::path::Path| -> Option<String> {
        if let Some(content) = cache.get(path) {
            return Some(content.clone());
        }
        if let Ok(content) = std::fs::read_to_string(path) {
            cache.insert(path.to_path_buf(), content.clone());
            Some(content)
        } else {
            None
        }
    };

    if json {
        let items: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                let best = read_cached(&mut file_cache, &r.path)
                    .and_then(|content| best_line_in_content(&content, r, &query_words))
                    .unwrap_or(r.start_line);
                serde_json::json!({
                    "path": r.path.to_string_lossy(),
                    "line": best,
                    "score": (r.score * 1000.0).round() / 1000.0,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    for result in &results {
        let content = read_cached(&mut file_cache, &result.path);
        let best = content.as_ref()
            .and_then(|c| best_line_in_content(c, result, &query_words))
            .unwrap_or(result.start_line);

        println!("{}:{} (score: {:.3})", result.path.display(), best, result.score);

        if show_snippet {
            if let Some(ref content) = content {
                let lines: Vec<&str> = content.lines().collect();
                let ctx = cfg.search.snippet_lines;
                let target = best.saturating_sub(1);
                let from = target.saturating_sub(ctx);
                let to = (target + ctx + 1).min(lines.len());
                for (i, line) in lines[from..to].iter().enumerate() {
                    let lineno = from + i + 1;
                    let marker = if lineno == best { ">" } else { " " };
                    println!("{} {:>5}: {}", marker, lineno, line);
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

async fn cmd_updatedb(
    full: bool,
    path_filter: Option<&std::path::Path>,
    cfg_path: Option<&std::path::Path>,
) -> Result<()> {
    let cfg = Config::load(cfg_path).context("loading config")?;

    let mut store =
        Store::open(&cfg.database.db_path, cfg.database.wal).context("opening store")?;

    let embedder = vec_core::load_embedder(&cfg);

    if full {
        anstream::eprintln!("Full re-index...");
    }

    let stats = run_updatedb(&mut store, &embedder, &cfg, full, path_filter, |msg| {
        anstream::eprintln!("{msg}")
    })
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

    let store = Store::open(&cfg.database.db_path, cfg.database.wal).context("opening store")?;

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
        let db_path = home.join(".local/share/vec/vec.db");
        let socket = home.join(".local/share/vec/embed.sock");

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
#
# Automatic indexing (systemd user services):
#   mkdir -p ~/.config/systemd/user
#   cp contrib/user/vec-*.service contrib/user/vec-*.timer ~/.config/systemd/user/
#   systemctl --user daemon-reload
#   systemctl --user enable --now vec-updatedb.timer vec-watch.service
#
# Tip: alias vec='vec --config ~/.config/vec/config.toml' in your shell profile.

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
            home = home.display(),
            model_search_path = model_search_path,
            db_path = db_path.display(),
            socket = socket.display(),
        );
    } else {
        // Print a starter /etc/vec.conf to stdout.
        // Usage: vec init | sudo tee /etc/vec.conf
        print!(
            r#"# /etc/vec.conf — system-wide vec configuration
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
"#
        );
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

    anstream::eprintln!("Loading model from {} …", model_path.display());
    let embedder =
        Embedder::load(&model_path, cfg.embed.max_tokens).context("loading embedding model")?;

    #[cfg(unix)]
    return vec_daemon::run_daemon(embedder, &cfg.embed.daemon_socket);

    #[cfg(not(unix))]
    {
        let _ = embedder;
        anstream::eprintln!("vec daemon is not supported on this platform.");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// vec context
// ---------------------------------------------------------------------------

fn cmd_context(file_line: &str, window: usize) -> Result<()> {
    let (path, line) = parse_file_line(file_line)?;
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {path}"))?;
    let lines: Vec<&str> = content.lines().collect();
    let target = line.saturating_sub(1); // 1-based → 0-based
    let from = target.saturating_sub(window);
    let to = (target + window + 1).min(lines.len());
    for (i, l) in lines[from..to].iter().enumerate() {
        let lineno = from + i + 1;
        let marker = if lineno == line { ">" } else { " " };
        println!("{} {:>5}: {}", marker, lineno, l);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// vec similar
// ---------------------------------------------------------------------------

fn cmd_similar(file_line: &str, limit: Option<usize>, cfg_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = Config::load(cfg_path).context("loading config")?;
    let store = Store::open(&cfg.database.db_path, cfg.database.wal).context("opening store")?;
    let limit = limit.unwrap_or(cfg.search.default_limit);

    let (path, line) = parse_file_line(file_line)?;
    let embedding = store
        .get_chunk_embedding_at(&std::path::PathBuf::from(&path), line)
        .context("looking up chunk embedding")?
        .ok_or_else(|| anyhow::anyhow!("no chunk covers {}:{}", path, line))?;

    let results = store.search(&embedding, limit + 1, 0.0, None)?;
    // Skip the exact same chunk (score ~1.0 from itself).
    for r in &results {
        if !vec_core::util::can_read(&r.path) {
            continue;
        }
        // Skip self-match (same file and overlapping lines).
        if r.path.to_string_lossy() == path && r.start_line <= line && r.end_line >= line {
            continue;
        }
        println!("{}:{} (score: {:.3})", r.path.display(), r.start_line, r.score);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// vec repl
// ---------------------------------------------------------------------------

fn cmd_repl(cfg_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = Config::load(cfg_path).context("loading config")?;
    let store = Store::open(&cfg.database.db_path, cfg.database.wal).context("opening store")?;
    let embedder = vec_core::load_embedder(&cfg);

    eprintln!("vec repl — type a query, Ctrl-D to exit");
    let mut line = String::new();
    loop {
        eprint!("> ");
        use std::io::Write;
        std::io::stderr().flush().ok();
        line.clear();
        if std::io::stdin().read_line(&mut line).is_err() || line.is_empty() {
            break;
        }
        let query = line.trim();
        if query.is_empty() {
            continue;
        }
        let embedding = embedder.embed_one(query)?;
        let results = store.search(&embedding, cfg.search.default_limit, 0.0, None)?;
        for r in &results {
            if vec_core::util::can_read(&r.path) {
                println!("{}:{} (score: {:.3})", r.path.display(), r.start_line, r.score);
            }
        }
        println!();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// vec gc
// ---------------------------------------------------------------------------

fn cmd_gc(cfg_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = Config::load(cfg_path).context("loading config")?;
    let mut store = Store::open(&cfg.database.db_path, cfg.database.wal).context("opening store")?;

    let size_before = std::fs::metadata(&cfg.database.db_path)
        .map(|m| m.len())
        .unwrap_or(0);

    let deleted = store.delete_missing_files().context("pruning deleted files")?;
    store.vacuum().context("vacuuming database")?;

    let size_after = std::fs::metadata(&cfg.database.db_path)
        .map(|m| m.len())
        .unwrap_or(0);

    println!("Pruned:    {} stale file(s)", deleted);
    println!("DB before: {}", fmt_size(size_before));
    println!("DB after:  {}", fmt_size(size_after));
    Ok(())
}

// ---------------------------------------------------------------------------
// vec explain
// ---------------------------------------------------------------------------

fn cmd_explain(file_line: &str, cfg_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = Config::load(cfg_path).context("loading config")?;
    let store = Store::open(&cfg.database.db_path, cfg.database.wal).context("opening store")?;

    let (path, line) = parse_file_line(file_line)?;
    let chunks = store.get_chunks_covering(&std::path::PathBuf::from(&path), line)?;

    if chunks.is_empty() {
        println!("No chunks cover {}:{}", path, line);
        return Ok(());
    }

    println!("{}:{} is covered by {} chunk(s):", path, line, chunks.len());
    for c in &chunks {
        println!("  lines {}-{}  (bytes {}..{})", c.start_line, c.end_line, c.byte_offset, c.byte_end);
    }

    if let Some(record) = store.get_file(&std::path::PathBuf::from(&path))? {
        println!("  file hash: {}", record.hash);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// vec diff
// ---------------------------------------------------------------------------

fn cmd_diff(cfg_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = Config::load(cfg_path).context("loading config")?;
    let store = Store::open(&cfg.database.db_path, cfg.database.wal).context("opening store")?;

    let diff = vec_index::diff_files(&store, &cfg)?;

    if !diff.new_files.is_empty() {
        println!("New ({}):", diff.new_files.len());
        for p in &diff.new_files {
            println!("  + {}", p.display());
        }
    }
    if !diff.changed_files.is_empty() {
        println!("Changed ({}):", diff.changed_files.len());
        for p in &diff.changed_files {
            println!("  ~ {}", p.display());
        }
    }
    if !diff.deleted_files.is_empty() {
        println!("Deleted ({}):", diff.deleted_files.len());
        for p in &diff.deleted_files {
            println!("  - {}", p.display());
        }
    }
    if diff.new_files.is_empty() && diff.changed_files.is_empty() && diff.deleted_files.is_empty() {
        println!("Index is up to date.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a "file:line" string into (path, line_number).
fn parse_file_line(s: &str) -> Result<(String, usize)> {
    let (path, line_str) = s.rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected file:line format, got: {s}"))?;
    let line: usize = line_str.parse()
        .with_context(|| format!("invalid line number: {line_str}"))?;
    Ok((path.to_string(), line))
}

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
    let embedder = vec_core::load_embedder(cfg);
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

    let floats = vec_store::unpack_f32(&data);
    if floats.is_empty() {
        return None;
    }
    Some(floats)
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

/// Find the line within a chunk that best matches the query keywords.
///
/// Takes the full file content (already cached) and extracts the chunk slice
/// using byte offsets. Scores each line by counting query word matches.
/// Returns the 1-based line number of the best match, or `None` if no line
/// scores above zero.
fn best_line_in_content(
    content: &str,
    result: &vec_store::SearchResult,
    query_words: &[String],
) -> Option<usize> {
    let bytes = content.as_bytes();
    let end = result.byte_end.min(bytes.len());
    let start = result.byte_offset.min(end);
    let chunk = &content[start..end];

    let mut best_score = 0usize;
    let mut best_offset = 0usize;

    for (i, line) in chunk.lines().enumerate() {
        let lower = line.to_lowercase();
        let score: usize = query_words
            .iter()
            .filter(|w| lower.contains(w.as_str()))
            .count();
        if score > best_score {
            best_score = score;
            best_offset = i;
        }
    }

    if best_score > 0 {
        Some(result.start_line + best_offset)
    } else {
        None
    }
}

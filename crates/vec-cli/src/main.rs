//! `vec` — semantic file search, the command-line interface.
//!
//! `vec "query"` (or `vec search "query"`) searches; the subcommands manage
//! the index (`updatedb`, `diff`, `gc`, `watch`), inspect it (`status`,
//! `doctor`, `explain`, `similar`, `context`), run services (`serve` for
//! MCP, `daemon` for embeddings) and help with setup (`init`, `config`,
//! `model download`, `completions`). Each subcommand lives in
//! [`commands`].

mod commands;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, CommandFactory, Parser, Subcommand};

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
    #[command(flatten)]
    search: SearchArgs,

    /// Config file to apply last (instead of the per-user file)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

/// Queries plus result options (`vec QUERY...` and `vec search QUERY...`).
#[derive(Args, Debug, Default)]
struct SearchArgs {
    /// Search queries; the results of several queries are merged
    query: Vec<String>,

    #[command(flatten)]
    flags: SearchFlags,
}

/// How to search and print results (also used by `vec repl`).
#[derive(Args, Debug, Default, Clone)]
struct SearchFlags {
    /// Number of results [default: [search] default_limit]
    #[arg(short, long)]
    limit: Option<usize>,

    /// Show [search] snippet_lines lines before and after each result's best line
    #[arg(long)]
    snippet: bool,

    /// Print JSON
    #[arg(long)]
    json: bool,

    /// Print each matching file once, best first (for xargs)
    #[arg(long, conflicts_with = "snippet")]
    files_only: bool,

    /// Only files equal to or under this path
    #[arg(long)]
    path: Option<PathBuf>,

    /// Drop files equal to or under this path (repeatable)
    #[arg(long)]
    exclude: Vec<PathBuf>,

    /// Minimum cosine similarity, -1.0 to 1.0 [default: [search] min_score]
    #[arg(long)]
    min_score: Option<f32>,
}

#[derive(Subcommand)]
enum Command {
    /// Search (same as `vec QUERY...`; use it for a query that is a subcommand name)
    Search(SearchArgs),
    /// Interactive search: the model is loaded once, then one query per line
    Repl(SearchFlags),
    /// Build or update the index
    Updatedb {
        /// Re-embed every file. With the same model, old entries stay searchable until
        /// replaced; after a model change the index is emptied at the first write
        /// (vectors of two models cannot be mixed)
        #[arg(long)]
        full: bool,
        /// Only index files under this path
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Show index, model and watcher state, and whether the daemon socket exists
    Status {
        /// Also ask the daemon for its model (starts a socket-activated daemon)
        #[arg(long)]
        daemon: bool,
    },
    /// Check the configuration, model, index and daemon (always asks the daemon,
    /// which starts a socket-activated one); non-zero exit on a problem
    Doctor,
    /// Show or check the configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Start the MCP server (AI assistant integration) on stdio
    Serve,
    /// Model files
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Print a starter config generated from the compiled-in defaults
    Init {
        /// Userland config (your home directory, data under ~/.local/share)
        /// instead of the system-wide /etc/vec.conf
        #[arg(long)]
        user: bool,
    },
    /// Watch the include paths and re-index on changes (real-time mode)
    Watch,
    /// Run the embedding daemon: loads the model once and serves embeddings
    /// over [daemon] socket, so interactive queries skip loading it
    Daemon,
    /// Show the lines around file:line
    Context {
        /// file:line (e.g. src/main.rs:42)
        file_line: String,
        /// Lines above and below [default: [search] context_window]
        #[arg(long)]
        window: Option<usize>,
    },
    /// Find code similar to the chunk at file:line (stored vector, no model needed)
    Similar {
        /// file:line (e.g. src/main.rs:42)
        file_line: String,
        /// Number of results [default: [search] default_limit]
        #[arg(short, long)]
        limit: Option<usize>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Show which chunks cover file:line and the file's index entry
    Explain {
        /// file:line (e.g. src/main.rs:42)
        file_line: String,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Garbage collect: remove entries of deleted files and compact the database
    Gc,
    /// Show what `vec updatedb` would change (new, changed, removed files)
    Diff,
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print every effective key, its value and the layer(s) that set it
    Show,
    /// Validate a config file on its own (or the effective configuration);
    /// non-zero exit on error
    Check {
        /// File to check against the compiled-in defaults
        file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ModelAction {
    /// Print the commands that download the configured model
    /// ([embed] model_download_url / model_download_files); downloads nothing
    Download,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    restore_sigpipe();
    let cli = Cli::parse();
    let cfg_path = cli.config.as_deref();
    use commands::*;

    match cli.command {
        None => search::run(&cli.search, cfg_path),
        Some(Command::Search(args)) => search::run(&args, cfg_path),
        Some(Command::Repl(flags)) => search::repl(&flags, cfg_path),
        Some(Command::Similar {
            file_line,
            limit,
            json,
        }) => search::similar(&file_line, limit, json, cfg_path),
        Some(Command::Explain { file_line, json }) => search::explain(&file_line, json, cfg_path),
        Some(Command::Context { file_line, window }) => {
            search::context(&file_line, window, cfg_path)
        }
        Some(Command::Updatedb { full, path }) => index::updatedb(full, path.as_deref(), cfg_path),
        Some(Command::Gc) => index::gc(cfg_path),
        Some(Command::Diff) => index::diff(cfg_path),
        Some(Command::Watch) => vec_watch::run_watch(cfg_path),
        Some(Command::Status { daemon }) => status::status(cfg_path, daemon),
        Some(Command::Doctor) => status::doctor(cfg_path),
        Some(Command::Config {
            action: ConfigAction::Show,
        }) => setup::config_show(cfg_path),
        Some(Command::Config {
            action: ConfigAction::Check { file },
        }) => setup::config_check(file.as_deref(), cfg_path),
        Some(Command::Init { user }) => setup::init(user),
        Some(Command::Model {
            action: ModelAction::Download,
        }) => setup::model_download(cfg_path),
        Some(Command::Serve) => services::serve(cfg_path),
        Some(Command::Daemon) => services::daemon(cfg_path),
        Some(Command::Completions { shell }) => {
            clap_complete::generate(shell, &mut Cli::command(), "vec", &mut std::io::stdout());
            Ok(())
        }
    }
}

/// Let a closed stdout end the process quietly (`vec ... | head`), as for
/// any Unix filter, instead of panicking in `println!` (Rust ignores SIGPIPE
/// by default).
fn restore_sigpipe() {
    #[cfg(unix)]
    {
        use nix::sys::signal::{SigHandler, Signal, signal};
        // SAFETY: called first in main, before any other thread exists; the
        // default disposition installs no Rust handler.
        if let Err(e) = unsafe { signal(Signal::SIGPIPE, SigHandler::SigDfl) } {
            eprintln!("warning: cannot restore the default SIGPIPE handling: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("vec").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_queries_and_the_search_subcommand_parse_alike() {
        let bare = parse(&["--limit", "3", "a", "b"]);
        assert!(bare.command.is_none());
        assert_eq!(bare.search.query, ["a", "b"]);
        assert_eq!(bare.search.flags.limit, Some(3));

        let sub = parse(&["search", "diff", "--files-only", "--exclude", "x"]);
        let Some(Command::Search(args)) = sub.command else {
            panic!("expected search");
        };
        assert_eq!(args.query, ["diff"]);
        assert!(args.flags.files_only);
        assert_eq!(args.flags.exclude, [PathBuf::from("x")]);
    }

    #[test]
    fn files_only_conflicts_with_snippet() {
        assert!(Cli::try_parse_from(["vec", "q", "--files-only", "--snippet"]).is_err());
    }

    #[test]
    fn config_subcommands_parse() {
        assert!(matches!(
            parse(&["config", "check", "/x.toml"]).command,
            Some(Command::Config {
                action: ConfigAction::Check { file: Some(_) }
            })
        ));
        // A global option before the subcommand must not turn it into a query.
        let c = parse(&["--config", "/c.toml", "config", "show"]);
        assert_eq!(c.config, Some(PathBuf::from("/c.toml")));
        assert!(c.search.query.is_empty());
        assert!(matches!(
            c.command,
            Some(Command::Config {
                action: ConfigAction::Show
            })
        ));
        let u = parse(&["--config", "/c.toml", "updatedb", "--full"]);
        assert!(matches!(
            u.command,
            Some(Command::Updatedb { full: true, .. })
        ));
        let ctx = parse(&["--config", "/c.toml", "context", "a:1", "--window", "2"]);
        assert!(matches!(
            ctx.command,
            Some(Command::Context {
                window: Some(2),
                ..
            })
        ));
    }
}

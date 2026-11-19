//! `vec QUERY`, `vec search`, `vec repl`, `vec similar`, `vec explain`,
//! `vec context`: everything that reads the index. Search and ranking are
//! [`vec_query::Searcher`]'s, shared with the MCP server.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Result, bail};
use vec_query::{Annotated, SearchRequest, Searcher};
use vec_store::{SearchResult, Store};

use super::load_config;
use crate::{SearchArgs, SearchFlags};

/// `vec QUERY...` / `vec search QUERY...`.
pub fn run(args: &SearchArgs, cfg_path: Option<&Path>) -> Result<()> {
    if args.query.is_empty() {
        bail!("no query given: vec \"<query>\" (see vec --help)");
    }
    let searcher = Searcher::new(load_config(cfg_path)?);
    let store = searcher.open_store()?;
    search_and_print(&searcher, &store, &args.query, &args.flags)
}

/// `vec repl`: one query per line; the model is loaded once, and an error
/// is reported without ending the session.
pub fn repl(flags: &SearchFlags, cfg_path: Option<&Path>) -> Result<()> {
    let searcher = Searcher::new(load_config(cfg_path)?);
    let store = searcher.open_store()?;
    eprintln!("vec repl — type a query, Ctrl-D to exit");
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        eprint!("> ");
        std::io::stderr().flush()?;
        let Some(line) = lines.next() else {
            break;
        };
        let line = line?;
        let query = line.trim();
        if query.is_empty() {
            continue;
        }
        if let Err(e) = search_and_print(&searcher, &store, &[query.to_owned()], flags) {
            eprintln!("error: {e:#}");
        }
        println!();
    }
    Ok(())
}

fn search_and_print(
    searcher: &Searcher,
    store: &Store,
    queries: &[String],
    flags: &SearchFlags,
) -> Result<()> {
    let req = SearchRequest {
        queries: queries.to_vec(),
        limit: flags.limit,
        min_score: flags.min_score,
        path_filter: flags.path.clone(),
        exclude: flags.exclude.clone(),
    };
    let hits = searcher.search(store, &req)?;
    if hits.is_empty() {
        eprintln!("No results.");
        return Ok(());
    }
    if flags.files_only {
        return print_files(&hits, flags.json);
    }
    let snippet_lines = flags
        .snippet
        .then_some(searcher.config().search.snippet_lines);
    print_results(
        &searcher.annotate(&hits, queries, snippet_lines),
        flags.json,
    )
}

/// Each file once, in rank order.
fn print_files(hits: &[SearchResult], json: bool) -> Result<()> {
    let mut files: Vec<&Path> = Vec::with_capacity(hits.len());
    for h in hits {
        if !files.contains(&h.path.as_path()) {
            files.push(&h.path);
        }
    }
    if json {
        let list: Vec<_> = files.iter().map(|p| p.to_string_lossy()).collect();
        println!("{}", serde_json::to_string_pretty(&list)?);
    } else {
        for f in files {
            println!("{}", f.display());
        }
    }
    Ok(())
}

/// Score rounded for display.
fn rounded(score: f32) -> f64 {
    (f64::from(score) * 1000.0).round() / 1000.0
}

fn print_results(results: &[Annotated], json: bool) -> Result<()> {
    if json {
        let items: Vec<_> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "path": r.path.to_string_lossy(),
                    "line": r.line,
                    "start_line": r.start_line,
                    "end_line": r.end_line,
                    "score": rounded(r.score),
                    "snippet": r.snippet,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }
    for r in results {
        println!("{}:{} (score: {:.3})", r.path.display(), r.line, r.score);
        if let Some(snippet) = &r.snippet {
            println!("{snippet}\n");
        }
    }
    Ok(())
}

/// `vec similar FILE:LINE`.
pub fn similar(
    file_line: &str,
    limit: Option<usize>,
    json: bool,
    cfg_path: Option<&Path>,
) -> Result<()> {
    let searcher = Searcher::new(load_config(cfg_path)?);
    let store = searcher.open_store()?;
    let (path, line) = vec_query::parse_location(file_line)?;
    let hits = searcher.similar(&store, &path, line, limit)?;
    if hits.is_empty() {
        eprintln!("No results.");
        return Ok(());
    }
    print_results(&searcher.annotate(&hits, &[], None), json)
}

/// `vec explain FILE:LINE`.
pub fn explain(file_line: &str, json: bool, cfg_path: Option<&Path>) -> Result<()> {
    let searcher = Searcher::new(load_config(cfg_path)?);
    let store = searcher.open_store()?;
    let (path, line) = vec_query::parse_location(file_line)?;
    let e = searcher.explain(&store, &path, line)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&e)?);
        return Ok(());
    }
    let shown = path.display();
    match &e.indexed_hash {
        None => println!("{shown} is not in the index"),
        Some(hash) => {
            if e.chunks.is_empty() {
                println!("No chunk covers {shown}:{line}");
            } else {
                println!("{shown}:{line} is covered by {} chunk(s):", e.chunks.len());
                for c in &e.chunks {
                    println!(
                        "  lines {}-{}  (bytes {}..{})",
                        c.start_line, c.end_line, c.byte_offset, c.byte_end
                    );
                }
            }
            println!("  file hash: {hash}");
            match e.changed_since_indexed {
                Some(true) => println!("  changed since indexed: yes (run vec updatedb)"),
                Some(false) => println!("  changed since indexed: no"),
                None => println!("  changed since indexed: unknown (cannot stat the file)"),
            }
        }
    }
    Ok(())
}

/// `vec context FILE:LINE`: any file the user can read (not only indexed
/// ones), regular, at most `[index] max_file_size` bytes.
pub fn context(file_line: &str, window: Option<usize>, cfg_path: Option<&Path>) -> Result<()> {
    let cfg = load_config(cfg_path)?;
    let (path, line) = vec_query::parse_location(file_line)?;
    let content = vec_core::util::read_text_capped(&path, cfg.index.max_file_size)?;
    let window = window.unwrap_or(cfg.search.context_window);
    println!("{}", vec_query::context(&content, &path, line, window)?);
    Ok(())
}

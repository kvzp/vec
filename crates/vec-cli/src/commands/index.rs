//! `vec updatedb`, `vec gc`, `vec diff`: building and maintaining the index.

use std::path::Path;

use anyhow::{Context, Result};
use vec_index::run_updatedb;

use super::{fmt_size, load_config, open_store_rw};

/// `vec updatedb [--full] [--path P]`.
pub fn updatedb(full: bool, path_filter: Option<&Path>, cfg_path: Option<&Path>) -> Result<()> {
    let cfg = load_config(cfg_path)?;
    let mut store = open_store_rw(&cfg)?;
    let embedder = vec_core::load_embedder(&cfg)?;
    if full {
        eprintln!("Full re-index...");
    }
    let stats = run_updatedb(&mut store, &embedder, &cfg, full, path_filter, |msg| {
        eprintln!("{msg}");
    })
    .context("running updatedb")?;
    println!(
        "Done. visited={} updated={} unchanged={} deleted={} chunks_added={} chunks_reused={} \
         skipped_non_utf8={} errors={}",
        stats.files_visited,
        stats.files_updated,
        stats.files_unchanged,
        stats.files_deleted,
        stats.chunks_added,
        stats.chunks_reused,
        stats.files_skipped_non_utf8,
        stats.errors,
    );
    Ok(())
}

/// `vec gc`: prune entries of deleted files, then VACUUM.
pub fn gc(cfg_path: Option<&Path>) -> Result<()> {
    let cfg = load_config(cfg_path)?;
    let mut store = open_store_rw(&cfg)?;
    let size = || {
        std::fs::metadata(&cfg.database.db_path)
            .map(|m| m.len())
            .ok()
    };
    let before = size();
    let deleted = store
        .delete_missing_files()
        .context("pruning deleted files")?;
    store.vacuum().context("vacuuming database")?;
    let shown = |s: Option<u64>| s.map_or_else(|| "unknown".to_owned(), fmt_size);
    println!("Pruned:    {deleted} stale file(s)");
    println!("DB before: {}", shown(before));
    println!("DB after:  {}", shown(size()));
    Ok(())
}

/// `vec diff`: what `vec updatedb` would change.
pub fn diff(cfg_path: Option<&Path>) -> Result<()> {
    let cfg = load_config(cfg_path)?;
    let store = vec_query::Searcher::new(cfg.clone()).open_store()?;
    let diff = vec_index::diff_files(&store, &cfg)?;
    for w in &diff.warnings {
        eprintln!("warn: {w}");
    }
    for (title, mark, list) in [
        ("New", '+', &diff.new_files),
        ("Changed", '~', &diff.changed_files),
        ("Removed", '-', &diff.deleted_files),
    ] {
        if !list.is_empty() {
            println!("{title} ({}):", list.len());
            for p in list {
                println!("  {mark} {}", p.display());
            }
        }
    }
    if diff.new_files.is_empty() && diff.changed_files.is_empty() && diff.deleted_files.is_empty() {
        println!("Index is up to date.");
    }
    Ok(())
}

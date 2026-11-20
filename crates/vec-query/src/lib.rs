//! vec-query — the read side of vec, shared by the CLI (`vec`) and the MCP
//! server (`vec serve`), so both search and rank identically.
//!
//! - [`Searcher::open_store`] opens the index read-only.
//! - [`Searcher::embed`] embeds queries with the embedding daemon when one
//!   serves the index's model, else in-process with the configured embedder,
//!   loaded at most once per [`Searcher`] (lazily). A daemon that cannot be
//!   used (busy, timed out, another model, an older protocol) is reported
//!   with a one-line warning on stderr, and the [`Searcher`] embeds
//!   in-process from then on. Either way the model is checked against the one
//!   the index was built with (name and dimension, and the model file's
//!   sha256 when the hash cache knows it: queries never hash the model): a
//!   mismatch is an error that asks for `vec updatedb --full`.
//! - [`Searcher::search`] runs every query, filters (path prefix, excludes,
//!   readability) before the limit, and ranks with [`rank::rank`].
//! - [`Searcher::annotate`] reads each result file once (regular files only,
//!   size-capped) to find the best line and render a snippet
//!   ([`vec_core::snippet`]).
//! - [`status::status`] reports the index, model, watcher and daemon state.

pub mod rank;
pub mod status;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use vec_core::config::Config;
use vec_core::{snippet, util};
use vec_embed::Embedder;
use vec_store::{SearchResult, Store, StoreOptions};

/// One search: queries and optional overrides of the `[search]` settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchRequest {
    /// The queries; their results are merged (at least one, at most
    /// `[search] max_queries`).
    pub queries: Vec<String>,
    /// Results wanted (default `[search] default_limit`).
    pub limit: Option<usize>,
    /// Minimum cosine similarity (default `[search] min_score`).
    pub min_score: Option<f32>,
    /// Only files equal to or under this path (relative: from the current
    /// directory).
    pub path_filter: Option<PathBuf>,
    /// Drop files equal to or under these paths (relative: from the current
    /// directory).
    pub exclude: Vec<PathBuf>,
}

/// A result with its best line and, when asked, a rendered snippet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Annotated {
    /// Absolute path of the file.
    pub path: PathBuf,
    /// The line of the chunk that best matches the query words (1-based).
    pub line: usize,
    /// First line of the chunk (1-based).
    pub start_line: usize,
    /// Last line of the chunk (1-based).
    pub end_line: usize,
    /// Ranking score: cosine similarity plus the path boost, at most 1.0.
    pub score: f32,
    /// Up to `[search] snippet_lines` lines before and after `line` (a radius),
    /// as `> NNNNN: text` rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// One chunk covering a location, for `explain`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoveringChunk {
    /// First line (1-based).
    pub start_line: usize,
    /// Last line (1-based).
    pub end_line: usize,
    /// Byte range in the file when it was indexed.
    pub byte_offset: usize,
    /// End of the byte range (exclusive).
    pub byte_end: usize,
}

/// What the index holds for a location (`vec explain`, MCP `explain`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Explanation {
    /// Absolute path.
    pub path: PathBuf,
    /// The line asked about.
    pub line: usize,
    /// Chunks covering the line (empty when none does).
    pub chunks: Vec<CoveringChunk>,
    /// sha256 of the file when it was indexed (`None`: not indexed).
    pub indexed_hash: Option<String>,
    /// Whether the file's size or mtime changed since it was indexed (`None`:
    /// not indexed, or it cannot be read).
    pub changed_since_indexed: Option<bool>,
}

/// The query engine. Cheap to share (`&self` methods, `Sync`): the MCP
/// server keeps one for its lifetime.
pub struct Searcher {
    cfg: Config,
    embedder: OnceLock<Embedder>,
    /// Serialises the first load of `embedder`.
    loading: Mutex<()>,
    /// Set once the daemon failed: later queries go in-process directly
    /// (no second warning, no second wait for a hung daemon).
    daemon_failed: AtomicBool,
}

impl Searcher {
    /// A searcher for `cfg`; nothing is loaded yet.
    #[must_use]
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            embedder: OnceLock::new(),
            loading: Mutex::new(()),
            daemon_failed: AtomicBool::new(false),
        }
    }

    /// A searcher that embeds in-process with `embedder` instead of loading
    /// the configured one (tests, or a caller that already holds one).
    #[must_use]
    pub fn with_embedder(cfg: Config, embedder: Embedder) -> Self {
        let s = Self::new(cfg);
        // The cell is new: setting it cannot fail.
        let _ = s.embedder.set(embedder);
        s
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Open the index read-only (`[database] db_path`).
    ///
    /// # Errors
    ///
    /// No index, an unreadable one, or one of another schema version.
    pub fn open_store(&self) -> Result<Store> {
        Store::open(
            &self.cfg.database.db_path,
            &StoreOptions::from_config(&self.cfg.database, true),
        )
    }

    /// The in-process embedder, loaded on first use.
    fn embedder(&self) -> Result<&Embedder> {
        if let Some(e) = self.embedder.get() {
            return Ok(e);
        }
        let _guard = self
            .loading
            .lock()
            .map_err(|_| anyhow!("a previous model load panicked"))?;
        if let Some(e) = self.embedder.get() {
            return Ok(e);
        }
        let loaded = vec_core::load_embedder(&self.cfg)?;
        Ok(self.embedder.get_or_init(|| loaded))
    }

    /// Embed `texts` with the model of the index in `store`: the daemon when
    /// it serves that model, else the in-process embedder.
    ///
    /// When the daemon cannot be used (busy, a timeout, another model, a
    /// daemon of an older version), a one-line warning goes to stderr and the
    /// texts are embedded in-process; this searcher then skips the daemon.
    ///
    /// # Errors
    ///
    /// The index was built with another model than `[embed] model` (run
    /// `vec updatedb --full`), or the in-process embedder cannot be loaded,
    /// does not match the index, or fails.
    pub fn embed(&self, store: &Store, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let stored = store.model().context("reading the index's model")?;
        if let Some(stored) = &stored
            && stored.name != self.cfg.embed.model
        {
            bail!(
                "model mismatch: the index at {} was built with {stored}, but [embed] model \
                 is {:?}. Run `vec updatedb --full` to rebuild the index for the configured \
                 model (or configure the old model again).",
                self.cfg.database.db_path.display(),
                self.cfg.embed.model
            );
        }
        #[cfg(unix)]
        if let Some(expected) = &stored
            && let Some(vectors) = self.embed_with_daemon(expected, texts)
        {
            return Ok(vectors);
        }
        let embedder = self.embedder()?;
        let cache = vec_core::model_sha::ShaCache::for_user()?;
        store.check_model(&vec_core::model_sha::reader_identity(
            embedder,
            cache.as_ref(),
        ))?;
        texts
            .iter()
            .map(|t| embedder.embed_one(t).context("embedding the query"))
            .collect()
    }

    /// The daemon's vectors, or `None` to embed in-process: no daemon, or one
    /// that failed (then a one-line warning on stderr, and this searcher does
    /// not try the daemon again).
    #[cfg(unix)]
    fn embed_with_daemon(
        &self,
        expected: &vec_embed::ModelIdentity,
        texts: &[&str],
    ) -> Option<Vec<Vec<f32>>> {
        if self.daemon_failed.load(Ordering::Relaxed) {
            return None;
        }
        match vec_daemon::client::embed(&self.cfg.daemon, expected, texts) {
            Ok(vectors) => vectors,
            Err(e) => {
                if !self.daemon_failed.swap(true, Ordering::Relaxed) {
                    anstream::eprintln!("{}", daemon_warning(&self.cfg.daemon.socket, &e));
                }
                None
            }
        }
    }

    /// Run `req` against `store`: up to `limit` ranked results, each in a
    /// file the caller can read, not excluded, and under the path filter.
    ///
    /// An empty index gives no result without loading any model.
    ///
    /// # Errors
    ///
    /// No query, too many, a blank one, a model mismatch, or a store error.
    pub fn search(&self, store: &Store, req: &SearchRequest) -> Result<Vec<SearchResult>> {
        let s = &self.cfg.search;
        if req.queries.is_empty() {
            bail!("no query given");
        }
        if req.queries.len() > s.max_queries {
            bail!(
                "{} queries given, at most {} allowed ([search] max_queries)",
                req.queries.len(),
                s.max_queries
            );
        }
        if req.queries.iter().any(|q| q.trim().is_empty()) {
            bail!("a query is empty");
        }
        let limit = req.limit.unwrap_or(s.default_limit);
        let min_score = req.min_score.unwrap_or(s.min_score);
        let path_filter = req
            .path_filter
            .as_deref()
            .map(util::absolute_lexical)
            .transpose()?;
        let exclude = req
            .exclude
            .iter()
            .map(|p| util::absolute_lexical(p))
            .collect::<Result<Vec<_>>>()?;
        if limit == 0 || index_is_empty(store)? {
            return Ok(Vec::new());
        }

        let texts: Vec<&str> = req.queries.iter().map(String::as_str).collect();
        let vectors = self.embed(store, &texts)?;
        let keep = |r: &SearchResult| {
            !exclude.iter().any(|e| r.path.starts_with(e)) && util::can_read(&r.path)
        };
        let per_query = vectors
            .iter()
            .map(|v| {
                store
                    .search_filtered(v, limit, min_score, path_filter.as_deref(), keep)
                    .context("searching the index")
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(rank::rank(
            s,
            &self.cfg.index.include_paths,
            &req.queries,
            per_query,
            limit,
        ))
    }

    /// Chunks most similar to the one covering `path:line` (by its stored
    /// vector: no model is needed), excluding that chunk itself, filtered like
    /// [`Searcher::search`] (`min_score`: `[search] min_score`).
    ///
    /// # Errors
    ///
    /// `path` does not exist or the caller cannot read it (its vector would
    /// reveal its content), no chunk covers the location, or a store error.
    pub fn similar(
        &self,
        store: &Store,
        path: &Path,
        line: usize,
        limit: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
        require_readable(path)?;
        let s = &self.cfg.search;
        let vector = store
            .get_chunk_embedding_at(path, line)
            .context("looking up the chunk's vector")?
            .ok_or_else(|| anyhow!("no indexed chunk covers {}:{line}", path.display()))?;
        let is_self =
            |r: &SearchResult| r.path == path && r.start_line <= line && line <= r.end_line;
        store
            .search_filtered(
                &vector,
                limit.unwrap_or(s.default_limit),
                s.min_score,
                None,
                |r| !is_self(r) && util::can_read(&r.path),
            )
            .context("searching the index")
    }

    /// What the index holds for `path:line`.
    ///
    /// # Errors
    ///
    /// `path` does not exist or the caller cannot read it (the index entry
    /// reveals its hash and layout), or a store error.
    pub fn explain(&self, store: &Store, path: &Path, line: usize) -> Result<Explanation> {
        require_readable(path)?;
        let chunks = store
            .get_chunks_covering(path, line)?
            .into_iter()
            .map(|c| CoveringChunk {
                start_line: c.start_line,
                end_line: c.end_line,
                byte_offset: c.byte_offset,
                byte_end: c.byte_end,
            })
            .collect();
        let record = store.get_file(path)?;
        let changed_since_indexed = record.as_ref().and_then(|r| {
            let meta = std::fs::metadata(path).ok()?;
            let mtime = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs_f64();
            Some(r.size != Some(meta.len()) || r.mtime != mtime)
        });
        Ok(Explanation {
            path: path.to_path_buf(),
            line,
            chunks,
            indexed_hash: record.map(|r| r.hash),
            changed_since_indexed,
        })
    }

    /// Read an indexed file for display: it must be in the index, readable by
    /// the caller, a regular file (symlinks are not followed) and at most
    /// `[index] max_file_size` bytes.
    ///
    /// # Errors
    ///
    /// Any of these conditions fails, or a store error.
    pub fn read_indexed(&self, store: &Store, path: &Path) -> Result<String> {
        if store.get_file(path)?.is_none() {
            bail!("{} is not in the index", path.display());
        }
        require_readable(path)?;
        util::read_text_capped(path, self.cfg.index.max_file_size)
    }

    /// The best line of each result for `queries` and, with `snippet_lines`,
    /// a snippet of up to that many lines before and after it (a radius:
    /// `2 * snippet_lines + 1` lines at most). Each file is read once (size
    /// cap `[index] max_file_size`); a file that cannot be read gets the
    /// chunk's first line and no snippet.
    #[must_use]
    pub fn annotate(
        &self,
        results: &[SearchResult],
        queries: &[String],
        snippet_lines: Option<usize>,
    ) -> Vec<Annotated> {
        let terms = self.cfg.search.terms(queries);
        let mut cache: HashMap<&Path, Option<String>> = HashMap::new();
        results
            .iter()
            .map(|r| {
                let content = cache.entry(r.path.as_path()).or_insert_with(|| {
                    util::read_text_capped(&r.path, self.cfg.index.max_file_size).ok()
                });
                let (line, snippet) = match content {
                    Some(text) => {
                        let line = snippet::best_line(
                            text,
                            r.byte_offset..r.byte_end,
                            r.start_line,
                            &terms,
                        );
                        (line, snippet_lines.map(|n| snippet::snippet(text, line, n)))
                    }
                    None => (r.start_line, None),
                };
                Annotated {
                    path: r.path.clone(),
                    line,
                    start_line: r.start_line,
                    end_line: r.end_line,
                    score: r.score,
                    snippet,
                }
            })
            .collect()
    }
}

/// Refuse a file the caller cannot read: what the index holds for it (its
/// vectors, hash and chunk layout) must not be shown to them, as for search
/// results.
fn require_readable(path: &Path) -> Result<()> {
    if util::can_read(path) {
        Ok(())
    } else if vec_store::is_gone(path) {
        bail!("no such file: {}", path.display())
    } else {
        bail!("permission denied: {}", path.display())
    }
}

/// The one-line warning printed when the daemon cannot be used.
fn daemon_warning(socket: &Path, e: &anyhow::Error) -> String {
    let reason = format!("{e:#}").replace('\n', " ");
    format!(
        "vec: warning: the embedding daemon at {} cannot be used ({reason}); embedding \
         in-process",
        socket.display()
    )
}

/// `true` for an index without vectors (no model recorded and no chunk).
fn index_is_empty(store: &Store) -> Result<bool> {
    if store.model()?.is_some() {
        return Ok(false);
    }
    Ok(store.stats()?.1 == 0)
}

/// Parse `file:line` (the last `:` separates the line) and resolve the file
/// to the absolute, lexically normalised form the index stores.
///
/// # Errors
///
/// No `:`, a line that is not a positive number, or no current directory.
pub fn parse_location(s: &str) -> Result<(PathBuf, usize)> {
    let (path, line) = s
        .rsplit_once(':')
        .filter(|(p, _)| !p.is_empty())
        .ok_or_else(|| anyhow!("expected file:line, got {s:?}"))?;
    let line: usize = line
        .parse()
        .ok()
        .filter(|&n| n > 0)
        .ok_or_else(|| anyhow!("invalid line number {line:?} in {s:?} (lines start at 1)"))?;
    Ok((util::absolute_lexical(Path::new(path))?, line))
}

/// `window` lines on each side of `line` of `content` (a file shown by
/// `vec context` and the MCP `context` tool), rendered with
/// [`snippet::render`].
///
/// # Errors
///
/// `line` is 0 or past the end of `content`.
pub fn context(content: &str, path: &Path, line: usize, window: usize) -> Result<String> {
    let total = snippet::line_count(content);
    if line == 0 || line > total {
        bail!("line {line} is outside {} ({total} lines)", path.display());
    }
    Ok(snippet::render(
        &snippet::window(content, line, window),
        line,
    ))
}

#[cfg(test)]
mod tests;

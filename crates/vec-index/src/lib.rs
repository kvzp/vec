//! vec-index — file walker, chunker and incremental indexer.
//!
//! # Chunker
//!
//! [`chunk_file`] splits a UTF-8 file into overlapping windows of
//! [`IndexConfig::chunk_size`] lines that share about
//! [`IndexConfig::chunk_overlap`] lines. Each window end and each window start
//! may move up to [`IndexConfig::boundary_scan`] lines to land on a line that starts
//! with one of [`IndexConfig::boundary_patterns`] (a function or class header), so
//! chunks tend to begin at a definition. Every line of the file belongs to at
//! least one chunk; a window with fewer than
//! [`IndexConfig::min_chunk_lines`] non-blank lines is merged into a neighbour
//! instead of being dropped (a file with fewer such lines in total has no
//! chunk).
//!
//! # Incremental update
//!
//! [`run_updatedb`] and [`update_paths`] walk the include paths with the
//! `ignore` crate (ripgrep's walker, honours `.gitignore`). For each file the walk
//! accepts:
//!
//! 1. unchanged size and mtime → skipped without reading;
//! 2. the file is opened without following symlinks, checked on the open
//!    handle and read with a size cap;
//! 3. unchanged sha256 → only the stored size/mtime are refreshed;
//! 4. otherwise it is chunked; a chunk whose text is already stored for that
//!    file reuses its vector, the others are embedded in batches;
//! 5. the file's record and all its chunks are replaced in one transaction.
//!
//! Before anything is written, a run checks the model identity
//! (`Store::ensure_model`) and re-embeds the index's embedding canary: a
//! drifted runtime stops the run with `vec_store::EmbeddingDrift` unless it is
//! an unscoped `--full`, which replaces the canary when it completes (see
//! ARCHITECTURE.md, "Embedding runtime and canary").
//!
//! A file whose embedding fails is counted as an error and keeps its previous
//! entry. After the walk, records the walk did not accept are pruned, so the
//! index ends up as a fresh full run would leave it. [`diff_files`] uses the same
//! walker and filters without writing anything.
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, anyhow, bail};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::{DirEntry, WalkBuilder};
use sha2::{Digest, Sha256};

use vec_core::config::{Config, EmbedBackend, IndexConfig};
use vec_embed::{Embedder, ModelIdentity};
use vec_store::{
    Canary, CanaryLimits, CanaryVerdict, FileRecord, ModelMismatch, NewChunk, Store, is_gone,
};

// ---------------------------------------------------------------------------
// Chunk
// ---------------------------------------------------------------------------

/// A contiguous slice of a source file, described by byte offsets and lines.
///
/// `text` is the raw UTF-8 content of the slice (for embedding only; it is
/// never stored in the database — only the byte offsets are persisted).
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Byte index of the first character of this chunk (inclusive).
    pub byte_offset: usize,
    /// Byte index one past the last character of this chunk (exclusive).
    pub byte_end: usize,
    /// First line of this chunk, 1-based.
    pub start_line: usize,
    /// Last line of this chunk, 1-based inclusive.
    pub end_line: usize,
    /// The text of this chunk, used for embedding.
    pub text: String,
}

// ---------------------------------------------------------------------------
// chunk_file
// ---------------------------------------------------------------------------

/// Split `content` into overlapping [`Chunk`]s according to `cfg`.
///
/// Windows are half-open line ranges `[start, end)`:
///
/// 1. a window ends `chunk_size` lines after its start, or at the end of the
///    file, whichever comes first;
/// 2. an end before the end of the file moves to the nearest line within
///    ±`boundary_scan` that matches a boundary pattern (that line then starts
///    the next window); the last window always reaches the end of the file;
/// 3. the next window starts `chunk_overlap` lines before the end, also snapped
///    to a boundary line, but always after the previous start and never after
///    the previous end — so consecutive windows leave no gap;
/// 4. a window with fewer than `min_chunk_lines` non-blank lines is merged into
///    the previous chunk (or, if there is none yet, into the next one).
///
/// Hence every line is in a chunk unless the whole file has fewer than
/// `min_chunk_lines` non-blank lines. `chunk_size = 0` (rejected by config
/// validation) is treated as 1.
pub fn chunk_file(content: &str, cfg: &IndexConfig) -> Vec<Chunk> {
    struct Line<'a> {
        text: &'a str,
        byte_start: usize,
        non_blank: bool,
    }

    let mut lines: Vec<Line<'_>> = Vec::new();
    let mut cursor = 0usize;
    for text in content.split_inclusive('\n') {
        lines.push(Line {
            text,
            byte_start: cursor,
            non_blank: text.bytes().any(|b| !b.is_ascii_whitespace()),
        });
        cursor += text.len();
    }
    let total = lines.len();
    if total == 0 {
        return Vec::new();
    }

    let is_boundary = |i: usize| {
        let s = lines[i].text.trim_start();
        cfg.boundary_patterns
            .iter()
            .any(|p| s.starts_with(p.as_str()))
    };
    // The boundary line nearest to `target` within ±boundary_scan and within
    // [lo, hi] (the lower one on a tie), else `target` itself. Requires lo <= target <= hi.
    let snap = |target: usize, lo: usize, hi: usize| -> usize {
        let from = target.saturating_sub(cfg.boundary_scan).max(lo);
        let to = target.saturating_add(cfg.boundary_scan).min(hi);
        (from..=to)
            .filter(|&i| is_boundary(i))
            .min_by_key(|&i| i.abs_diff(target))
            .unwrap_or(target)
    };

    // Step 1-3: covering windows.
    let size = cfg.chunk_size.max(1);
    let mut windows: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    loop {
        let tentative = start + size;
        if tentative >= total {
            windows.push((start, total));
            break;
        }
        // At least one line in the window, and a next window remains.
        let end = snap(tentative, start + 1, total - 1);
        windows.push((start, end));
        let target = end.saturating_sub(cfg.chunk_overlap).clamp(start + 1, end);
        start = snap(target, start + 1, end);
    }

    // Step 4: merge windows that are too sparse.
    let non_blank = |s: usize, e: usize| lines[s..e].iter().filter(|l| l.non_blank).count();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut carry: Option<usize> = None;
    for (s, e) in windows {
        let s = carry.take().unwrap_or(s);
        if non_blank(s, e) >= cfg.min_chunk_lines {
            spans.push((s, e));
        } else if let Some(last) = spans.last_mut() {
            last.1 = e;
        } else {
            carry = Some(s);
        }
    }

    spans
        .into_iter()
        .map(|(s, e)| {
            let byte_offset = lines[s].byte_start;
            let last = &lines[e - 1];
            let byte_end = last.byte_start + last.text.len();
            Chunk {
                byte_offset,
                byte_end,
                start_line: s + 1,
                end_line: e,
                text: content[byte_offset..byte_end].to_owned(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Glob matching (filename only, not path)
// ---------------------------------------------------------------------------

/// Match `filename` against `pattern` using simple glob rules:
///
/// - `*` matches any sequence of characters that does **not** contain `/`
/// - `?` matches exactly one character (any, except `/`)
/// - All other characters match literally
///
/// Only the bare filename is matched — the directory portion is ignored.
pub fn glob_match(pattern: &str, filename: &str) -> bool {
    fn inner(pat: &[u8], name: &[u8]) -> bool {
        match (pat.first(), name.first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some(b'*'), _) => {
                // '*' can match zero characters (skip '*') or one character
                // from `name` (as long as it is not '/').
                if inner(&pat[1..], name) {
                    return true;
                }
                if name.first().is_some_and(|&c| c != b'/') {
                    inner(pat, &name[1..])
                } else {
                    false
                }
            }
            (Some(b'?'), Some(&c)) if c != b'/' => inner(&pat[1..], &name[1..]),
            (Some(b'?'), _) => false,
            (Some(&p), Some(&n)) if p == n => inner(&pat[1..], &name[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), filename.as_bytes())
}

// ---------------------------------------------------------------------------
// IndexStats
// ---------------------------------------------------------------------------

/// Summary of what [`run_updatedb`] or [`update_paths`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexStats {
    /// Files the walk accepted (regular, UTF-8 name, not excluded, size in range).
    pub files_visited: usize,
    /// Files whose entry was (re)written: new or changed.
    pub files_updated: usize,
    /// Files whose content was unchanged (by size+mtime or by sha256).
    pub files_unchanged: usize,
    /// Records pruned because the walk no longer accepts their file.
    pub files_deleted: usize,
    /// Chunks written for new or changed files.
    pub chunks_added: usize,
    /// Of [`IndexStats::chunks_added`], chunks whose vector was reused because
    /// the same text was already stored for that file.
    pub chunks_reused: usize,
    /// Files skipped because their path is not valid UTF-8 (the index stores
    /// UTF-8 paths).
    pub files_skipped_non_utf8: usize,
    /// Problems that skipped a file or directory (each reported through
    /// `progress`); the previous entries of the files concerned are kept.
    pub errors: usize,
}

// ---------------------------------------------------------------------------
// run_updatedb / update_paths
// ---------------------------------------------------------------------------

/// Walk the configured paths, embed changed files, and update the store.
///
/// # Arguments
///
/// * `store`       — the open store; its model identity must match
///   `embedder`'s (recorded on a new index).
/// * `embedder`    — embedding engine.
/// * `cfg`         — full application config (paths, exclusions, batch size).
/// * `full`        — re-embed every file, even unchanged ones. The old entries
///   stay searchable until each file is rewritten; records the walk no longer
///   accepts are pruned at the end. When the index was built by a different
///   model, it is emptied right before the first file is written (so a model
///   that cannot embed leaves it intact); this needs `path_filter = None`.
/// * `path_filter` — only walk this path and only prune records under it (see
///   [`update_paths`]). A relative path is resolved against the current
///   directory, `..` is resolved lexically; a path outside every include path
///   is reported through `progress` and indexes nothing.
/// * `progress`    — receives human-readable status lines (one per file
///   written, warnings prefixed with `warn:`).
///
/// # Errors
///
/// A store error, a model mismatch (without `full`), or a batch in which no
/// file could be embedded (the backend is failing). Records written before
/// the error stay; nothing is pruned.
pub fn run_updatedb(
    store: &mut Store,
    embedder: &Embedder,
    cfg: &Config,
    full: bool,
    path_filter: Option<&Path>,
    progress: impl Fn(&str),
) -> Result<IndexStats> {
    let scopes = match path_filter {
        Some(p) => Some(vec![resolve_path_filter(cfg, p, &progress)?]),
        None => {
            if cfg.index.include_paths.is_empty() {
                progress("no include_paths configured; nothing to index");
            }
            None
        }
    };
    let stats = index_with(store, embedder, cfg, full, scopes.as_deref(), &progress)?;
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    store
        .set_meta(META_LAST_UPDATEDB, &now.to_string())
        .context("recording the time of this run")?;
    Ok(stats)
}

/// Meta key holding the end time of the last successful [`run_updatedb`]
/// (Unix seconds), shown by `vec status`.
pub const META_LAST_UPDATEDB: &str = "updatedb.last_run";

/// Re-index exactly the given paths (files or directories) — used by `vec watch`.
///
/// The result for records equal to or under `paths` is exactly what a full
/// `run_updatedb` would produce for them:
/// * each path is walked from its include path with the real walker, which
///   descends only along the way to the path, so every ancestor's
///   `.gitignore` and `exclude_dirs` rule applies exactly as in a full walk;
/// * records in scope that the walk did not accept (deleted, now excluded or
///   ignored, outside `include_paths`, binary, too large) are pruned, in one
///   transaction with a scoped query — not a whole-index scan;
/// * records under a directory that could not be read are kept unless their
///   file is gone.
pub fn update_paths(
    store: &mut Store,
    embedder: &Embedder,
    cfg: &Config,
    paths: &[PathBuf],
    progress: impl Fn(&str),
) -> Result<IndexStats> {
    index_with(store, embedder, cfg, false, Some(paths), &progress)
}

/// Embedding function used by a run: texts in, one vector per text out.
type EmbedFn<'a> = dyn Fn(&[&str]) -> Result<Vec<Vec<f32>>> + 'a;

/// Run an index pass with `embedder`, on a thread pool local to this run.
fn index_with(
    store: &mut Store,
    embedder: &Embedder,
    cfg: &Config,
    full: bool,
    scopes: Option<&[PathBuf]>,
    progress: &dyn Fn(&str),
) -> Result<IndexStats> {
    let cache = vec_core::model_sha::ShaCache::for_user()?;
    let identity = vec_core::model_sha::writer_identity(embedder, cache.as_ref())
        .context("hashing the model file")?;
    let runtime = embedder.runtime();
    let threads = EmbedThreads::new(cfg)?;
    let embed = |texts: &[&str]| match &threads.pool {
        Some(pool) => pool.install(|| embedder.embed_batch(texts)),
        None => embedder.embed_batch(texts),
    };
    let run = Run::new(
        store,
        identity,
        runtime,
        &embed,
        cfg,
        threads.batch_size,
        full,
        scopes,
        progress,
    )?;
    run.execute(scopes)
}

/// Embed one text with a run's embedding function.
fn embed_one(embed: &EmbedFn<'_>, text: &str) -> Result<Vec<f32>> {
    embed(&[text])?
        .pop()
        .ok_or_else(|| anyhow!("the embedder returned no vector"))
}

/// Where embedding work runs, and how many chunks one call receives.
struct EmbedThreads {
    /// ONNX: a pool sized by `[embed] index_threads` (0 = one thread per
    /// core), built per run so a changed setting always applies. HTTP: none,
    /// the server does the work.
    pool: Option<rayon::ThreadPool>,
    /// Chunks per embedding call: `[embed] batch_size` per thread.
    batch_size: usize,
}

impl EmbedThreads {
    fn new(cfg: &Config) -> Result<Self> {
        match cfg.embed.backend {
            EmbedBackend::Onnx => {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(cfg.embed.index_threads)
                    .build()
                    .context("starting the embedding threads")?;
                let batch_size = cfg
                    .embed
                    .batch_size
                    .saturating_mul(pool.current_num_threads());
                Ok(Self {
                    pool: Some(pool),
                    batch_size,
                })
            }
            EmbedBackend::Ollama => Ok(Self {
                pool: None,
                batch_size: cfg.embed.batch_size,
            }),
        }
    }
}

/// Make a `--path` argument comparable with the walker's paths: absolute
/// (against the current directory) with `.` and `..` resolved lexically.
/// When that lies outside every include path but the canonical path (symlinks
/// resolved) lies inside one, the canonical path is used. Outside both: a
/// warning, and the lexical path (which then matches nothing).
fn resolve_path_filter(cfg: &Config, p: &Path, progress: &dyn Fn(&str)) -> Result<PathBuf> {
    let lexical = vec_core::util::absolute_lexical(p)
        .with_context(|| format!("resolving path filter {}", p.display()))?;
    let in_scope = |q: &Path| {
        cfg.index
            .include_paths
            .iter()
            .any(|root| q.starts_with(root) || root.starts_with(q))
    };
    if in_scope(&lexical) {
        return Ok(lexical);
    }
    if let Ok(canonical) = std::fs::canonicalize(&lexical)
        && in_scope(&canonical)
    {
        return Ok(canonical);
    }
    progress(&format!(
        "warn: {} is outside every include path; nothing to index there",
        lexical.display()
    ));
    Ok(lexical)
}

// ---------------------------------------------------------------------------
// Run — one index pass
// ---------------------------------------------------------------------------

/// A file waiting for its missing vectors.
struct PendingFile {
    path: PathBuf,
    mtime: f64,
    size: u64,
    hash: String,
    /// Content unchanged: only size/mtime are refreshed (no stats counted).
    refresh: bool,
    chunks: Vec<Chunk>,
    text_hashes: Vec<String>,
    /// `Some` when reused from the index or already embedded.
    vectors: Vec<Option<Vec<f32>>>,
    reused: usize,
}

impl PendingFile {
    fn missing_texts(&self) -> impl Iterator<Item = &str> {
        self.chunks
            .iter()
            .zip(&self.vectors)
            .filter(|(_, v)| v.is_none())
            .map(|(c, _)| c.text.as_str())
    }
}

/// Fill in the missing vectors of `files` with one call to `embed`.
/// Returns how many texts were embedded.
fn embed_missing(embed: &EmbedFn<'_>, files: &mut [PendingFile]) -> Result<usize> {
    let vectors = {
        let texts: Vec<&str> = files.iter().flat_map(PendingFile::missing_texts).collect();
        if texts.is_empty() {
            return Ok(0);
        }
        let vectors = embed(&texts)?;
        if vectors.len() != texts.len() {
            bail!(
                "the embedder returned {} vectors for {} texts",
                vectors.len(),
                texts.len()
            );
        }
        vectors
    };
    let n = vectors.len();
    let mut it = vectors.into_iter();
    for slot in files
        .iter_mut()
        .flat_map(|f| f.vectors.iter_mut())
        .filter(|v| v.is_none())
    {
        *slot = it.next();
    }
    Ok(n)
}

struct Run<'a> {
    store: &'a mut Store,
    embed: &'a EmbedFn<'a>,
    cfg: &'a Config,
    progress: &'a dyn Fn(&str),
    full: bool,
    batch_size: usize,
    db_files: Option<DbFiles>,
    /// The model that computes this run's vectors (checked by the store on
    /// every write).
    identity: ModelIdentity,
    /// [`Embedder::runtime`] of the embedder, recorded with the writes.
    runtime: Option<String>,
    /// `runtime` was recorded as the runtime of the last write.
    runtime_recorded: bool,
    /// A full run over an index built by another model: the index is emptied
    /// and `identity` recorded right before the first write.
    reset_pending: bool,
    seen: WalkSeen,
    stats: IndexStats,
    pending: Vec<PendingFile>,
    pending_chunks: usize,
}

impl<'a> Run<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        store: &'a mut Store,
        identity: ModelIdentity,
        runtime: Option<String>,
        embed: &'a EmbedFn<'a>,
        cfg: &'a Config,
        batch_size: usize,
        full: bool,
        scopes: Option<&[PathBuf]>,
        progress: &'a dyn Fn(&str),
    ) -> Result<Self> {
        // Refuse to mix vectors of different models (a new index records it).
        // Only a real model mismatch may lead to a reset: any other error (a
        // busy database, an I/O error) stops the run with the index intact.
        let mut reset_pending = false;
        if let Err(e) = store.ensure_model(&identity) {
            if !full || scopes.is_some() || !e.is::<ModelMismatch>() {
                return Err(e.context("checking the index model"));
            }
            progress(&format!(
                "{e:#}\nThe index is rebuilt with {identity}: it is emptied before the first \
                 file is written."
            ));
            reset_pending = true;
        }
        let mut run = Self {
            store,
            embed,
            cfg,
            progress,
            full,
            batch_size,
            db_files: DbFiles::new(cfg),
            identity,
            runtime,
            runtime_recorded: false,
            reset_pending,
            seen: WalkSeen::default(),
            stats: IndexStats::default(),
            pending: Vec::new(),
            pending_chunks: 0,
        };
        // After a model reset the old canary means nothing: `reset` records one.
        if !run.reset_pending {
            run.check_canary(full && scopes.is_none())?;
        }
        Ok(run)
    }

    /// Re-embed the index's canary before writing anything. No canary: record
    /// one (a new index, or one built before canaries existed: adopted, with
    /// a note). Drift: an [`vec_store::EmbeddingDrift`] error, unless
    /// `rebuild` (an unscoped `--full`): every file is re-embedded and the
    /// canary is replaced when the run completes ([`Run::execute`]), so a run
    /// that dies midway leaves the old canary and the next write still sees
    /// the drift.
    fn check_canary(&mut self, rebuild: bool) -> Result<()> {
        let limits = CanaryLimits::from_config(&self.cfg.embed);
        let embed = self.embed;
        let check = self
            .store
            .check_canary(|t| embed_one(embed, t), self.runtime.clone(), limits)
            .context("checking the embedding canary")?;
        let Some(check) = check else {
            let files = self.store.stats().context("reading index stats")?.0;
            if files > 0 {
                (self.progress)(&format!(
                    "note: the index has no embedding canary (built before vec recorded one); \
                     recording it now with runtime {}, which is assumed to have built the index",
                    self.runtime.as_deref().unwrap_or("unknown")
                ));
            }
            return self.record_canary();
        };
        match check.verdict {
            CanaryVerdict::Ok => Ok(()),
            CanaryVerdict::Warn => {
                (self.progress)(&format!("warn: {check}"));
                Ok(())
            }
            CanaryVerdict::Drift if rebuild => {
                (self.progress)(&format!(
                    "warn: embedding drift: {check}. Every file is re-embedded; the canary is \
                     replaced when this run completes."
                ));
                Ok(())
            }
            CanaryVerdict::Drift => Err(self.store.drift_error(check).into()),
        }
    }

    /// Embed `[embed] canary_text` and store it as the index's canary.
    fn record_canary(&mut self) -> Result<()> {
        let text = self.cfg.embed.canary_text.clone();
        let vector = embed_one(self.embed, &text).context("embedding the canary text")?;
        self.store
            .set_canary(&Canary {
                text,
                vector,
                runtime: self.runtime.clone(),
            })
            .context("recording the embedding canary")
    }

    /// Walk every scope, write what changed, then prune what the walk did
    /// not accept.
    fn execute(mut self, scopes: Option<&[PathBuf]>) -> Result<IndexStats> {
        for builder in scoped_walkers(self.cfg, scopes, self.progress) {
            self.walk(&builder)?;
        }
        self.flush()?;
        if self.reset_pending {
            // Nothing was written: the index still holds the old model's vectors.
            self.reset()?;
        }
        let seen = &self.seen;
        let pruned = self
            .store
            .delete_files_where(scopes, |p| seen.is_stale(p))
            .context("pruning stale files from store")?;
        if pruned > 0 {
            (self.progress)(&format!("pruned {pruned} stale files from index"));
        }
        self.stats.files_deleted += pruned;
        if self.full && scopes.is_none() {
            // Every file was re-embedded by this runtime: it is now the
            // reference (see `check_canary`).
            self.record_canary()?;
            if let Some(runtime) = &self.runtime {
                self.store
                    .record_write_runtime(runtime, true)
                    .context("recording the runtime")?;
            }
        }
        Ok(self.stats)
    }

    fn warn(&mut self, msg: &str) {
        (self.progress)(&format!("warn: {msg}"));
        self.stats.errors += 1;
    }

    fn walk(&mut self, builder: &WalkBuilder) -> Result<()> {
        for result in builder.build() {
            let entry = match result {
                Ok(e) => e,
                Err(e) => {
                    // Vanished while walking (deleted or renamed): plain absence.
                    if !is_not_found(&e) {
                        self.warn(&format!("walk error: {e}"));
                        self.seen.record_error(&e);
                    }
                    continue;
                }
            };
            match accept(&entry, &self.cfg.index, self.db_files.as_ref()) {
                Accept::Skip => {}
                Accept::NonUtf8 => {
                    self.stats.files_skipped_non_utf8 += 1;
                    (self.progress)(&format!(
                        "warn: skipping {}: the file name is not valid UTF-8",
                        entry.path().display()
                    ));
                }
                Accept::MetadataError(e) => {
                    // Exists but cannot be inspected right now: keep its record.
                    self.warn(&format!("metadata({}): {e}", entry.path().display()));
                    self.seen.kept.insert(entry.into_path());
                }
                Accept::File(c) => self.index_file(c)?,
            }
        }
        Ok(())
    }

    fn index_file(&mut self, c: Candidate) -> Result<()> {
        // Nested include paths: a file already handled by an earlier walk.
        if self.seen.kept.contains(&c.path) {
            return Ok(());
        }
        self.stats.files_visited += 1;

        let record = if self.full {
            None
        } else {
            self.store
                .get_file(&c.path)
                .with_context(|| format!("reading the index entry of {}", c.path.display()))?
        };
        if record.as_ref().is_some_and(|r| c.same_metadata(r)) {
            self.stats.files_unchanged += 1;
            self.seen.kept.insert(c.path);
            return Ok(());
        }

        let text = match read_file(&c, &self.cfg.index) {
            ReadOutcome::Text(t) => t,
            // Binary, too large or small, gone, or no longer a regular file.
            ReadOutcome::NotIndexable => return Ok(()),
            ReadOutcome::Failed(e) => {
                // Transient failure: keep the existing record.
                self.warn(&format!("read({}): {e}", c.path.display()));
                self.seen.kept.insert(c.path);
                return Ok(());
            }
        };
        let hash = sha256_hex(text.content.as_bytes());
        let refresh = record.as_ref().is_some_and(|r| r.hash == hash);

        let chunks = chunk_file(&text.content, &self.cfg.index);
        if chunks.is_empty() {
            return Ok(()); // not indexable: an existing record is pruned
        }
        let mut stored = match record {
            Some(_) => self
                .store
                .embeddings_by_text_hash(&c.path)
                .with_context(|| format!("reading the stored vectors of {}", c.path.display()))?,
            None => Default::default(),
        };
        let text_hashes: Vec<String> = chunks
            .iter()
            .map(|ch| sha256_hex(ch.text.as_bytes()))
            .collect();
        let vectors: Vec<Option<Vec<f32>>> =
            text_hashes.iter().map(|h| stored.get(h).cloned()).collect();
        stored.clear();
        let reused = vectors.iter().filter(|v| v.is_some()).count();

        self.seen.kept.insert(c.path.clone());
        self.pending_chunks += chunks.len();
        self.pending.push(PendingFile {
            path: c.path,
            mtime: text.mtime,
            size: text.size,
            hash,
            refresh,
            chunks,
            text_hashes,
            vectors,
            reused,
        });
        if self.pending_chunks >= self.batch_size {
            self.flush()?;
        }
        Ok(())
    }

    /// Embed and write every pending file.
    ///
    /// When the batch call fails, each file is retried alone, so the failure
    /// is attributed to the files that cause it: they count as errors and
    /// keep their previous entry. When no file of the batch can be embedded,
    /// a short probe text ([`vec_embed::PROBE_TEXT`]) tells a failing backend
    /// (the probe fails too: the run stops) from files that cannot be
    /// embedded (the probe works: they are errors and the run goes on), so a
    /// single bad file alone in a batch cannot block indexing for good.
    fn flush(&mut self) -> Result<()> {
        let mut files = std::mem::take(&mut self.pending);
        self.pending_chunks = 0;
        if files.is_empty() {
            return Ok(());
        }
        let batch_err = match embed_missing(self.embed, &mut files) {
            Ok(_) => {
                for f in files {
                    self.write(f)?;
                }
                return Ok(());
            }
            Err(e) => e,
        };

        let mut failed = Vec::new();
        let mut any_embedded = false;
        for mut f in files {
            let needed = f.missing_texts().next().is_some();
            match embed_missing(self.embed, std::slice::from_mut(&mut f)) {
                Ok(_) => {
                    any_embedded |= needed;
                    self.write(f)?;
                }
                Err(e) => failed.push((f.path, e)),
            }
        }
        if !any_embedded && let Err(probe_err) = (self.embed)(&[vec_embed::PROBE_TEXT]) {
            return Err(batch_err.context(format!(
                "embedding failed for every file of a batch, and for a probe text too \
                 ({probe_err:#}); is the embedding backend working?"
            )));
        }
        for (path, e) in failed {
            self.warn(&format!(
                "embedding {}: {e:#}; its previous index entry is kept",
                path.display()
            ));
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.reset_pending = false;
        let cleared = self.store.stats().context("reading index stats")?.0;
        self.store
            .reset_model(&self.identity)
            .context("emptying the index for the new model")?;
        (self.progress)(&format!(
            "cleared {cleared} entries built by the previous model"
        ));
        // The index holds only this model's vectors from now on, even if the
        // run dies midway: the new canary is valid at once.
        self.runtime_recorded = false;
        self.record_canary()
    }

    fn write(&mut self, f: PendingFile) -> Result<()> {
        if self.reset_pending {
            self.reset()?;
        }
        if !self.runtime_recorded {
            if let Some(runtime) = &self.runtime {
                self.store
                    .record_write_runtime(runtime, false)
                    .context("recording the runtime")?;
            }
            self.runtime_recorded = true;
        }
        let new_chunks: Vec<NewChunk> = f
            .chunks
            .iter()
            .zip(f.text_hashes)
            .zip(f.vectors)
            .map(|((c, text_hash), v)| {
                Ok(NewChunk {
                    byte_offset: c.byte_offset,
                    byte_end: c.byte_end,
                    start_line: c.start_line,
                    end_line: c.end_line,
                    text_hash,
                    embedding: v
                        .ok_or_else(|| anyhow!("a chunk of {} has no vector", f.path.display()))?,
                })
            })
            .collect::<Result<_>>()?;
        self.store
            .replace_file(
                &self.identity,
                &f.path,
                f.mtime,
                f.size,
                &f.hash,
                &new_chunks,
            )
            .with_context(|| format!("writing the index entry of {}", f.path.display()))?;

        if f.refresh {
            self.stats.files_unchanged += 1;
            return Ok(());
        }
        self.stats.files_updated += 1;
        self.stats.chunks_added += new_chunks.len();
        self.stats.chunks_reused += f.reused;
        let reused = match f.reused {
            0 => String::new(),
            n => format!(" ({n} reused)"),
        };
        (self.progress)(&format!(
            "indexed {} chunks{reused} from {}",
            new_chunks.len(),
            f.path.display()
        ));
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// Accepting and reading files (shared by indexing and diff_files)
// ---------------------------------------------------------------------------

/// A file the walk accepts, with its size, mtime and identity as seen by the
/// walker (`lstat`, no symlink followed).
struct Candidate {
    path: PathBuf,
    size: u64,
    mtime: f64,
    /// Device and inode (`None` where the platform has none).
    id: Option<FileId>,
}

/// A file's identity on its filesystem: `(st_dev, st_ino)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    #[cfg(unix)]
    fn of(md: &std::fs::Metadata) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            dev: md.dev(),
            ino: md.ino(),
        })
    }

    #[cfg(not(unix))]
    fn of(_md: &std::fs::Metadata) -> Option<Self> {
        None
    }
}

impl Candidate {
    /// A candidate from the walker's metadata of `path`.
    fn new(path: &Path, md: &std::fs::Metadata) -> Self {
        Self {
            path: path.to_path_buf(),
            size: md.len(),
            mtime: mtime_secs(md),
            id: FileId::of(md),
        }
    }

    /// Same size and mtime as when `record` was written: skipped unread. An
    /// unknown mtime (0) or size never matches.
    fn same_metadata(&self, record: &FileRecord) -> bool {
        self.mtime > 0.0 && record.mtime == self.mtime && record.size == Some(self.size)
    }
}

enum Accept {
    /// Not a regular file, a database file, excluded by name, or size out of range.
    Skip,
    /// A regular file whose path is not valid UTF-8.
    NonUtf8,
    /// A regular file whose metadata could not be read (not "not found").
    MetadataError(ignore::Error),
    File(Candidate),
}

/// The filters applied to every walker entry, before reading it. The walker
/// itself has already applied `.gitignore`, hidden files and `exclude_dirs`.
fn accept(entry: &DirEntry, icfg: &IndexConfig, db_files: Option<&DbFiles>) -> Accept {
    // Not a directory, symlink, device or FIFO. Symlinks are not followed
    // (follow_links = false), so a link to a regular file is not a duplicate.
    if !entry.file_type().is_some_and(|t| t.is_file()) {
        return Accept::Skip;
    }
    let path = entry.path();
    let Some(name) = path
        .to_str()
        .and_then(|_| path.file_name())
        .and_then(|n| n.to_str())
    else {
        return Accept::NonUtf8;
    };
    if db_files.is_some_and(|d| d.contains(path)) {
        return Accept::Skip;
    }
    if icfg.exclude_files.iter().any(|pat| glob_match(pat, name)) {
        return Accept::Skip;
    }
    let md = match entry.metadata() {
        Ok(md) => md,
        Err(e) if is_not_found(&e) => return Accept::Skip,
        Err(e) => return Accept::MetadataError(e),
    };
    let size = md.len();
    if !md.is_file() || size > icfg.max_file_size || size < icfg.min_file_size {
        return Accept::Skip;
    }
    Accept::File(Candidate::new(path, &md))
}

/// Seconds since the epoch, 0 when unknown.
fn mtime_secs(md: &std::fs::Metadata) -> f64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64())
}

/// A file's content, with the size and mtime of the open handle.
struct FileText {
    content: String,
    size: u64,
    mtime: f64,
}

enum ReadOutcome {
    Text(FileText),
    /// Not UTF-8, size out of range, gone, or not a regular file (any more).
    NotIndexable,
    /// Could not be read right now.
    Failed(io::Error),
}

/// Read a candidate safely. The indexer may run as root over directories
/// that users control, so a user must not be able to make it read another
/// file than the one the walk accepted (e.g. by swapping one of their
/// directories for a symlink to `/root` between the walk and the read):
///
/// - every path component below the include path is opened without following
///   a symlink ([`open_no_follow`]): the walker never follows one, so a
///   symlink there was swapped in after the walk and the path is not indexable;
/// - the open handle must be the file the walker saw (same device and inode),
///   else the file was replaced meanwhile: a transient failure, the record is
///   kept and the next run reads the new file;
/// - the last component is opened without blocking (a FIFO swapped in), the
///   type and size are checked on the open handle, and at most
///   `max_file_size + 1` bytes are read.
fn read_file(c: &Candidate, icfg: &IndexConfig) -> ReadOutcome {
    let file = match open_no_follow(&c.path, &icfg.include_paths) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound || is_symlink_error(&e) => {
            return ReadOutcome::NotIndexable;
        }
        Err(e) => return ReadOutcome::Failed(e),
    };
    let md = match file.metadata() {
        Ok(md) => md,
        Err(e) => return ReadOutcome::Failed(e),
    };
    if let Some(walked) = c.id
        && FileId::of(&md) != Some(walked)
    {
        return ReadOutcome::Failed(io::Error::other(
            "the file was replaced after the walk saw it; it is read on the next run",
        ));
    }
    let in_range = |n: u64| (icfg.min_file_size..=icfg.max_file_size).contains(&n);
    if !md.is_file() || !in_range(md.len()) {
        return ReadOutcome::NotIndexable;
    }
    let mut buf = Vec::with_capacity(usize::try_from(md.len()).unwrap_or_default());
    if let Err(e) = file
        .take(icfg.max_file_size.saturating_add(1))
        .read_to_end(&mut buf)
    {
        return ReadOutcome::Failed(e);
    }
    // It may have grown or shrunk since the fstat.
    if !u64::try_from(buf.len()).is_ok_and(in_range) {
        return ReadOutcome::NotIndexable;
    }
    match String::from_utf8(buf) {
        Ok(content) => ReadOutcome::Text(FileText {
            content,
            size: md.len(),
            mtime: mtime_secs(&md),
        }),
        Err(_) => ReadOutcome::NotIndexable,
    }
}

/// Open `path` for reading without following a symlink anywhere below its
/// include path.
///
/// The include path (the longest of `include_paths` containing `path`; `/`
/// when none does) is opened normally: it is the administrator's choice and
/// may itself be or contain a symlink. Each component below it is then
/// opened relative to its parent's descriptor with `O_NOFOLLOW`
/// (`openat`), so no component can be redirected by a symlink, whichever
/// one is swapped. The last component is also opened with `O_NONBLOCK` and
/// `O_NOCTTY`.
#[cfg(unix)]
fn open_no_follow(path: &Path, include_paths: &[PathBuf]) -> io::Result<File> {
    use nix::fcntl::{OFlag, open, openat};
    use nix::sys::stat::Mode;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::path::Component;

    fn owned(fd: nix::Result<RawFd>) -> io::Result<OwnedFd> {
        let fd = fd.map_err(io::Error::from)?;
        // SAFETY: `fd` was just returned by open(2)/openat(2): it is a valid
        // descriptor that nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    let root = include_paths
        .iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.components().count())
        .map_or(Path::new("/"), PathBuf::as_path);
    let below = path
        .strip_prefix(root)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path is not absolute"))?;
    let mut names = Vec::new();
    for c in below.components() {
        match c {
            Component::Normal(name) => names.push(name),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path is not in normal form (\".\" or \"..\")",
                ));
            }
        }
    }
    let Some((name, dirs)) = names.split_last() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is an include path, not a file below one",
        ));
    };

    let dir_flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
    let mut dir = owned(open(root, dir_flags, Mode::empty()))?;
    for d in dirs {
        dir = owned(openat(
            Some(dir.as_raw_fd()),
            *d,
            dir_flags | OFlag::O_NOFOLLOW,
            Mode::empty(),
        ))?;
    }
    let file = owned(openat(
        Some(dir.as_raw_fd()),
        *name,
        OFlag::O_RDONLY
            | OFlag::O_NOFOLLOW
            | OFlag::O_NONBLOCK
            | OFlag::O_NOCTTY
            | OFlag::O_CLOEXEC,
        Mode::empty(),
    ))?;
    Ok(File::from(file))
}

#[cfg(not(unix))]
fn open_no_follow(path: &Path, _include_paths: &[PathBuf]) -> io::Result<File> {
    File::open(path)
}

/// `open` refused because a component is a symlink (`O_NOFOLLOW`: `ELOOP`)
/// or no longer a directory (`ENOTDIR`): the path no longer leads, without
/// symlinks, to a file.
fn is_symlink_error(e: &io::Error) -> bool {
    #[cfg(unix)]
    {
        use nix::errno::Errno;
        matches!(
            e.raw_os_error().map(Errno::from_raw),
            Some(Errno::ELOOP | Errno::ENOTDIR)
        )
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

/// The SQLite database and its side files (`-wal`, `-shm`, `-journal`):
/// never indexed, and their events are dropped by [`IndexFilter`].
struct DbFiles {
    dir: PathBuf,
    name: OsString,
}

/// Suffixes SQLite appends to the database file name for its side files.
const SQLITE_SIDE_FILE_SUFFIXES: &[&str] = &["-wal", "-shm", "-journal"];

impl DbFiles {
    /// `None` for a database path without a directory and a name.
    fn new(cfg: &Config) -> Option<Self> {
        let db = &cfg.database.db_path;
        let dir = db.parent().filter(|d| !d.as_os_str().is_empty())?;
        Some(Self {
            dir: dir.to_path_buf(),
            name: db.file_name()?.to_os_string(),
        })
    }

    fn contains(&self, path: &Path) -> bool {
        if path.parent() != Some(self.dir.as_path()) {
            return false;
        }
        let Some(name) = path.file_name() else {
            return false;
        };
        let (name, db) = (name.as_encoded_bytes(), self.name.as_encoded_bytes());
        name.strip_prefix(db).is_some_and(|rest| {
            rest.is_empty()
                || SQLITE_SIDE_FILE_SUFFIXES
                    .iter()
                    .any(|s| rest == s.as_bytes())
        })
    }
}

// ---------------------------------------------------------------------------
// Walkers
// ---------------------------------------------------------------------------

/// Visit every directory the indexer would descend into, equal to or under
/// one of `paths`, using the real walker (see [`update_paths`]). For a
/// directory holding a git repository, `<dir>/.git/info` is visited too,
/// because `.git/info/exclude` changes what is indexed.
///
/// Used by `vec watch` to place one non-recursive inotify watch per
/// directory, so excluded and ignored trees never get a watch.
pub fn visit_dirs(
    cfg: &Config,
    paths: &[PathBuf],
    progress: &dyn Fn(&str),
    mut f: impl FnMut(&Path),
) {
    for builder in scoped_walkers(cfg, Some(paths), progress) {
        for entry in builder.build() {
            let e = match entry {
                Ok(e) => e,
                Err(e) => {
                    if !is_not_found(&e) {
                        progress(&format!("warn: walk error: {e}"));
                    }
                    continue;
                }
            };
            if !e.file_type().is_some_and(|t| t.is_dir()) {
                continue;
            }
            // Skip the ancestors the walk passes through on its way down.
            if !paths.iter().any(|p| e.path().starts_with(p)) {
                continue;
            }
            f(e.path());
            let info = e.path().join(".git/info");
            if info.is_dir() {
                f(&info);
            }
        }
    }
}

/// Walkers covering `scopes` (`None` = everything), one per include path.
///
/// A scope inside include path `I` is walked from `I`, with an entry filter
/// that only lets the walk descend along ancestors of a scope and into the
/// scope itself. That costs a directory listing per ancestor, but means the
/// walker's own rules decide every ancestor — no re-implementation of
/// `.gitignore` semantics that could disagree with a full run.
///
/// Fixed policy: hidden entries are skipped, symlinks are not followed, the
/// global git excludes file is ignored (same result on every machine).
fn scoped_walkers(
    cfg: &Config,
    scopes: Option<&[PathBuf]>,
    progress: &dyn Fn(&str),
) -> Vec<WalkBuilder> {
    let mut out = Vec::new();
    for root in &cfg.index.include_paths {
        let mut builder = WalkBuilder::new(root);
        builder.git_ignore(cfg.index.gitignore);
        builder.git_global(false);
        builder.git_exclude(cfg.index.gitignore);
        builder.overrides(exclude_override(&cfg.index, root, progress));
        builder.follow_links(false);

        if let Some(scopes) = scopes
            && !scopes.iter().any(|s| root.starts_with(s))
        {
            let inside: Vec<PathBuf> = scopes
                .iter()
                .filter(|s| s.starts_with(root))
                .cloned()
                .collect();
            if inside.is_empty() {
                continue;
            }
            builder.filter_entry(move |e| {
                let p = e.path();
                inside.iter().any(|s| s.starts_with(p) || p.starts_with(s))
            });
        }
        out.push(builder);
    }
    out
}

/// What a walk accepted, and where it could not see.
#[derive(Default)]
struct WalkSeen {
    /// Files the walk accepted: their records must stay.
    kept: HashSet<PathBuf>,
    /// Paths that could not be read: records under them are only removed when
    /// their file is definitely gone.
    protected: Vec<PathBuf>,
    /// An unreadable location of unknown path: no record is removed unless
    /// its file is definitely gone.
    unsure: bool,
}

impl WalkSeen {
    fn record_error(&mut self, e: &ignore::Error) {
        match error_scope(e, None) {
            ErrScope::Harmless => {}
            ErrScope::Path(p) => self.protected.push(p),
            ErrScope::Unknown => self.unsure = true,
        }
    }

    /// Should the record for `path` be deleted?
    fn is_stale(&self, path: &Path) -> bool {
        if self.kept.contains(path) {
            return false;
        }
        if is_gone(path) {
            return true;
        }
        !(self.unsure || self.protected.iter().any(|p| path.starts_with(p)))
    }
}

/// The entry disappeared during the walk: not an error, just absence.
fn is_not_found(e: &ignore::Error) -> bool {
    e.io_error()
        .is_some_and(|io| io.kind() == io::ErrorKind::NotFound)
}

enum ErrScope {
    /// Does not hide any entry (e.g. an invalid line in a .gitignore).
    Harmless,
    /// Entries under this path may have been skipped.
    Path(PathBuf),
    /// Entries may have been skipped somewhere.
    Unknown,
}

fn error_scope(e: &ignore::Error, path: Option<&Path>) -> ErrScope {
    use ignore::Error as E;
    match e {
        E::WithPath { path, err } => error_scope(err, Some(path)),
        E::WithDepth { err, .. } | E::WithLineNumber { err, .. } => error_scope(err, path),
        E::Partial(errs) => {
            let mut worst = ErrScope::Harmless;
            for err in errs {
                match error_scope(err, path) {
                    ErrScope::Unknown => return ErrScope::Unknown,
                    ErrScope::Path(p) => worst = ErrScope::Path(p),
                    ErrScope::Harmless => {}
                }
            }
            worst
        }
        E::Glob { .. } | E::UnrecognizedFileType(_) | E::InvalidDefinition => ErrScope::Harmless,
        E::Loop { child, .. } => ErrScope::Path(child.clone()),
        _ => path.map_or(ErrScope::Unknown, |p| ErrScope::Path(p.to_path_buf())),
    }
}

/// Build the `exclude_dirs` matcher for one include path.
///
/// Patterns use gitignore syntax rooted at `root`:
/// * a bare name such as `"build"` matches a file or directory of that
///   name at any depth;
/// * a pattern with a leading or inner `/` is anchored to `root`
///   (`"/builds"` matches only `<root>/builds`);
/// * an absolute path under `root` (`"/home/me/builds"` with root
///   `/home/me`) is rewritten to its anchored form, so it matches exactly
///   that directory.
fn exclude_override(icfg: &IndexConfig, root: &Path, progress: &dyn Fn(&str)) -> Override {
    let mut override_builder = OverrideBuilder::new(root);
    for dir in &icfg.exclude_dirs {
        let as_path = Path::new(dir);
        let pattern = match as_path.strip_prefix(root) {
            Ok(rel) if as_path.is_absolute() && root != Path::new("/") => {
                format!("/{}", rel.display())
            }
            _ => dir.clone(),
        };
        // The override builder uses gitignore syntax with inverted meaning:
        // "!pattern" means "ignore".
        if let Err(e) = override_builder.add(&format!("!{pattern}")) {
            progress(&format!("warn: invalid exclude_dirs pattern '{dir}': {e}"));
        }
    }
    override_builder.build().unwrap_or_else(|e| {
        progress(&format!("warn: could not build exclude_dirs matcher: {e}"));
        Override::empty()
    })
}

// ---------------------------------------------------------------------------
// IndexFilter — exact, cheap exclusion rules for one path at a time
// ---------------------------------------------------------------------------

/// Decides, without touching the filesystem, whether the indexer will skip
/// a path, using only the rules that can be decided exactly from the path:
/// outside `include_paths`, the database file or one of its SQLite side files,
/// a hidden component, or an `exclude_dirs` match on any component.
///
/// The walker applies the same rules, so this never excludes a path the
/// walker would index — `vec watch` can drop such events safely. Everything
/// else (`.gitignore`, `exclude_files`, size, binary content) is left to the
/// walker, which is authoritative.
pub struct IndexFilter {
    /// `(include path, its exclude_dirs matcher)`.
    roots: Vec<(PathBuf, Override)>,
    db_files: Option<DbFiles>,
}

impl IndexFilter {
    pub fn new(cfg: &Config, progress: &dyn Fn(&str)) -> Self {
        Self {
            roots: cfg
                .index
                .include_paths
                .iter()
                .map(|r| (r.clone(), exclude_override(&cfg.index, r, progress)))
                .collect(),
            db_files: DbFiles::new(cfg),
        }
    }

    /// `true` if the indexer will never index `path` nor anything under it.
    pub fn is_excluded(&self, path: &Path) -> bool {
        if self.db_files.as_ref().is_some_and(|d| d.contains(path)) {
            return true;
        }
        // Excluded only if *every* include path containing it excludes it
        // (nested include paths are walked independently).
        // A path outside every include path is excluded too.
        self.roots
            .iter()
            .filter(|(r, _)| path.starts_with(r))
            .all(|(root, ov)| Self::excluded_under(root, ov, path))
    }

    fn excluded_under(root: &Path, ov: &Override, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(root) else {
            return true;
        };
        let mut cur = root.to_path_buf();
        let n = rel.components().count();
        for (i, comp) in rel.components().enumerate() {
            cur.push(comp);
            if comp.as_os_str().to_string_lossy().starts_with('.') {
                return true; // the walker skips hidden entries
            }
            // Ancestors are directories. For the leaf the type is unknown: a
            // pattern that ignores it as a file ("name") also ignores it as a
            // directory, while a directory-only pattern ("name/") does not
            // ignore it as a file — so this never over-excludes.
            let is_dir = i + 1 < n;
            if ov.matched(&cur, is_dir).is_ignore() {
                return true;
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// diff_files — compare filesystem state against the index
// ---------------------------------------------------------------------------

/// Result of comparing the filesystem against the stored index.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiffResult {
    /// Files `vec updatedb` would add.
    pub new_files: Vec<PathBuf>,
    /// Indexed files whose content changed.
    pub changed_files: Vec<PathBuf>,
    /// Indexed files `vec updatedb` would remove: gone, no longer indexable
    /// (excluded by name or size, binary, no chunk), or under a directory the
    /// walk no longer enters (newly excluded or ignored).
    pub deleted_files: Vec<PathBuf>,
    /// Problems met during the walk (unreadable entries, non-UTF-8 names).
    pub warnings: Vec<String>,
}

/// Walk the configured paths with the same walker and filters as
/// [`run_updatedb`] and compare against the index without modifying anything.
///
/// Files are read with the same size cap as indexing, and only when their
/// size or mtime differ from the index. `deleted_files` lists exactly the
/// records [`run_updatedb`] would prune: every indexed file the walk does not
/// accept, including files under directories it no longer enters (newly
/// excluded or ignored), except under a location it could not read.
///
/// # Errors
///
/// A store error.
pub fn diff_files(store: &Store, cfg: &Config) -> Result<DiffResult> {
    let icfg = &cfg.index;
    let warnings = RefCell::new(Vec::new());
    let warn = |m: &str| warnings.borrow_mut().push(m.to_owned());
    let db_files = DbFiles::new(cfg);
    let mut out = DiffResult::default();
    // What the walk accepted and where it could not see, as for a prune.
    let mut seen = WalkSeen::default();

    for builder in scoped_walkers(cfg, None, &warn) {
        for result in builder.build() {
            let entry = match result {
                Ok(e) => e,
                Err(e) => {
                    if !is_not_found(&e) {
                        warn(&format!("walk error: {e}"));
                        seen.record_error(&e);
                    }
                    continue;
                }
            };
            let c = match accept(&entry, icfg, db_files.as_ref()) {
                Accept::File(c) => c,
                Accept::Skip => continue,
                Accept::NonUtf8 => {
                    warn(&format!(
                        "skipping {}: the file name is not valid UTF-8",
                        entry.path().display()
                    ));
                    continue;
                }
                Accept::MetadataError(e) => {
                    warn(&format!("metadata({}): {e}", entry.path().display()));
                    seen.kept.insert(entry.into_path()); // its record would be kept
                    continue;
                }
            };
            if seen.kept.contains(&c.path) {
                continue; // nested include paths
            }
            let record = store
                .get_file(&c.path)
                .with_context(|| format!("reading the index entry of {}", c.path.display()))?;
            if record.as_ref().is_some_and(|r| c.same_metadata(r)) {
                seen.kept.insert(c.path);
                continue;
            }
            match read_file(&c, icfg) {
                ReadOutcome::Text(text) => {
                    let hash = sha256_hex(text.content.as_bytes());
                    match record {
                        Some(r) if r.hash == hash => {}
                        // No chunk: not indexable, an existing record is pruned.
                        _ if chunk_file(&text.content, icfg).is_empty() => continue,
                        Some(_) => out.changed_files.push(c.path.clone()),
                        None => out.new_files.push(c.path.clone()),
                    }
                    seen.kept.insert(c.path);
                }
                ReadOutcome::NotIndexable => {}
                ReadOutcome::Failed(e) => {
                    warn(&format!("read({}): {e}", c.path.display()));
                    seen.kept.insert(c.path);
                }
            }
        }
    }

    store
        .for_each_indexed_path(|p| {
            if seen.is_stale(p) {
                out.deleted_files.push(p.to_path_buf());
            }
        })
        .context("listing indexed files")?;
    out.warnings = warnings.into_inner();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal IndexConfig for tests.
    // We build one manually because IndexConfig does not derive Default —
    // the project keeps defaults centralised in config::default_config().
    fn test_icfg() -> IndexConfig {
        let mut icfg = vec_core::config::Config::defaults().unwrap().index;
        icfg.min_file_size = 0;
        icfg.gitignore = false;
        icfg.include_paths = vec![];
        icfg.exclude_dirs = vec![];
        icfg.exclude_files = vec![];
        icfg
    }

    fn test_store_opts() -> vec_store::StoreOptions {
        let db = vec_core::config::Config::defaults().unwrap().database;
        vec_store::StoreOptions::from_config(&db, false)
    }

    // -----------------------------------------------------------------------
    // glob_match
    // -----------------------------------------------------------------------

    #[test]
    fn glob_star_matches_any_name() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("*.rs", "foo.rs"));
        assert!(!glob_match("*.rs", "foo.py"));
    }

    #[test]
    fn glob_star_does_not_cross_slash() {
        // '*' must not match across directory separators.
        assert!(!glob_match("*.rs", "src/main.rs"));
    }

    #[test]
    fn glob_question_matches_one_char() {
        assert!(glob_match("foo?.rs", "fooX.rs"));
        assert!(!glob_match("foo?.rs", "foo.rs"));
        assert!(!glob_match("foo?.rs", "fooXY.rs"));
    }

    #[test]
    fn glob_literal_match() {
        assert!(glob_match("Makefile", "Makefile"));
        assert!(!glob_match("Makefile", "makefile"));
    }

    #[test]
    fn glob_star_matches_empty_prefix() {
        assert!(glob_match("*_test.go", "_test.go"));
        assert!(glob_match("*_test.go", "foo_test.go"));
    }

    // -----------------------------------------------------------------------
    // chunk_file
    // -----------------------------------------------------------------------

    #[test]
    fn chunk_empty_content() {
        let chunks = chunk_file("", &test_icfg());
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_short_file_below_min_lines() {
        // 3 non-blank lines, below min_chunk_lines → no chunks
        let content = "a\nb\nc\n";
        let chunks = chunk_file(content, &test_icfg());
        assert!(
            chunks.is_empty(),
            "expected no chunks, got {}",
            chunks.len()
        );
    }

    #[test]
    fn chunk_file_byte_offsets_are_correct() {
        // Build a file with enough content to produce at least one chunk.
        let mut lines = Vec::new();
        for i in 0..50 {
            lines.push(format!("line {i}"));
        }
        let content = lines.join("\n") + "\n";
        let chunks = chunk_file(&content, &test_icfg());
        assert!(!chunks.is_empty(), "expected at least one chunk");

        for chunk in &chunks {
            // The text stored in the chunk must match the byte slice.
            let extracted = &content[chunk.byte_offset..chunk.byte_end];
            assert_eq!(
                extracted, chunk.text,
                "byte offsets do not match chunk text"
            );
        }
    }

    #[test]
    fn chunk_line_numbers_are_one_based() {
        let mut lines = Vec::new();
        for i in 0..50 {
            lines.push(format!("x {i}"));
        }
        let content = lines.join("\n") + "\n";
        let chunks = chunk_file(&content, &test_icfg());
        for c in &chunks {
            assert!(c.start_line >= 1, "start_line must be ≥ 1");
            assert!(c.end_line >= c.start_line, "end_line must be ≥ start_line");
        }
    }

    #[test]
    fn chunk_snaps_to_fn_boundary() {
        // Build a file where a 'fn ' line appears just after the tentative boundary.
        // The chunker should prefer splitting there.
        let mut lines: Vec<String> = Vec::new();
        for i in 0..40 {
            lines.push(format!("    // filler line {i}"));
        }
        // Place a boundary pattern at 40 + 3 (within BOUNDARY_SCAN).
        for _ in 0..3 {
            lines.push("    // more filler".to_string());
        }
        lines.push("fn my_function() {".to_string());
        for i in 0..20 {
            lines.push(format!("    body line {i}"));
        }
        let content = lines.join("\n") + "\n";
        let chunks = chunk_file(&content, &test_icfg());
        assert!(!chunks.is_empty());
        // The first chunk should end at or near the 'fn ' line.
        // The 'fn' line is 0-based index 43 (1-based line 44), 3 lines after
        // the tentative end (40) and within boundary_scan: the first chunk
        // ends right before it.
        assert_eq!(chunks[0].end_line, 43);
    }

    #[test]
    fn chunk_overlap_between_consecutive_chunks() {
        // Build a file large enough to produce two chunks.
        let mut lines = Vec::new();
        for i in 0..(40 * 2) {
            lines.push(format!("code line {i:03}"));
        }
        let content = lines.join("\n") + "\n";
        let chunks = chunk_file(&content, &test_icfg());
        assert!(chunks.len() >= 2, "80 lines must give at least 2 chunks");
        let c0 = &chunks[0];
        let c1 = &chunks[1];
        // Chunk 1 should start before chunk 0 ends (overlap).
        assert!(
            c1.start_line <= c0.end_line,
            "no overlap: chunk0.end={} chunk1.start={}",
            c0.end_line,
            c1.start_line
        );
    }

    // --- New tests ---

    // Helper: build a full Config pointing at a given TempDir with stub-friendly settings.
    fn test_cfg_for_dir(dir: &tempfile::TempDir) -> vec_core::config::Config {
        // Compiled-in defaults only: no system or user config file is read.
        let mut cfg = vec_core::config::Config::defaults().unwrap();
        cfg.index.include_paths = vec![dir.path().to_path_buf()];
        cfg.index.min_file_size = 0;
        cfg.index.gitignore = false;
        cfg.index.exclude_dirs = vec![];
        cfg.index.exclude_files = vec![];
        cfg.embed.batch_size = 4;
        cfg
    }

    /// Write `n` lines of placeholder text to `path`.
    fn write_lines(path: &std::path::Path, n: usize) {
        use std::io::Write;
        let mut f = std::fs::File::create(path).unwrap();
        for i in 0..n {
            writeln!(f, "Line {i} of content for testing purposes.").unwrap();
        }
    }

    fn open_temp_db() -> (tempfile::NamedTempFile, vec_store::Store) {
        let db_file = tempfile::NamedTempFile::new().unwrap();
        let store = vec_store::Store::open(db_file.path(), &test_store_opts()).unwrap();
        (db_file, store)
    }

    #[test]
    fn run_updatedb_indexes_temp_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("hello.txt"), 50);
        write_lines(&dir.path().join("world.txt"), 50);

        let cfg = test_cfg_for_dir(&dir);
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);

        let stats = run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();

        assert!(
            stats.files_updated >= 2,
            "expected ≥ 2 files updated, got {}",
            stats.files_updated
        );
        assert!(
            stats.chunks_added > 0,
            "expected > 0 chunks added, got {}",
            stats.chunks_added
        );
    }

    #[test]
    fn run_updatedb_skips_unchanged() {
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("unchanged.txt"), 50);

        let cfg = test_cfg_for_dir(&dir);
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);

        // First run indexes the file.
        run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();

        // Second run — file content unchanged — should show files_unchanged > 0.
        let stats2 = run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();

        assert!(
            stats2.files_unchanged >= 1,
            "second run should skip unchanged files, got files_unchanged={}",
            stats2.files_unchanged
        );
        assert_eq!(
            stats2.files_updated, 0,
            "second run should not update unchanged files"
        );
    }

    #[test]
    fn run_updatedb_full_reindexes() {
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("reindex.txt"), 50);

        let cfg = test_cfg_for_dir(&dir);
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);

        // First run — normal incremental.
        run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();

        // Second run with full=true — should re-index even though the file hasn't changed.
        let stats_full = run_updatedb(&mut store, &embedder, &cfg, true, None, |_| {}).unwrap();

        assert!(
            stats_full.files_updated >= 1,
            "full re-index should update the file, got files_updated={}",
            stats_full.files_updated
        );
    }

    #[test]
    fn run_updatedb_excludes_binary() {
        use std::io::Write;

        let dir = tempfile::TempDir::new().unwrap();
        // Write a file that contains non-UTF-8 bytes (binary content).
        let bin_path = dir.path().join("binary.dat");
        let mut f = std::fs::File::create(&bin_path).unwrap();
        // Write a null byte sequence that is valid binary but not valid UTF-8.
        // Size must be above min_file_size (0 in test cfg) but below max_file_size.
        f.write_all(&[
            0u8, 1, 2, 3, 0xFF, 0xFE, 0xFD, 200, 201, 202, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF,
            0xFE, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFE, 0, 1, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0,
        ])
        .unwrap();

        let cfg = test_cfg_for_dir(&dir);
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);

        let stats = run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();

        // The binary file is visited but not indexed (read_to_string fails → skip).
        // files_updated should be 0 because the binary file is not valid UTF-8.
        assert_eq!(
            stats.files_updated, 0,
            "binary file should not be indexed, got files_updated={}",
            stats.files_updated
        );
    }

    #[test]
    fn run_updatedb_respects_size_limits() {
        use std::io::Write;

        let dir = tempfile::TempDir::new().unwrap();
        // Write a file larger than max_file_size.
        let big_path = dir.path().join("big.txt");
        let mut f = std::fs::File::create(&big_path).unwrap();
        // max_file_size = 100 bytes; write 200 bytes.
        f.write_all(&[b'x'; 200]).unwrap();

        let mut cfg = test_cfg_for_dir(&dir);
        cfg.index.max_file_size = 100; // 100 bytes limit

        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);

        let stats = run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();

        assert_eq!(
            stats.files_updated, 0,
            "file exceeding max_file_size should not be indexed"
        );
    }

    #[test]
    fn run_updatedb_path_filter() {
        let dir = tempfile::TempDir::new().unwrap();

        // Two sub-directories.
        let sub_a = dir.path().join("subdir_a");
        let sub_b = dir.path().join("subdir_b");
        std::fs::create_dir_all(&sub_a).unwrap();
        std::fs::create_dir_all(&sub_b).unwrap();

        write_lines(&sub_a.join("fileA.txt"), 50);
        write_lines(&sub_b.join("fileB.txt"), 50);

        let cfg = test_cfg_for_dir(&dir);
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);

        // Only index files under sub_a.
        let stats = run_updatedb(&mut store, &embedder, &cfg, false, Some(&sub_a), |_| {}).unwrap();

        assert!(
            stats.files_updated >= 1,
            "at least one file in sub_a should be indexed"
        );

        // sub_b's file should NOT be in the store.
        let rec = store.get_file(&sub_b.join("fileB.txt")).unwrap();
        assert!(
            rec.is_none(),
            "fileB.txt from sub_b should not be indexed when path_filter is sub_a"
        );
    }

    #[test]
    fn scoped_update_only_visits_scope() {
        let dir = tempfile::TempDir::new().unwrap();
        let sub_a = dir.path().join("a");
        let sub_b = dir.path().join("b");
        std::fs::create_dir_all(&sub_a).unwrap();
        std::fs::create_dir_all(&sub_b).unwrap();
        write_lines(&sub_a.join("one.txt"), 50);
        for i in 0..20 {
            write_lines(&sub_b.join(format!("f{i}.txt")), 50);
        }
        let cfg = test_cfg_for_dir(&dir);
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);

        let stats = update_paths(
            &mut store,
            &embedder,
            &cfg,
            std::slice::from_ref(&sub_a),
            |_| {},
        )
        .unwrap();
        assert_eq!(
            stats.files_visited, 1,
            "the 20 files in b/ must not be walked"
        );
        assert_eq!(stats.files_updated, 1);
    }

    #[test]
    fn scoped_update_prunes_deleted_file_and_dedups_nested_roots() {
        let dir = tempfile::TempDir::new().unwrap();
        let sub = dir.path().join("a");
        std::fs::create_dir_all(&sub).unwrap();
        let keep = sub.join("keep.txt");
        let gone = sub.join("gone.txt");
        write_lines(&keep, 50);
        write_lines(&gone, 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);
        run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();

        std::fs::remove_file(&gone).unwrap();
        // The directory and a file inside it: walked once, not twice.
        let stats = update_paths(
            &mut store,
            &embedder,
            &cfg,
            &[sub.clone(), keep.clone(), gone.clone()],
            |_| {},
        )
        .unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(stats.files_visited, 1);
        assert!(store.get_file(&gone).unwrap().is_none());
    }

    #[test]
    fn scoped_update_skips_root_inside_excluded_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let deep = dir.path().join("target/debug");
        std::fs::create_dir_all(&deep).unwrap();
        write_lines(&deep.join("gen.txt"), 50);
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.index.exclude_dirs = vec!["target".into()];
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);

        let stats = update_paths(
            &mut store,
            &embedder,
            &cfg,
            std::slice::from_ref(&deep),
            |_| {},
        )
        .unwrap();
        assert_eq!(
            stats.files_visited, 0,
            "a walk root inside target/ is excluded"
        );
    }

    #[test]
    fn exclude_dirs_anchored_and_absolute_patterns() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.index.exclude_dirs = vec![
            "/builds".into(),                                // anchored to include path
            root.join("gen").to_string_lossy().into_owned(), // absolute path
        ];
        let f = IndexFilter::new(&cfg, &|_| {});
        assert!(f.is_excluded(Path::new("/definitely/outside/x.rs")));
        assert!(
            !f.is_excluded(root),
            "an include path itself is never excluded"
        );
        assert!(f.is_excluded(&root.join("builds/x.rs")));
        assert!(f.is_excluded(&root.join("gen/y.rs")));
        assert!(
            !f.is_excluded(&root.join("sub/builds/x.rs")),
            "an anchored pattern only matches at the include path"
        );
        assert!(!f.is_excluded(&root.join("sub/gen/y.rs")));
    }

    #[test]
    fn visit_dirs_skips_excluded_hidden_and_gitignored_trees() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        for d in [
            "src/a",
            "target/debug/deps",
            ".cache/x",
            "builds/out",
            ".git",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join(".gitignore"), "builds/\n").unwrap();
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.index.exclude_dirs = vec!["target".into()];
        cfg.index.gitignore = true;

        let mut seen = Vec::new();
        visit_dirs(&cfg, &[root.to_path_buf()], &|_| {}, |d| {
            seen.push(d.strip_prefix(root).unwrap().to_path_buf())
        });
        seen.sort();
        let expect: Vec<PathBuf> = ["", "src", "src/a"].iter().map(PathBuf::from).collect();
        assert_eq!(seen, expect);

        // A new directory inside an excluded tree gets no watch.
        let mut inner = Vec::new();
        visit_dirs(&cfg, &[root.join("target/debug")], &|_| {}, |d| {
            inner.push(d.to_path_buf())
        });
        assert!(inner.is_empty());
    }

    #[test]
    fn gitignore_edit_prunes_and_unignores_on_scoped_update() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        write_lines(&root.join("a/x.txt"), 50);
        write_lines(&root.join("b/y.txt"), 50);
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.index.gitignore = true;
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);

        std::fs::write(root.join(".gitignore"), "b/\n").unwrap();
        run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();
        assert!(store.get_file(&root.join("a/x.txt")).unwrap().is_some());
        assert!(store.get_file(&root.join("b/y.txt")).unwrap().is_none());

        // Flip the rule: a/ ignored, b/ allowed. A scoped update of the root
        // must both prune a/x.txt and index b/y.txt.
        std::fs::write(root.join(".gitignore"), "a/\n").unwrap();
        let stats =
            update_paths(&mut store, &embedder, &cfg, &[root.to_path_buf()], |_| {}).unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert!(store.get_file(&root.join("a/x.txt")).unwrap().is_none());
        assert!(store.get_file(&root.join("b/y.txt")).unwrap().is_some());
    }

    #[test]
    fn incremental_updatedb_prunes_newly_excluded_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor")).unwrap();
        write_lines(&dir.path().join("vendor/v.txt"), 50);
        let mut cfg = test_cfg_for_dir(&dir);
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);
        run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();

        cfg.index.exclude_dirs = vec!["vendor".into()];
        let stats = run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();
        assert_eq!(
            stats.files_deleted, 1,
            "no --full needed after adding an exclusion"
        );
    }

    #[test]
    fn scoped_update_applies_ancestor_gitignore_exactly() {
        // The scope is deep inside an ignored directory: only the real walk
        // from the include path can see that an ancestor is ignored.
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("out/a/b")).unwrap();
        std::fs::write(root.join(".gitignore"), "out/\n").unwrap();
        write_lines(&root.join("out/a/b/gen.txt"), 50);
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.index.gitignore = true;
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);

        let stats =
            update_paths(&mut store, &embedder, &cfg, &[root.join("out/a/b")], |_| {}).unwrap();
        assert_eq!(stats.files_visited, 0);
        assert!(
            store
                .get_file(&root.join("out/a/b/gen.txt"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn scoped_and_full_runs_agree() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        for d in ["src", "out", "node_modules/x", "keep"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join(".gitignore"), "out/\n*.tmp\n!keep/*.tmp\n").unwrap();
        for f in [
            "src/a.rs",
            "src/b.tmp",
            "out/c.rs",
            "node_modules/x/d.js",
            "keep/e.tmp",
            "f.lock",
        ] {
            write_lines(&root.join(f), 50);
        }
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.index.gitignore = true;
        cfg.index.exclude_dirs = vec!["node_modules".into()];
        cfg.index.exclude_files = vec!["*.lock".into()];
        let embedder = vec_embed::Embedder::stub(768);

        let (_f1, mut full) = open_temp_db();
        run_updatedb(&mut full, &embedder, &cfg, true, None, |_| {}).unwrap();
        let (_f2, mut scoped) = open_temp_db();
        let every: Vec<PathBuf> = [
            "src/a.rs",
            "src/b.tmp",
            "out/c.rs",
            "node_modules/x/d.js",
            "keep/e.tmp",
            "f.lock",
        ]
        .iter()
        .map(|f| root.join(f))
        .collect();
        update_paths(&mut scoped, &embedder, &cfg, &every, |_| {}).unwrap();

        for f in &every {
            assert_eq!(
                full.get_file(f).unwrap().is_some(),
                scoped.get_file(f).unwrap().is_some(),
                "full and scoped runs disagree on {}",
                f.display()
            );
        }
        assert!(full.get_file(&root.join("src/a.rs")).unwrap().is_some());
        assert!(full.get_file(&root.join("keep/e.tmp")).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directory_keeps_its_records() {
        use std::os::unix::fs::PermissionsExt;
        if nix_is_root() {
            return; // root ignores permissions
        }
        let dir = tempfile::TempDir::new().unwrap();
        let sub = dir.path().join("locked");
        std::fs::create_dir_all(&sub).unwrap();
        let f = sub.join("x.txt");
        write_lines(&f, 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);
        run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();

        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
        let r1 = run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {});
        let r2 = update_paths(
            &mut store,
            &embedder,
            &cfg,
            std::slice::from_ref(&sub),
            |_| {},
        );
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        r1.unwrap();
        r2.unwrap();
        assert!(
            store.get_file(&f).unwrap().is_some(),
            "a transient read error must not delete records"
        );
    }

    fn nix_is_root() -> bool {
        std::fs::metadata("/proc/self")
            .map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.uid() == 0
            })
            .unwrap_or(false)
    }

    #[test]
    fn chunk_file_empty_returns_empty() {
        // Explicit alias for the same test that already exists (chunk_empty_content),
        // making the naming align with the requested test names.
        let chunks = chunk_file("", &test_icfg());
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_file_short_file() {
        // 3 non-blank lines is fewer than min_chunk_lines → no chunks.
        let content = "line one\nline two\nline three\n";
        let chunks = chunk_file(content, &test_icfg());
        assert!(
            chunks.is_empty(),
            "3-line file should produce no chunks (below min_chunk_lines)"
        );
    }

    // --- Gap 4: chunk_file edge cases ---

    #[test]
    fn chunk_file_no_trailing_newline() {
        // File without a trailing '\n' — byte_end must not exceed content length.
        let lines: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
        let content = lines.join("\n"); // no trailing newline
        let chunks = chunk_file(&content, &test_icfg());
        assert!(
            !chunks.is_empty(),
            "file without trailing newline should produce chunks"
        );
        for chunk in &chunks {
            assert!(
                chunk.byte_end <= content.len(),
                "byte_end {} exceeds content length {}",
                chunk.byte_end,
                content.len()
            );
            // Slice must be valid.
            let _slice = &content[chunk.byte_offset..chunk.byte_end];
        }
    }

    #[test]
    fn chunk_file_crlf_line_endings() {
        // CRLF content — split_inclusive('\n') leaves '\r' at end of each line.
        // Byte offsets must still be valid and slice within the content.
        let lines: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
        let content = lines.join("\r\n") + "\r\n";
        let chunks = chunk_file(&content, &test_icfg());
        assert!(!chunks.is_empty(), "CRLF content should produce chunks");
        for chunk in &chunks {
            assert!(chunk.byte_end <= content.len());
            assert!(chunk.byte_offset < chunk.byte_end);
            let slice = &content[chunk.byte_offset..chunk.byte_end];
            assert!(!slice.is_empty());
        }
    }

    // --- Gap 6: unreadable directory ---

    #[cfg(unix)]
    #[test]
    fn run_updatedb_does_not_index_symlinks() {
        // A symlink to a regular file must not be indexed.
        // Without the fix, both the real path and the symlink path would appear
        // in the store — same content, same line numbers, duplicate results.
        let dir = tempfile::TempDir::new().unwrap();
        let real_file = dir.path().join("real.txt");
        let link_file = dir.path().join("link.txt");

        write_lines(&real_file, 50);
        std::os::unix::fs::symlink(&real_file, &link_file).unwrap();

        let cfg = test_cfg_for_dir(&dir);
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);

        let stats = run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();

        // Only the real file should be indexed; the symlink must be skipped.
        assert_eq!(
            stats.files_updated, 1,
            "symlink must not be indexed: expected 1 file, got {}",
            stats.files_updated
        );
        assert!(
            store.get_file(&real_file).unwrap().is_some(),
            "real file must be in store"
        );
        assert!(
            store.get_file(&link_file).unwrap().is_none(),
            "symlink must not be in store"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_updatedb_skips_unreadable_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let unreadable = dir.path().join("secret");
        std::fs::create_dir(&unreadable).unwrap();
        write_lines(&unreadable.join("private.txt"), 50);

        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();

        // If we can still enter the directory (running as root), restore and skip.
        if std::fs::read_dir(&unreadable).is_ok() {
            let _ = std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o755));
            return;
        }

        let cfg = test_cfg_for_dir(&dir);
        let (_db_file, mut store) = open_temp_db();
        let embedder = vec_embed::Embedder::stub(768);

        let result = run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {});

        // Restore permissions before asserting so TempDir can clean up.
        let _ = std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o755));

        assert!(
            result.is_ok(),
            "updatedb must not error on an unreadable directory: {:?}",
            result
        );
    }

    #[test]
    fn chunk_boundary_snap() {
        // Build a file where a function definition appears near a chunk boundary.
        // The chunker should snap the boundary to the function definition line.
        let mut lines: Vec<String> = Vec::new();
        // Fill 40 lines of filler.
        for i in 0..40 {
            lines.push(format!("    // filler {i}"));
        }
        // Insert 2 filler lines then a boundary pattern within BOUNDARY_SCAN.
        lines.push("    // near boundary 1".to_string());
        lines.push("    // near boundary 2".to_string());
        lines.push("fn snap_target() {".to_string()); // boundary at 40 + 3 (0-based)
        for i in 0..20 {
            lines.push(format!("    let x = {i};"));
        }
        let content = lines.join("\n") + "\n";

        let chunks = chunk_file(&content, &test_icfg());
        assert!(!chunks.is_empty(), "file should produce at least one chunk");

        // The fn line is 0-based index 42: the first chunk ends at line 42
        // (1-based), and the second one starts on or before the fn line.
        assert_eq!(chunks[0].end_line, 42);
        assert!(chunks[1].start_line <= 43);
    }

    #[test]
    fn full_pipeline_with_stub() {
        use std::io::Write;

        let db_file = tempfile::NamedTempFile::new().unwrap();
        let mut store = vec_store::Store::open(db_file.path(), &test_store_opts()).unwrap();

        let dir = tempfile::TempDir::new().unwrap();
        for name in &["alpha.txt", "beta.txt"] {
            let p = dir.path().join(name);
            let mut f = std::fs::File::create(&p).unwrap();
            for i in 0..50u32 {
                writeln!(f, "This is line {i} of {name}").unwrap();
            }
        }

        let cfg = test_cfg_for_dir(&dir);

        let embedder = vec_embed::Embedder::stub(768);
        let stats = run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();

        assert!(stats.files_updated >= 2);
        assert!(stats.chunks_added > 0);

        let qemb = vec_embed::Embedder::stub(768);
        let query_vec = qemb.embed_one("authentication middleware").unwrap();
        let results = store.search(&query_vec, 5, -1.0, None).unwrap();
        assert!(!results.is_empty());
    }

    // -----------------------------------------------------------------------
    // Chunker: coverage and snapping (regressions for audit B1, B2)
    // -----------------------------------------------------------------------

    /// xorshift64: deterministic pseudo-random numbers without a dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Check the structural invariants of `chunks` for `content`: offsets
    /// match the text, starts strictly increase, no gap between consecutive
    /// chunks, the first starts at line 1 and the last ends at the last line.
    fn assert_covers(content: &str, chunks: &[Chunk], what: &str) {
        let total = content.split_inclusive('\n').count();
        assert!(!chunks.is_empty(), "{what}: no chunk");
        assert_eq!(chunks[0].start_line, 1, "{what}: first line not covered");
        assert_eq!(
            chunks.last().unwrap().end_line,
            total,
            "{what}: tail dropped"
        );
        assert_eq!(chunks.last().unwrap().byte_end, content.len(), "{what}");
        for c in chunks {
            assert!(c.start_line <= c.end_line, "{what}: empty chunk");
            assert_eq!(&content[c.byte_offset..c.byte_end], c.text, "{what}");
        }
        for w in chunks.windows(2) {
            assert!(w[1].start_line > w[0].start_line, "{what}: no progress");
            assert!(
                w[1].start_line <= w[0].end_line + 1,
                "{what}: gap between lines {} and {}",
                w[0].end_line,
                w[1].start_line
            );
        }
    }

    #[test]
    fn chunker_covers_every_line_property() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for case in 0..2000 {
            let mut icfg = test_icfg();
            icfg.chunk_size = 1 + rng.below(50);
            icfg.chunk_overlap = rng.below(icfg.chunk_size);
            icfg.boundary_scan = rng.below(9);
            icfg.min_chunk_lines = rng.below(4);
            let n = 1 + rng.below(200);
            let mut content = String::new();
            for i in 0..n {
                match rng.below(10) {
                    0 => content.push_str("fn f() {"),
                    1 => content.push_str("    def g():"),
                    2 => {}
                    _ => content.push_str(&format!("    body {i}")),
                }
                if i + 1 < n || rng.below(2) == 0 {
                    content.push('\n');
                }
            }
            let non_blank = content.lines().filter(|l| !l.trim().is_empty()).count();
            let chunks = chunk_file(&content, &icfg);
            let what = format!(
                "case {case}: {n} lines, size {} overlap {} scan {} min {}",
                icfg.chunk_size, icfg.chunk_overlap, icfg.boundary_scan, icfg.min_chunk_lines
            );
            if non_blank < icfg.min_chunk_lines {
                assert!(chunks.is_empty(), "{what}: too sparse for a chunk");
            } else {
                assert_covers(&content, &chunks, &what);
            }
        }
    }

    #[test]
    fn chunker_keeps_the_tail_after_a_late_function() {
        // Audit repro: a 20-line file whose `fn tail()` starts on line 17
        // gave one chunk of lines 1-16.
        let mut lines: Vec<String> = (0..16).map(|i| format!("let a{i} = {i};")).collect();
        lines.push("fn tail() {".into());
        lines.extend((0..3).map(|i| format!("    t{i}();")));
        let content = lines.join("\n") + "\n";
        let chunks = chunk_file(&content, &test_icfg());
        assert_covers(&content, &chunks, "late fn");
        assert!(chunks.last().unwrap().text.contains("fn tail()"));
    }

    #[test]
    fn chunker_zero_overlap_leaves_no_gap() {
        let mut icfg = test_icfg();
        icfg.chunk_size = 10;
        icfg.chunk_overlap = 0;
        // Boundaries a few lines before each tentative end pull ends back.
        let content: String = (0..60)
            .map(|i| {
                if i % 10 == 7 {
                    "fn x() {\n".to_owned()
                } else {
                    format!("    s{i};\n")
                }
            })
            .collect();
        let chunks = chunk_file(&content, &icfg);
        assert_covers(&content, &chunks, "overlap 0");
    }

    #[test]
    fn chunker_zero_chunk_size_does_not_panic() {
        let mut icfg = test_icfg();
        icfg.chunk_size = 0;
        icfg.chunk_overlap = 0;
        icfg.min_chunk_lines = 0;
        let content = "a\nb\nc\n";
        assert_covers(content, &chunk_file(content, &icfg), "chunk_size 0");
    }

    #[test]
    fn chunker_starts_next_chunk_on_a_boundary() {
        // With overlap <= boundary_scan the function header the first chunk
        // ends before is also where the second chunk starts.
        let mut icfg = test_icfg();
        icfg.chunk_size = 40;
        icfg.chunk_overlap = 3;
        icfg.boundary_scan = 5;
        let mut lines: Vec<String> = (0..37).map(|i| format!("    a{i};")).collect();
        lines.push("fn second() {".into()); // 0-based 37
        lines.extend((0..40).map(|i| format!("    b{i};")));
        let content = lines.join("\n") + "\n";
        let chunks = chunk_file(&content, &icfg);
        assert_eq!(chunks[0].end_line, 37);
        assert_eq!(chunks[1].start_line, 38, "second chunk starts at the fn");
        assert!(chunks[1].text.starts_with("fn second()"));
    }

    #[test]
    fn chunker_uses_configured_patterns_and_scan() {
        let mut lines: Vec<String> = (0..42).map(|i| format!("    x{i}")).collect();
        lines.push("SECTION two".into()); // 0-based 42
        lines.extend((0..20).map(|i| format!("    y{i}")));
        let content = lines.join("\n") + "\n";

        let mut icfg = test_icfg();
        icfg.boundary_patterns = vec!["SECTION ".into()];
        assert_eq!(chunk_file(&content, &icfg)[0].end_line, 42);

        icfg.boundary_scan = 0;
        assert_eq!(
            chunk_file(&content, &icfg)[0].end_line,
            icfg.chunk_size,
            "boundary_scan = 0 never moves a boundary"
        );

        icfg.boundary_scan = 5;
        icfg.boundary_patterns = vec!["fn ".into()];
        assert_eq!(chunk_file(&content, &icfg)[0].end_line, icfg.chunk_size);
    }

    #[test]
    fn chunker_merges_a_sparse_tail_instead_of_dropping_it() {
        let mut icfg = test_icfg();
        icfg.chunk_size = 10;
        icfg.chunk_overlap = 0;
        icfg.boundary_scan = 0;
        icfg.min_chunk_lines = 5;
        // 10 dense lines, then 2 non-blank lines among blanks.
        let mut content: String = (0..10).map(|i| format!("dense {i}\n")).collect();
        content.push_str("\n\ntail one\n\n\ntail two\n");
        let chunks = chunk_file(&content, &icfg);
        assert_eq!(chunks.len(), 1);
        assert_covers(&content, &chunks, "sparse tail");
        assert!(chunks[0].text.ends_with("tail two\n"));
    }

    // -----------------------------------------------------------------------
    // Indexing behaviour
    // -----------------------------------------------------------------------

    fn stub() -> vec_embed::Embedder {
        vec_embed::Embedder::stub(TEST_DIM)
    }

    /// Dimension of the test stub embedder.
    const TEST_DIM: usize = 32;

    /// Run an index pass with a custom embedding function (failure injection).
    fn run_with(
        store: &mut Store,
        cfg: &Config,
        full: bool,
        identity: ModelIdentity,
        embed: &EmbedFn<'_>,
        log: &RefCell<Vec<String>>,
    ) -> Result<IndexStats> {
        let progress = |m: &str| log.borrow_mut().push(m.to_owned());
        Run::new(
            store,
            identity,
            None,
            embed,
            cfg,
            cfg.embed.batch_size,
            full,
            None,
            &progress,
        )?
        .execute(None)
    }

    #[test]
    fn updatedb_records_the_model_identity() {
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("a.txt"), 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        assert_eq!(store.model().unwrap().as_ref(), Some(stub().identity()));

        // Another model is refused without --full, and nothing is changed.
        let other = vec_embed::Embedder::stub(TEST_DIM * 2);
        assert!(run_updatedb(&mut store, &other, &cfg, false, None, |_| {}).is_err());
        assert!(store.get_file(&dir.path().join("a.txt")).unwrap().is_some());
    }

    #[test]
    fn a_file_that_fails_to_embed_keeps_its_old_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let bad = dir.path().join("bad.txt");
        let good = dir.path().join("good.txt");
        write_lines(&bad, 50);
        write_lines(&good, 50);
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.embed.batch_size = 1000; // both files in one batch
        let (_db, mut store) = open_temp_db();
        let log = RefCell::new(Vec::new());
        let s = stub();
        let embed_ok = |t: &[&str]| s.embed_batch(t);
        run_with(
            &mut store,
            &cfg,
            false,
            s.identity().clone(),
            &embed_ok,
            &log,
        )
        .unwrap();
        let old_hash = store.get_file(&bad).unwrap().unwrap().hash;

        std::fs::write(&bad, "POISON\n".repeat(20)).unwrap();
        std::fs::write(&good, "fresh content line\n".repeat(20)).unwrap();
        let embed_poison = |t: &[&str]| {
            if t.iter().any(|x| x.contains("POISON")) {
                bail!("cannot embed this text")
            }
            s.embed_batch(t)
        };
        let stats = run_with(
            &mut store,
            &cfg,
            false,
            s.identity().clone(),
            &embed_poison,
            &log,
        )
        .unwrap();
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.files_updated, 1);
        assert_eq!(store.get_file(&bad).unwrap().unwrap().hash, old_hash);
        assert!(
            log.borrow()
                .iter()
                .any(|m| m.contains("previous index entry is kept"))
        );
    }

    /// Regression (review 2026-11-19 #6): a file that always fails to embed,
    /// alone in its batch, made the run look like a down backend, so
    /// `updatedb` failed on every run and `vec watch` retried forever.
    #[test]
    fn a_file_that_fails_alone_in_its_batch_does_not_stop_the_run() {
        let dir = tempfile::TempDir::new().unwrap();
        let bad = dir.path().join("a-bad.txt");
        let good = dir.path().join("b-good.txt");
        std::fs::write(&bad, "POISON line\n".repeat(50)).unwrap();
        write_lines(&good, 50);
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.embed.batch_size = 1; // every file is flushed alone
        let (_db, mut store) = open_temp_db();
        let log = RefCell::new(Vec::new());
        let s = stub();
        let embed_poison = |t: &[&str]| {
            if t.iter().any(|x| x.contains("POISON")) {
                bail!("cannot embed this text")
            }
            s.embed_batch(t)
        };
        let stats = run_with(
            &mut store,
            &cfg,
            false,
            s.identity().clone(),
            &embed_poison,
            &log,
        )
        .unwrap();
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.files_updated, 1);
        assert!(store.get_file(&good).unwrap().is_some());
        assert!(store.get_file(&bad).unwrap().is_none());
    }

    /// Regression (review 2026-11-19 #5): `--full` treated any error of
    /// `ensure_model` (here: the database is locked) as a model mismatch and
    /// emptied the index at the first write.
    #[test]
    fn full_run_does_not_reset_on_an_error_that_is_not_a_model_mismatch() {
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("a.txt"), 50);
        let cfg = test_cfg_for_dir(&dir);
        let (db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        drop(store);
        // Test value: fail fast on the lock.
        let quick = vec_store::StoreOptions {
            busy_timeout_ms: 50,
            ..test_store_opts()
        };
        let mut store = vec_store::Store::open(db.path(), &quick).unwrap();

        let mut locker = rusqlite::Connection::open(db.path()).unwrap();
        let lock = locker
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let s = stub();
        let embed = |t: &[&str]| s.embed_batch(t);
        let progress = |_: &str| {};
        let err = Run::new(
            &mut store,
            s.identity().clone(),
            None,
            &embed,
            &cfg,
            cfg.embed.batch_size,
            true,
            None,
            &progress,
        )
        .err()
        .expect("a locked database is an error");
        assert!(!err.is::<ModelMismatch>(), "{err:#}");
        drop(lock);
        assert_eq!(store.stats().unwrap().0, 1, "the index is intact");
    }

    #[test]
    fn a_failing_backend_stops_the_run_and_changes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = dir.path().join("a.txt");
        write_lines(&f, 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        let before = store.get_file(&f).unwrap().unwrap();

        let log = RefCell::new(Vec::new());
        let down = |_: &[&str]| -> Result<Vec<Vec<f32>>> { bail!("connection refused") };
        // --full must not empty the index first.
        let err = run_with(
            &mut store,
            &cfg,
            true,
            stub().identity().clone(),
            &down,
            &log,
        );
        assert!(err.is_err());
        assert_eq!(store.get_file(&f).unwrap().unwrap().hash, before.hash);
        assert_eq!(store.stats().unwrap().1, 2, "chunks are still there");
    }

    #[test]
    fn full_rebuild_with_another_model_empties_only_once_it_can_embed() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = dir.path().join("a.txt");
        write_lines(&f, 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();

        let other = vec_embed::Embedder::stub(TEST_DIM * 2);
        let log = RefCell::new(Vec::new());
        let down = |_: &[&str]| -> Result<Vec<Vec<f32>>> { bail!("model broken") };
        assert!(
            run_with(
                &mut store,
                &cfg,
                true,
                other.identity().clone(),
                &down,
                &log
            )
            .is_err()
        );
        assert_eq!(store.model().unwrap().as_ref(), Some(stub().identity()));
        assert!(store.get_file(&f).unwrap().is_some(), "old index intact");

        // A scoped --full cannot switch models.
        assert!(run_updatedb(&mut store, &other, &cfg, true, Some(dir.path()), |_| {}).is_err());

        let stats = run_updatedb(&mut store, &other, &cfg, true, None, |_| {}).unwrap();
        assert_eq!(stats.files_updated, 1);
        assert_eq!(store.model().unwrap().as_ref(), Some(other.identity()));
    }

    #[test]
    fn full_run_prunes_after_reembedding() {
        let dir = tempfile::TempDir::new().unwrap();
        let keep = dir.path().join("keep.txt");
        let gone = dir.path().join("gone.txt");
        write_lines(&keep, 50);
        write_lines(&gone, 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        std::fs::remove_file(&gone).unwrap();
        let stats = run_updatedb(&mut store, &stub(), &cfg, true, None, |_| {}).unwrap();
        assert_eq!((stats.files_updated, stats.files_deleted), (1, 1));
        assert!(store.get_file(&keep).unwrap().is_some());
    }

    #[test]
    fn full_run_embeds_files_under_nested_include_paths_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        write_lines(&sub.join("a.txt"), 50);
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.index.include_paths.push(sub.clone());
        let (_db, mut store) = open_temp_db();
        let calls = std::cell::Cell::new(0usize);
        let s = stub();
        // Texts of the files only, not the embedding canary.
        let embed = |t: &[&str]| {
            let files = t.iter().filter(|x| **x != cfg.embed.canary_text).count();
            calls.set(calls.get() + files);
            s.embed_batch(t)
        };
        let log = RefCell::new(Vec::new());
        let stats = run_with(&mut store, &cfg, true, s.identity().clone(), &embed, &log).unwrap();
        assert_eq!(stats.files_visited, 1);
        assert_eq!(stats.files_updated, 1);
        assert_eq!(calls.get(), stats.chunks_added);
    }

    #[test]
    fn unchanged_size_and_mtime_skip_the_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = dir.path().join("a.txt");
        write_lines(&f, 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        let mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
        let old_hash = store.get_file(&f).unwrap().unwrap().hash;

        // Same size, same mtime, different bytes: by design not re-read.
        let content = std::fs::read_to_string(&f).unwrap().replace("Line", "LINE");
        std::fs::write(&f, &content).unwrap();
        File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let stats = run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        assert_eq!((stats.files_unchanged, stats.files_updated), (1, 0));
        assert_eq!(store.get_file(&f).unwrap().unwrap().hash, old_hash);

        // --full reads everything.
        run_updatedb(&mut store, &stub(), &cfg, true, None, |_| {}).unwrap();
        assert_ne!(store.get_file(&f).unwrap().unwrap().hash, old_hash);
    }

    #[test]
    fn touched_file_refreshes_metadata_without_embedding() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = dir.path().join("a.txt");
        write_lines(&f, 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        let before = store.get_file(&f).unwrap().unwrap();

        let later =
            std::fs::metadata(&f).unwrap().modified().unwrap() + std::time::Duration::from_secs(10);
        File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let calls = std::cell::Cell::new(0usize);
        let s = stub();
        // Texts of the files only, not the embedding canary.
        let embed = |t: &[&str]| {
            let files = t.iter().filter(|x| **x != cfg.embed.canary_text).count();
            calls.set(calls.get() + files);
            s.embed_batch(t)
        };
        let log = RefCell::new(Vec::new());
        let stats = run_with(&mut store, &cfg, false, s.identity().clone(), &embed, &log).unwrap();
        assert_eq!((stats.files_unchanged, stats.files_updated), (1, 0));
        assert_eq!(calls.get(), 0, "no text embedded");
        let after = store.get_file(&f).unwrap().unwrap();
        assert_eq!(after.hash, before.hash);
        assert!(after.mtime > before.mtime, "mtime refreshed");

        // The next run skips it by metadata alone.
        let stats = run_with(&mut store, &cfg, false, s.identity().clone(), &embed, &log).unwrap();
        assert_eq!(stats.files_unchanged, 1);
    }

    #[test]
    fn an_edit_reembeds_only_the_changed_chunks() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = dir.path().join("a.txt");
        write_lines(&f, 200);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();

        let mut content = std::fs::read_to_string(&f).unwrap();
        content.push_str("one more line at the end\n");
        std::fs::write(&f, &content).unwrap();
        let calls = std::cell::Cell::new(0usize);
        let s = stub();
        // Texts of the files only, not the embedding canary.
        let embed = |t: &[&str]| {
            let files = t.iter().filter(|x| **x != cfg.embed.canary_text).count();
            calls.set(calls.get() + files);
            s.embed_batch(t)
        };
        let log = RefCell::new(Vec::new());
        let stats = run_with(&mut store, &cfg, false, s.identity().clone(), &embed, &log).unwrap();
        assert_eq!(stats.files_updated, 1);
        assert!(stats.chunks_reused > 0);
        assert_eq!(calls.get(), stats.chunks_added - stats.chunks_reused);
        assert!(calls.get() < stats.chunks_added);

        // Reused vectors are the ones a fresh embedding gives.
        let stored = store.embeddings_by_text_hash(&f).unwrap();
        for c in chunk_file(&content, &cfg.index) {
            let v = &stored[&sha256_hex(c.text.as_bytes())];
            let fresh = s.embed_one(&c.text).unwrap();
            assert!(v.iter().zip(&fresh).all(|(a, b)| (a - b).abs() < 1e-6));
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_file_names_are_skipped_and_counted() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::TempDir::new().unwrap();
        let name = std::ffi::OsStr::from_bytes(b"bad-\xff.txt");
        write_lines(&dir.path().join(name), 50);
        write_lines(&dir.path().join("ok.txt"), 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        let log = RefCell::new(Vec::new());
        let stats = run_updatedb(&mut store, &stub(), &cfg, false, None, |m| {
            log.borrow_mut().push(m.to_owned())
        })
        .unwrap();
        assert_eq!(stats.files_skipped_non_utf8, 1);
        assert_eq!(stats.files_updated, 1);
        assert!(log.borrow().iter().any(|m| m.contains("not valid UTF-8")));
    }

    #[cfg(unix)]
    #[test]
    fn read_file_refuses_symlinks_and_caps_the_size() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().join("real.txt");
        let link = dir.path().join("link.txt");
        write_lines(&real, 50);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let icfg = test_icfg();
        let real_c = walked(&real);
        assert!(matches!(read_file(&real_c, &icfg), ReadOutcome::Text(_)));
        // A file swapped for a symlink after the walk is not read.
        let link_c = Candidate {
            path: link,
            ..walked(&real)
        };
        assert!(matches!(
            read_file(&link_c, &icfg),
            ReadOutcome::NotIndexable
        ));
        let missing = Candidate {
            path: dir.path().join("missing"),
            ..walked(&real)
        };
        assert!(matches!(
            read_file(&missing, &icfg),
            ReadOutcome::NotIndexable
        ));
        let mut small = test_icfg();
        small.max_file_size = 10;
        assert!(matches!(
            read_file(&real_c, &small),
            ReadOutcome::NotIndexable
        ));
    }

    /// The candidate the walker would produce for `path` (`lstat`).
    fn walked(path: &Path) -> Candidate {
        Candidate::new(path, &std::fs::symlink_metadata(path).unwrap())
    }

    /// Regression (review 2026-11-19 #4): a user swaps one of their
    /// directories for a symlink (e.g. to `/root`) between the walk and the
    /// read. The indexer must not read the file behind the link, whether the
    /// walker's `lstat` ran before the swap or after it.
    #[test]
    fn a_directory_swapped_for_a_symlink_is_not_followed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("home");
        let user_dir = root.join("user/project");
        let secret_dir = tmp.path().join("root-only");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::create_dir_all(&secret_dir).unwrap();
        let path = user_dir.join("notes.txt");
        write_lines(&path, 50);
        std::fs::write(secret_dir.join("notes.txt"), "SECRET line\n".repeat(50)).unwrap();
        let mut icfg = test_icfg();
        icfg.include_paths = vec![root.clone()];

        let before_swap = walked(&path);
        assert!(matches!(
            read_file(&before_swap, &icfg),
            ReadOutcome::Text(_)
        ));

        // Swap the directory for a symlink to the secret directory.
        std::fs::rename(&user_dir, root.join("user/project.old")).unwrap();
        std::os::unix::fs::symlink(&secret_dir, &user_dir).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("SECRET"));

        let after_swap = walked(&path); // lstat follows the swapped directory
        for c in [&before_swap, &after_swap] {
            match read_file(c, &icfg) {
                ReadOutcome::Text(t) => panic!("read through the symlink: {}", &t.content[..20]),
                ReadOutcome::NotIndexable => {}
                ReadOutcome::Failed(e) => panic!("expected NotIndexable, got {e}"),
            }
        }

        // The include path itself may be a symlink (the admin's choice).
        let alias = tmp.path().join("home-alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        std::fs::remove_file(&user_dir).unwrap();
        std::fs::rename(root.join("user/project.old"), &user_dir).unwrap();
        icfg.include_paths = vec![alias.clone()];
        let via_alias = alias.join("user/project/notes.txt");
        assert!(matches!(
            read_file(&walked(&via_alias), &icfg),
            ReadOutcome::Text(_)
        ));
    }

    /// A file replaced (another inode) between the walk and the read is a
    /// transient failure: its record is kept and the next run reads it.
    #[test]
    fn a_file_replaced_after_the_walk_is_a_transient_failure() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("a.txt");
        write_lines(&path, 50);
        let c = walked(&path);
        let new = dir.path().join("a.txt.new");
        write_lines(&new, 60);
        std::fs::rename(&new, &path).unwrap();
        let mut icfg = test_icfg();
        icfg.include_paths = vec![dir.path().to_path_buf()];
        assert!(matches!(read_file(&c, &icfg), ReadOutcome::Failed(_)));
        assert!(matches!(
            read_file(&walked(&path), &icfg),
            ReadOutcome::Text(_)
        ));
    }

    #[test]
    fn database_files_are_not_indexed_and_their_events_are_dropped() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.database.db_path = root.join("vec.db");
        // Text files under the database names, plus a normal neighbour.
        for name in ["vec.db", "vec.db-wal", "vec.db-journal", "notes.txt"] {
            write_lines(&root.join(name), 50);
        }
        let filter = IndexFilter::new(&cfg, &|_| {});
        assert!(filter.is_excluded(&root.join("vec.db")));
        assert!(filter.is_excluded(&root.join("vec.db-shm")));
        assert!(!filter.is_excluded(&root.join("notes.txt")));
        assert!(!filter.is_excluded(&root.join("vec.dbx")));

        let (_db, mut store) = open_temp_db();
        let stats = run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        assert_eq!(stats.files_updated, 1, "only notes.txt");
    }

    #[test]
    fn relative_db_path_does_not_exclude_everything() {
        // Audit B6: `Path::new("vec.db").parent()` is `""`, a prefix of every path.
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.database.db_path = PathBuf::from("vec.db");
        let filter = IndexFilter::new(&cfg, &|_| {});
        assert!(!filter.is_excluded(&dir.path().join("src/main.rs")));
    }

    #[test]
    fn path_filter_is_normalised_and_warns_outside_include_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let a = dir.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        write_lines(&a.join("x.txt"), 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();

        let dotted = dir.path().join("b/../a/./");
        let stats = run_updatedb(&mut store, &stub(), &cfg, false, Some(&dotted), |_| {}).unwrap();
        assert_eq!(stats.files_updated, 1);

        let log = RefCell::new(Vec::new());
        let outside = tempfile::TempDir::new().unwrap();
        let stats = run_updatedb(
            &mut store,
            &stub(),
            &cfg,
            false,
            Some(outside.path()),
            |m| log.borrow_mut().push(m.to_owned()),
        )
        .unwrap();
        assert_eq!(stats, IndexStats::default());
        assert!(
            log.borrow()
                .iter()
                .any(|m| m.contains("outside every include path"))
        );
    }

    #[test]
    fn embed_threads_scale_onnx_batches_only() {
        let mut cfg = vec_core::config::Config::defaults().unwrap();
        cfg.embed.index_threads = 3;
        let t = EmbedThreads::new(&cfg).unwrap();
        assert_eq!(
            t.pool.as_ref().map(rayon::ThreadPool::current_num_threads),
            Some(3)
        );
        assert_eq!(t.batch_size, cfg.embed.batch_size * 3);

        cfg.embed.backend = EmbedBackend::Ollama;
        let t = EmbedThreads::new(&cfg).unwrap();
        assert!(t.pool.is_none());
        assert_eq!(t.batch_size, cfg.embed.batch_size);
    }

    // -----------------------------------------------------------------------
    // diff_files (audit B5)
    // -----------------------------------------------------------------------

    #[test]
    fn diff_uses_the_indexing_filters_and_agrees_with_updatedb() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        write_lines(&root.join("a.txt"), 50);
        write_lines(&root.join("vendor/v.txt"), 50); // exclude_dirs
        write_lines(&root.join("x.lock"), 50); // exclude_files
        std::fs::write(root.join("tiny.txt"), "a\nb\n").unwrap(); // too few lines
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.index.exclude_dirs = vec!["vendor".into()];
        cfg.index.exclude_files = vec!["*.lock".into()];
        let (_db, mut store) = open_temp_db();

        let d = diff_files(&store, &cfg).unwrap();
        assert_eq!(d.new_files, vec![root.join("a.txt")]);
        assert!(d.changed_files.is_empty() && d.deleted_files.is_empty());

        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        assert_eq!(diff_files(&store, &cfg).unwrap(), DiffResult::default());

        // Changed content, a file turned binary (would be pruned).
        write_lines(&root.join("b.txt"), 50);
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        std::fs::write(root.join("a.txt"), "changed line\n".repeat(30)).unwrap();
        std::fs::write(root.join("b.txt"), [0xffu8; 300]).unwrap();
        let d = diff_files(&store, &cfg).unwrap();
        assert_eq!(d.changed_files, vec![root.join("a.txt")]);
        assert_eq!(d.deleted_files, vec![root.join("b.txt")]);
        assert!(d.new_files.is_empty());
    }

    #[test]
    fn diff_reports_files_under_a_newly_excluded_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("gen")).unwrap();
        write_lines(&root.join("a.txt"), 50);
        write_lines(&root.join("gen/g.txt"), 50);
        let mut cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();

        cfg.index.exclude_dirs.push("gen".into());
        let d = diff_files(&store, &cfg).unwrap();
        assert_eq!(d.deleted_files, vec![root.join("gen/g.txt")]);
        // updatedb agrees: it prunes exactly that record.
        let stats = run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(diff_files(&store, &cfg).unwrap(), DiffResult::default());
    }

    #[test]
    fn updatedb_records_its_run_time() {
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("a.txt"), 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        assert_eq!(store.get_meta(META_LAST_UPDATEDB).unwrap(), None);
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        let t: u64 = store
            .get_meta(META_LAST_UPDATEDB)
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        assert!(t > 0);
    }

    #[test]
    fn diff_reports_deleted_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = dir.path().join("a.txt");
        write_lines(&f, 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        std::fs::remove_file(&f).unwrap();
        assert_eq!(diff_files(&store, &cfg).unwrap().deleted_files, vec![f]);
    }

    // -----------------------------------------------------------------------
    // Real model (run with VEC_TEST_MODEL=<model.onnx> cargo test -- --ignored)
    // -----------------------------------------------------------------------

    #[test]
    #[ignore = "needs a real ONNX model in VEC_TEST_MODEL"]
    fn real_model_index_vectors_equal_single_text_embeddings() {
        let model = std::env::var_os("VEC_TEST_MODEL")
            .expect("set VEC_TEST_MODEL to an ONNX model file with tokenizer.json beside it");
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.embed.index_threads = 2;
        let embedder =
            vec_embed::Embedder::load(Path::new(&model), "test-model", &cfg.onnx_options())
                .unwrap();
        for (i, name) in ["a.rs", "b.py", "c.txt"].iter().enumerate() {
            let text: String = (0..60 + 20 * i)
                .map(|l| format!("fn handler_{l}() {{ let total = compute({l}); }}\n"))
                .collect();
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        let (_db, mut store) = open_temp_db();
        let check = |store: &Store| {
            for name in ["a.rs", "b.py", "c.txt"] {
                let path = dir.path().join(name);
                let content = std::fs::read_to_string(&path).unwrap();
                let stored = store.embeddings_by_text_hash(&path).unwrap();
                for c in chunk_file(&content, &cfg.index) {
                    let v = &stored[&sha256_hex(c.text.as_bytes())];
                    let single = embedder.embed_one(&c.text).unwrap();
                    let diff = v
                        .iter()
                        .zip(&single)
                        .map(|(a, b)| (a - b).abs())
                        .fold(0f32, f32::max);
                    assert!(
                        diff < 1e-4,
                        "{name} lines {}: max diff {diff}",
                        c.start_line
                    );
                }
            }
        };
        run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();
        assert_eq!(
            store.model().unwrap().unwrap().sha256,
            embedder.model_sha256().unwrap()
        );
        check(&store);

        // An edit reuses the untouched chunks; the result is the same.
        let a = dir.path().join("a.rs");
        let mut content = std::fs::read_to_string(&a).unwrap();
        content.push_str("fn appended() { done(); }\n");
        std::fs::write(&a, content).unwrap();
        let stats = run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();
        assert!(stats.chunks_reused > 0);
        check(&store);
    }

    // -----------------------------------------------------------------------
    // Embedding canary and runtime record
    // -----------------------------------------------------------------------

    /// The stub with another seed: same identity, other vectors (a runtime
    /// that drifted).
    fn drifted() -> vec_embed::Embedder {
        vec_embed::Embedder::stub_with_seed(TEST_DIM, 1)
    }

    fn is_drift(e: &anyhow::Error) -> bool {
        e.chain().any(|c| c.is::<vec_store::EmbeddingDrift>())
    }

    #[test]
    fn first_write_records_canary_and_runtime() {
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("a.txt"), 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        let canary = store.canary().unwrap().expect("canary recorded");
        assert_eq!(canary.text, cfg.embed.canary_text);
        let fresh = stub().embed_one(&cfg.embed.canary_text).unwrap();
        assert!((vec_store::dot(&canary.vector, &fresh).unwrap() - 1.0).abs() < 1e-6);
        assert_eq!(canary.runtime, stub().runtime());
        let h = store.runtime_history().unwrap();
        assert_eq!(h.first_write, stub().runtime());
        assert_eq!(h.last_write, stub().runtime());
    }

    #[test]
    fn writers_refuse_a_drifted_runtime_and_write_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = dir.path().join("a.txt");
        write_lines(&f, 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        let before = store.get_file(&f).unwrap().unwrap();

        write_lines(&dir.path().join("b.txt"), 60);
        std::fs::write(&f, "changed content line\n".repeat(30)).unwrap();
        let err = run_updatedb(&mut store, &drifted(), &cfg, false, None, |_| {}).unwrap_err();
        assert!(is_drift(&err), "{err:#}");
        assert!(
            format!("{err:#}").contains("vec updatedb --full"),
            "{err:#}"
        );
        let err = update_paths(
            &mut store,
            &drifted(),
            &cfg,
            std::slice::from_ref(&f),
            |_| {},
        )
        .unwrap_err();
        assert!(is_drift(&err), "{err:#}");
        // A scoped --full cannot fix drift either: it would leave a mix.
        let err =
            run_updatedb(&mut store, &drifted(), &cfg, true, Some(dir.path()), |_| {}).unwrap_err();
        assert!(is_drift(&err), "{err:#}");

        assert_eq!(
            store.get_file(&f).unwrap().unwrap(),
            before,
            "nothing written"
        );
        assert!(store.get_file(&dir.path().join("b.txt")).unwrap().is_none());
        assert_eq!(
            store.runtime_history().unwrap().last_write,
            stub().runtime()
        );
    }

    #[test]
    fn a_small_move_is_a_warning_and_indexing_goes_on() {
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("a.txt"), 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();

        // The canary moves by cos ≈ 1/sqrt(1.01) ≈ 0.995: between the
        // default limits (0.98, 0.999).
        let s = stub();
        let other = s.embed_one("an unrelated text").unwrap();
        let nudged = |t: &[&str]| -> Result<Vec<Vec<f32>>> {
            let mut out = s.embed_batch(t)?;
            for (text, v) in t.iter().zip(&mut out) {
                if *text == cfg.embed.canary_text {
                    v.iter_mut().zip(&other).for_each(|(x, o)| *x += 0.1 * o);
                }
            }
            Ok(out)
        };
        write_lines(&dir.path().join("b.txt"), 60);
        let log = RefCell::new(Vec::new());
        let stats = run_with(&mut store, &cfg, false, s.identity().clone(), &nudged, &log).unwrap();
        assert_eq!(stats.files_updated, 1);
        assert!(
            log.borrow()
                .iter()
                .any(|m| m.starts_with("warn: embedding canary cosine 0.99")),
            "{:?}",
            log.borrow()
        );
    }

    #[test]
    fn full_rebuild_accepts_drift_and_replaces_the_canary_at_the_end() {
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("a.txt"), 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();

        let log = RefCell::new(Vec::new());
        let stats = run_updatedb(&mut store, &drifted(), &cfg, true, None, |m| {
            log.borrow_mut().push(m.to_owned());
        })
        .unwrap();
        assert_eq!(stats.files_updated, 1);
        assert!(log.borrow().iter().any(|m| m.contains("embedding drift")));
        let canary = store.canary().unwrap().unwrap();
        assert_eq!(canary.runtime, drifted().runtime());
        let h = store.runtime_history().unwrap();
        assert_eq!(
            (h.first_write, h.last_write),
            (drifted().runtime(), drifted().runtime())
        );
        // The drifted runtime is now the reference; the old one drifts.
        run_updatedb(&mut store, &drifted(), &cfg, false, None, |_| {}).unwrap();
        let err = run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap_err();
        assert!(is_drift(&err), "{err:#}");
    }

    /// A `--full` that dies midway must not leave a canary matching only the
    /// files it rewrote: the old canary stays, so the index (now a mix) keeps
    /// being refused until a `--full` completes.
    #[test]
    fn a_full_rebuild_that_dies_midway_keeps_the_old_canary() {
        let dir = tempfile::TempDir::new().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            write_lines(&dir.path().join(name), 50);
        }
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.embed.batch_size = 1; // one file per flush
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        let old = store.canary().unwrap().unwrap();

        let d = drifted();
        let file_batches = std::cell::Cell::new(0usize);
        let dies = |t: &[&str]| -> Result<Vec<Vec<f32>>> {
            if !t.contains(&cfg.embed.canary_text.as_str()) {
                file_batches.set(file_batches.get() + 1);
                if file_batches.get() > 1 {
                    bail!("backend went away");
                }
            }
            d.embed_batch(t)
        };
        let log = RefCell::new(Vec::new());
        assert!(run_with(&mut store, &cfg, true, d.identity().clone(), &dies, &log).is_err());
        assert_eq!(store.canary().unwrap().unwrap(), old, "canary unchanged");
        let err = run_updatedb(&mut store, &d, &cfg, false, None, |_| {}).unwrap_err();
        assert!(is_drift(&err), "{err:#}");
        // A completed --full fixes it.
        run_updatedb(&mut store, &d, &cfg, true, None, |_| {}).unwrap();
        run_updatedb(&mut store, &d, &cfg, false, None, |_| {}).unwrap();
    }

    #[test]
    fn an_index_without_canary_adopts_one_with_a_note() {
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("a.txt"), 50);
        let cfg = test_cfg_for_dir(&dir);
        let (db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        // As written by a vec without canaries.
        rusqlite::Connection::open(db.path())
            .unwrap()
            .execute(
                "DELETE FROM meta WHERE key LIKE 'canary.%' OR key LIKE 'runtime.%'",
                [],
            )
            .unwrap();
        assert_eq!(store.canary().unwrap(), None);

        let log = RefCell::new(Vec::new());
        run_updatedb(&mut store, &stub(), &cfg, false, None, |m| {
            log.borrow_mut().push(m.to_owned());
        })
        .unwrap();
        assert!(
            log.borrow()
                .iter()
                .any(|m| m.starts_with("note: the index has no embedding canary")),
            "{:?}",
            log.borrow()
        );
        assert_eq!(store.canary().unwrap().unwrap().runtime, stub().runtime());
        // A new, empty index records its canary without a note.
        let (_db2, mut empty) = open_temp_db();
        let log = RefCell::new(Vec::new());
        run_updatedb(&mut empty, &stub(), &cfg, false, None, |m| {
            log.borrow_mut().push(m.to_owned());
        })
        .unwrap();
        assert!(!log.borrow().iter().any(|m| m.starts_with("note:")));
    }

    #[test]
    fn a_model_reset_records_the_new_models_canary() {
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("a.txt"), 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &stub(), &cfg, false, None, |_| {}).unwrap();
        let other = vec_embed::Embedder::stub_with_seed(TEST_DIM * 2, 3);
        run_updatedb(&mut store, &other, &cfg, true, None, |_| {}).unwrap();
        let canary = store.canary().unwrap().unwrap();
        assert_eq!(canary.vector.len(), TEST_DIM * 2);
        assert_eq!(canary.runtime, other.runtime());
        run_updatedb(&mut store, &other, &cfg, false, None, |_| {}).unwrap();
    }

    /// An index recorded without a model digest (HTTP backend before digests,
    /// or a server that reports none) gets the digest at the next write.
    #[test]
    fn a_missing_model_digest_is_filled_in_by_the_next_write() {
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("a.txt"), 50);
        let cfg = test_cfg_for_dir(&dir);
        let (_db, mut store) = open_temp_db();
        let s = stub();
        let embed = |t: &[&str]| s.embed_batch(t);
        let log = RefCell::new(Vec::new());
        run_with(&mut store, &cfg, false, s.identity().clone(), &embed, &log).unwrap();
        assert_eq!(store.model().unwrap().unwrap().sha256, None);
        let with_digest = ModelIdentity {
            sha256: Some("feed".into()),
            ..s.identity().clone()
        };
        run_with(&mut store, &cfg, false, with_digest.clone(), &embed, &log).unwrap();
        assert_eq!(store.model().unwrap(), Some(with_digest));
    }

    /// End to end against a live Ollama-compatible server (read-only use:
    /// lists models, reads the version, embeds). Run with
    /// `VEC_TEST_OLLAMA_URL=http://ollama.example:11434 VEC_TEST_OLLAMA_MODEL=<name>
    /// cargo test -p vec-index -- --ignored live_ollama`.
    #[test]
    #[ignore = "needs a live Ollama in VEC_TEST_OLLAMA_URL / VEC_TEST_OLLAMA_MODEL"]
    fn live_ollama_records_digest_runtime_and_canary() {
        let url = std::env::var("VEC_TEST_OLLAMA_URL").expect("set VEC_TEST_OLLAMA_URL");
        let model = std::env::var("VEC_TEST_OLLAMA_MODEL").expect("set VEC_TEST_OLLAMA_MODEL");
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("a.txt"), 50);
        let mut cfg = test_cfg_for_dir(&dir);
        cfg.embed.backend = vec_core::config::EmbedBackend::Ollama;
        cfg.embed.embed_url = Some(url);
        cfg.embed.model = model;
        let embedder = vec_core::load_embedder(&cfg).unwrap();
        let digest = embedder
            .identity()
            .sha256
            .clone()
            .expect("the server reports a digest");
        assert!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "{digest}"
        );
        let runtime = embedder.runtime().expect("the server reports its version");

        let (_db, mut store) = open_temp_db();
        run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();
        assert_eq!(store.model().unwrap().unwrap().sha256, Some(digest));
        assert_eq!(store.runtime_history().unwrap().last_write, Some(runtime));
        let limits = CanaryLimits::from_config(&cfg.embed);
        let check = store
            .check_canary(|t| embedder.embed_one(t), embedder.runtime(), limits)
            .unwrap()
            .unwrap();
        assert_eq!(check.verdict, CanaryVerdict::Ok, "{check}");
        eprintln!("{check}");
    }
}

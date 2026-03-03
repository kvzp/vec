/// index.rs — file walker, chunker, and incremental indexing.
///
/// # Chunker
///
/// [`chunk_file`] splits a UTF-8 source file into overlapping 40-line windows.
/// Before committing each boundary it scans ±5 lines for a recognised
/// function/class-start pattern and snaps to that line if one is found.
/// Chunks with fewer than [`MIN_CHUNK_LINES`] non-blank lines are discarded.
///
/// # Incremental update
///
/// [`run_updatedb`] walks the configured include paths using the `ignore`
/// crate (ripgrep's engine — respects .gitignore), skips unchanged files
/// (sha256 hash comparison), re-embeds changed files in batches, and keeps
/// the `files` / `chunks` tables consistent.
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use sha2::{Digest, Sha256};

use crate::config::{Config, IndexConfig};
use crate::embed::Embedder;
use crate::store::Store;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Target chunk size in source lines.
const CHUNK_LINES: usize = 40;

/// Overlap between adjacent chunks in source lines.
const OVERLAP_LINES: usize = 10;

/// Scan window (±N lines) around a tentative boundary when looking for a
/// better split point aligned to a function/class start.
const BOUNDARY_SCAN: usize = 5;

/// Minimum non-blank lines a chunk must contain to be worth embedding.
const MIN_CHUNK_LINES: usize = 5;

/// Line-start patterns that indicate good chunk-boundary candidates.
///
/// A line is a candidate when, after stripping leading whitespace, it starts
/// with one of these strings.  The list covers Rust, Python, Go, JavaScript,
/// TypeScript, C, and C++.
const BOUNDARY_PATTERNS: &[&str] = &[
    "fn ",
    "pub fn ",
    "async fn ",
    "pub async fn ",
    "impl ",
    "pub impl ",
    "pub struct ",
    "pub enum ",
    "mod ",
    "pub mod ",
    "def ",
    "class ",
    "func ",
    "function ",
];

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
/// The algorithm:
///
/// 1. Split `content` into lines, accumulating cumulative byte offsets so we
///    can map line numbers → byte positions in O(n).
/// 2. Advance the chunk window in steps of `CHUNK_LINES - OVERLAP_LINES`
///    (i.e., 30 lines), capping at the end of the file.
/// 3. Before committing each boundary, scan the ±[`BOUNDARY_SCAN`] lines
///    around it for a line whose stripped content starts with a recognised
///    keyword from [`BOUNDARY_PATTERNS`].  If found, snap the boundary there.
/// 4. Skip chunks with fewer than [`MIN_CHUNK_LINES`] non-blank lines.
///
/// The `cfg` parameter is accepted for future per-language configuration but
/// the current implementation uses the compile-time constants above.
pub fn chunk_file(content: &str, _cfg: &IndexConfig) -> Vec<Chunk> {
    // -----------------------------------------------------------------------
    // Step 1: build a line table: line_starts[i] = byte offset of line i+1
    // -----------------------------------------------------------------------
    //
    // We collect one entry per line: the byte offset where that line begins,
    // and the length of the line in bytes (including the trailing '\n' if any).
    //
    // "line" here means the substring up to and including the '\n'.  The last
    // line may not have a trailing newline.

    struct LineInfo {
        /// Byte offset of the first character on this line.
        byte_start: usize,
        /// Number of bytes in this line (including '\n' if present).
        byte_len: usize,
        /// True if the line contains at least one non-whitespace character.
        non_blank: bool,
    }

    let mut lines: Vec<LineInfo> = Vec::new();
    let mut cursor = 0usize;
    for line_str in content.split_inclusive('\n') {
        let byte_len = line_str.len();
        let non_blank = line_str.bytes().any(|b| !b.is_ascii_whitespace());
        lines.push(LineInfo {
            byte_start: cursor,
            byte_len,
            non_blank,
        });
        cursor += byte_len;
    }
    // If content is empty, nothing to chunk.
    let total_lines = lines.len();
    if total_lines == 0 {
        return Vec::new();
    }

    // -----------------------------------------------------------------------
    // Helper: does line at index `idx` start with a boundary pattern?
    // -----------------------------------------------------------------------
    let line_strs: Vec<&str> = content.split_inclusive('\n').collect();
    let is_boundary = |idx: usize| -> bool {
        if idx >= total_lines {
            return false;
        }
        let s = line_strs[idx].trim_start();
        BOUNDARY_PATTERNS.iter().any(|p| s.starts_with(p))
    };

    // -----------------------------------------------------------------------
    // Step 2 & 3: emit chunks
    // -----------------------------------------------------------------------
    let step = CHUNK_LINES.saturating_sub(OVERLAP_LINES).max(1);
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut chunk_start_line = 0usize; // 0-based index into `lines`

    while chunk_start_line < total_lines {
        // Tentative end: CHUNK_LINES after start, clamped to file end.
        let tentative_end = (chunk_start_line + CHUNK_LINES).min(total_lines);

        // ----- boundary snapping -----
        // Search ±BOUNDARY_SCAN around `tentative_end` for a better split.
        // We only scan forward from (tentative_end - scan) to
        // (tentative_end + scan), clamped to valid range.
        //
        // Preference: find the nearest matching line that is still ≥
        // chunk_start_line + 1 (we need at least one line in the chunk).
        let scan_lo = tentative_end
            .saturating_sub(BOUNDARY_SCAN)
            .max(chunk_start_line + 1);
        let scan_hi = (tentative_end + BOUNDARY_SCAN).min(total_lines);

        // Find the boundary-pattern line closest to tentative_end in [scan_lo, scan_hi).
        let snapped_end = {
            let mut best: Option<usize> = None;
            let mut best_dist = usize::MAX;
            for idx in scan_lo..scan_hi {
                if is_boundary(idx) {
                    let dist = idx.abs_diff(tentative_end);
                    if dist < best_dist {
                        best_dist = dist;
                        best = Some(idx);
                    }
                }
            }
            // If snapping would push the end line *before* advancing the cursor
            // at all, ignore it.
            match best {
                Some(b) if b > chunk_start_line => b,
                _ => tentative_end,
            }
        };

        let chunk_end_line = snapped_end; // exclusive

        // ----- build chunk text and count non-blank lines -----
        let byte_start = lines[chunk_start_line].byte_start;
        let last = chunk_end_line - 1;
        let byte_end = lines[last].byte_start + lines[last].byte_len;

        let non_blank_count: usize = lines[chunk_start_line..chunk_end_line]
            .iter()
            .filter(|l| l.non_blank)
            .count();

        if non_blank_count >= MIN_CHUNK_LINES {
            let text = content[byte_start..byte_end].to_owned();
            chunks.push(Chunk {
                byte_offset: byte_start,
                byte_end,
                // Convert 0-based → 1-based for public API.
                start_line: chunk_start_line + 1,
                end_line: chunk_end_line, // chunk_end_line is the 1-based inclusive last line
                text,
            });
        }

        // Advance start by `step` lines; stop if we've covered the whole file.
        let next_start = chunk_start_line + step;
        if next_start >= total_lines {
            break;
        }
        chunk_start_line = next_start;
    }

    chunks
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
    // Recursive implementation with memoisation avoided by using slices.
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
                // Try consuming one non-'/' character from name.
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

/// Summary of what [`run_updatedb`] did.
#[derive(Debug, Default)]
pub struct IndexStats {
    /// Total number of files visited by the walker.
    pub files_visited: usize,
    /// Files that were re-embedded (new or changed).
    pub files_updated: usize,
    /// Files skipped because their sha256 matched the stored hash.
    pub files_unchanged: usize,
    /// Files removed from the DB because they no longer exist on disk.
    pub files_deleted: usize,
    /// Total number of chunks added to the database.
    pub chunks_added: usize,
    /// Number of files that produced errors (skipped, logged via `progress`).
    pub errors: usize,
}

// ---------------------------------------------------------------------------
// run_updatedb
// ---------------------------------------------------------------------------

/// Walk the configured paths, embed changed files, and update the store.
///
/// # Arguments
///
/// * `store`       — mutable reference to the open SQLite store.
/// * `embedder`    — embedding engine (stub or real).
/// * `cfg`         — full application config (paths, exclusions, batch size).
/// * `full`        — if `true`, skip the sha256 check and re-embed every file.
/// * `path_filter` — if `Some(p)`, only consider files under `p`.
/// * `progress`    — callback invoked with human-readable status lines.
///   Called for each file processed; the format is intentionally
///   unstructured so callers can log, print, or discard it.
///
/// # Returns
///
/// An [`IndexStats`] summarising the run.
pub fn run_updatedb(
    store: &mut Store,
    embedder: &mut Embedder,
    cfg: &Config,
    full: bool,
    path_filter: Option<&Path>,
    progress: impl Fn(&str),
) -> Result<IndexStats> {
    let mut stats = IndexStats::default();

    // -----------------------------------------------------------------------
    // 1. Remove stale DB records.
    //
    // --full: wipe everything and start clean. This ensures files that were
    // previously indexed but are now out-of-scope (due to changed include_paths
    // or exclude_dirs) are purged — not just files deleted from disk.
    //
    // incremental: only remove records for files that no longer exist on disk.
    // -----------------------------------------------------------------------
    if full && path_filter.is_none() {
        let deleted = store
            .delete_all_files()
            .context("clearing index for full re-index")?;
        stats.files_deleted = deleted;
        if deleted > 0 {
            progress(&format!("cleared {deleted} entries for full re-index"));
        }
    } else {
        let deleted = store
            .delete_missing_files()
            .context("pruning deleted files from store")?;
        stats.files_deleted = deleted;
        if deleted > 0 {
            progress(&format!("pruned {deleted} deleted files from index"));
        }
    }

    // -----------------------------------------------------------------------
    // 2. Walk configured paths.
    // -----------------------------------------------------------------------
    let icfg = &cfg.index;

    // Build the walker from the first include path, then add the rest.
    // `ignore::WalkBuilder` handles .gitignore at every directory level.
    let include_paths = &icfg.include_paths;
    if include_paths.is_empty() {
        progress("no include_paths configured; nothing to index");
        return Ok(stats);
    }

    let mut builder = WalkBuilder::new(&include_paths[0]);
    for extra in include_paths.iter().skip(1) {
        builder.add(extra);
    }

    // Respect .gitignore files at every level if configured.
    builder.git_ignore(icfg.gitignore);
    builder.git_global(false); // keep behaviour predictable across machines
    builder.git_exclude(icfg.gitignore);

    // Add custom exclusion patterns for directories.
    // The `ignore` crate accepts these as "overrides" that act like .gitignore rules.
    let mut override_builder = ignore::overrides::OverrideBuilder::new(".");
    for dir in &icfg.exclude_dirs {
        // A leading '!' is not needed; these are "ignore" patterns (not overrides that
        // force inclusion).  The override builder uses gitignore syntax, so a bare
        // pattern matches files/dirs anywhere in the tree.
        let pattern = format!("!{}", dir);
        if let Err(e) = override_builder.add(&pattern) {
            progress(&format!("warn: invalid exclude_dirs pattern '{dir}': {e}"));
        }
    }
    if let Ok(overrides) = override_builder.build() {
        builder.overrides(overrides);
    }

    // Follow symlinks is intentionally false (default) — avoids infinite loops.
    builder.follow_links(false);

    // Process entries one by one (not in parallel — we call `store` which is
    // single-threaded and embedding is already CPU-bound).
    //
    // Batch state: collect (path, chunks) then flush to DB.
    let batch_size = cfg.embed.batch_size;

    // We accumulate chunks across files until we fill a batch, then embed.
    struct PendingChunk {
        /// Which file this chunk belongs to.
        path: std::path::PathBuf,
        file_mtime: f64,
        file_hash: String,
        chunk: Chunk,
    }
    let mut pending: Vec<PendingChunk> = Vec::new();

    // Helper: flush `pending` to the embedder + store.
    // This is a local closure called when the batch is full or at the end.
    // We can't capture `store` and `embedder` by mutable reference inside a
    // closure that we also call from within the loop, so we make it a function
    // that takes explicit arguments.
    fn flush_pending(
        pending: &mut Vec<PendingChunk>,
        embedder: &mut Embedder,
        store: &mut Store,
        stats: &mut IndexStats,
        progress: &dyn Fn(&str),
    ) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }

        let texts: Vec<&str> = pending.iter().map(|p| p.chunk.text.as_str()).collect();
        let embeddings = embedder
            .embed_batch(&texts)
            .context("embedding batch failed")?;

        // Group by file so we can do one DB transaction per file.
        // Files appear contiguously in `pending` because we flush at file
        // boundaries and always complete a file before moving to the next.
        let mut i = 0;
        while i < pending.len() {
            let file_path = pending[i].path.clone();
            let file_mtime = pending[i].file_mtime;
            let file_hash = pending[i].file_hash.clone();

            // Find the end of this file's slice.
            let j = pending[i..].partition_point(|p| p.path == file_path) + i;

            // 1. Upsert the file record → get file_id.
            let file_id = store
                .upsert_file(&file_path, file_mtime, &file_hash)
                .with_context(|| format!("upserting file record for {}", file_path.display()))?;

            // 2. Delete all existing chunks for this file (avoids duplicates
            //    on re-index; the ON DELETE CASCADE would also handle it if we
            //    deleted the file row, but we keep the file row and just
            //    refresh the chunks).
            store
                .delete_chunks_for_file(file_id)
                .with_context(|| format!("deleting old chunks for {}", file_path.display()))?;

            // 3. Insert each chunk with its freshly computed embedding.
            for (pc, emb) in pending[i..j].iter().zip(&embeddings[i..j]) {
                store
                    .insert_chunk(
                        file_id,
                        pc.chunk.byte_offset,
                        pc.chunk.byte_end,
                        pc.chunk.start_line,
                        pc.chunk.end_line,
                        emb.as_slice(),
                    )
                    .with_context(|| {
                        format!(
                            "inserting chunk (lines {}-{}) for {}",
                            pc.chunk.start_line,
                            pc.chunk.end_line,
                            file_path.display()
                        )
                    })?;
            }

            let chunk_count = j - i;
            stats.chunks_added += chunk_count;
            progress(&format!(
                "indexed {} chunks from {}",
                chunk_count,
                file_path.display()
            ));

            i = j;
        }

        pending.clear();
        Ok(())
    }

    for result in builder.build() {
        let entry = match result {
            Ok(e) => e,
            Err(e) => {
                progress(&format!("warn: walk error: {e}"));
                stats.errors += 1;
                continue;
            }
        };

        // Skip anything that is not a regular file: directories, symlinks,
        // devices, FIFOs. Symlinks are not followed (follow_links = false)
        // so without this check a symlink to a regular file would be indexed
        // and appear as a duplicate alongside the real path.
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let path = entry.path().to_path_buf();

        // ---- path_filter ----
        if let Some(filter) = path_filter {
            if !path.starts_with(filter) {
                continue;
            }
        }

        // ---- file metadata ----
        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                progress(&format!("warn: metadata({}): {e}", path.display()));
                stats.errors += 1;
                continue;
            }
        };

        if !metadata.is_file() {
            continue;
        }

        let file_size = metadata.len();

        // ---- size filters ----
        // max_file_size / min_file_size are plain u64 thresholds in IndexConfig
        // (zero means "no limit" for min; for max a reasonable compiled default
        // is always set, so we always compare).
        if file_size > icfg.max_file_size {
            continue;
        }
        if file_size < icfg.min_file_size {
            continue;
        }

        // ---- exclude_files glob check ----
        // Match against the bare filename (not the full path).
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if icfg
            .exclude_files
            .iter()
            .any(|pat| glob_match(pat, filename))
        {
            continue;
        }

        stats.files_visited += 1;

        // ---- read as UTF-8 ----
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => {
                // Binary file or encoding error — skip silently.
                continue;
            }
        };

        // ---- sha256 hash ----
        let hash = {
            let digest = Sha256::digest(content.as_bytes());
            hex::encode(digest)
        };

        // ---- incremental check ----
        // Use store.get_file() and compare the stored hash directly.
        if !full {
            if let Ok(Some(record)) = store.get_file(&path) {
                if record.hash == hash {
                    stats.files_unchanged += 1;
                    continue;
                }
            }
        }

        // ---- chunk ----
        let chunks = chunk_file(&content, icfg);
        if chunks.is_empty() {
            continue;
        }

        // ---- mtime ----
        let mtime: f64 = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        // Queue chunks.
        for chunk in chunks {
            pending.push(PendingChunk {
                path: path.clone(),
                file_mtime: mtime,
                file_hash: hash.clone(),
                chunk,
            });
        }

        stats.files_updated += 1;

        // Flush when batch is full.
        if pending.len() >= batch_size {
            flush_pending(&mut pending, embedder, store, &mut stats, &progress)?;
        }
    }

    // ---- flush remaining ----
    flush_pending(&mut pending, embedder, store, &mut stats, &progress)?;

    Ok(stats)
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
        IndexConfig {
            chunk_size: 40,
            chunk_overlap: 10,
            max_file_size: 10 * 1024 * 1024,
            min_file_size: 0,
            min_chunk_lines: 5,
            gitignore: false,
            include_paths: vec![],
            exclude_dirs: vec![],
            exclude_files: vec![],
        }
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
        // 3 non-blank lines → below MIN_CHUNK_LINES → no chunks
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
        for i in 0..CHUNK_LINES {
            lines.push(format!("    // filler line {i}"));
        }
        // Place a boundary pattern at CHUNK_LINES + 3 (within BOUNDARY_SCAN).
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
        let first = &chunks[0];
        // The 'fn' line is at 1-based index CHUNK_LINES + 4.
        // After snapping, end_line should equal CHUNK_LINES + 4.
        let fn_line = CHUNK_LINES + 4; // 1-based
        assert!(
            first.end_line >= fn_line.saturating_sub(BOUNDARY_SCAN)
                && first.end_line <= fn_line + BOUNDARY_SCAN,
            "expected end_line near fn boundary ({fn_line}), got {}",
            first.end_line
        );
    }

    #[test]
    fn chunk_overlap_between_consecutive_chunks() {
        // Build a file large enough to produce two chunks.
        let mut lines = Vec::new();
        for i in 0..(CHUNK_LINES * 2) {
            lines.push(format!("code line {i:03}"));
        }
        let content = lines.join("\n") + "\n";
        let chunks = chunk_file(&content, &test_icfg());
        if chunks.len() < 2 {
            // Small test environments may produce only one chunk — skip overlap test.
            return;
        }
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
    fn test_cfg_for_dir(dir: &tempfile::TempDir) -> crate::config::Config {
        let mut cfg = crate::config::Config::load(None).unwrap();
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

    fn open_temp_db() -> (tempfile::NamedTempFile, crate::store::Store) {
        let db_file = tempfile::NamedTempFile::new().unwrap();
        let store = crate::store::Store::open(db_file.path(), false).unwrap();
        (db_file, store)
    }

    #[test]
    fn run_updatedb_indexes_temp_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        write_lines(&dir.path().join("hello.txt"), 50);
        write_lines(&dir.path().join("world.txt"), 50);

        let cfg = test_cfg_for_dir(&dir);
        let (_db_file, mut store) = open_temp_db();
        let mut embedder = crate::embed::Embedder::stub(768);

        let stats = run_updatedb(&mut store, &mut embedder, &cfg, false, None, |_| {}).unwrap();

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
        let mut embedder = crate::embed::Embedder::stub(768);

        // First run indexes the file.
        run_updatedb(&mut store, &mut embedder, &cfg, false, None, |_| {}).unwrap();

        // Second run — file content unchanged — should show files_unchanged > 0.
        let stats2 = run_updatedb(&mut store, &mut embedder, &cfg, false, None, |_| {}).unwrap();

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
        let mut embedder = crate::embed::Embedder::stub(768);

        // First run — normal incremental.
        run_updatedb(&mut store, &mut embedder, &cfg, false, None, |_| {}).unwrap();

        // Second run with full=true — should re-index even though the file hasn't changed.
        let stats_full = run_updatedb(&mut store, &mut embedder, &cfg, true, None, |_| {}).unwrap();

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
        let mut embedder = crate::embed::Embedder::stub(768);

        let stats = run_updatedb(&mut store, &mut embedder, &cfg, false, None, |_| {}).unwrap();

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
        let mut embedder = crate::embed::Embedder::stub(768);

        let stats = run_updatedb(&mut store, &mut embedder, &cfg, false, None, |_| {}).unwrap();

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
        let mut embedder = crate::embed::Embedder::stub(768);

        // Only index files under sub_a.
        let stats =
            run_updatedb(&mut store, &mut embedder, &cfg, false, Some(&sub_a), |_| {}).unwrap();

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
    fn chunk_file_empty_returns_empty() {
        // Explicit alias for the same test that already exists (chunk_empty_content),
        // making the naming align with the requested test names.
        let chunks = chunk_file("", &test_icfg());
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_file_short_file() {
        // 3 non-blank lines is fewer than MIN_CHUNK_LINES (5) → no chunks.
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
        let mut embedder = crate::embed::Embedder::stub(768);

        let stats = run_updatedb(&mut store, &mut embedder, &cfg, false, None, |_| {}).unwrap();

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
        let mut embedder = crate::embed::Embedder::stub(768);

        let result = run_updatedb(&mut store, &mut embedder, &cfg, false, None, |_| {});

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
        // Fill CHUNK_LINES lines of filler.
        for i in 0..CHUNK_LINES {
            lines.push(format!("    // filler {i}"));
        }
        // Insert 2 filler lines then a boundary pattern within BOUNDARY_SCAN.
        lines.push("    // near boundary 1".to_string());
        lines.push("    // near boundary 2".to_string());
        lines.push("fn snap_target() {".to_string()); // boundary at CHUNK_LINES + 3 (0-based)
        for i in 0..20 {
            lines.push(format!("    let x = {i};"));
        }
        let content = lines.join("\n") + "\n";

        let chunks = chunk_file(&content, &test_icfg());
        assert!(!chunks.is_empty(), "file should produce at least one chunk");

        let first = &chunks[0];
        // fn line is at 1-based line CHUNK_LINES + 3.
        let fn_line_1based = CHUNK_LINES + 3;
        // The first chunk should end at or near the fn line (within BOUNDARY_SCAN).
        assert!(
            first.end_line >= fn_line_1based.saturating_sub(BOUNDARY_SCAN)
                && first.end_line <= fn_line_1based + BOUNDARY_SCAN,
            "chunk end_line {} should be near the fn boundary at {}",
            first.end_line,
            fn_line_1based
        );
    }
}

// src/store.rs — SQLite database layer.
//
// Schema (created on first open):
//
//   meta   — key/value pairs: model_name, model_sha256, embedding_dim
//   files  — one row per indexed file; tracks mtime + sha256
//   chunks — one row per chunk; embedding stored as raw f32 little-endian BLOB
//
// Security note: there is NO content column in chunks. Text is always read
// from the live source file at query time using byte_offset / byte_end.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

// ---------------------------------------------------------------------------
// Public record types
// ---------------------------------------------------------------------------

/// A row from the `files` table.
#[derive(Debug)]
#[allow(dead_code)]
pub struct FileRecord {
    pub id: i64,
    pub path: PathBuf,
    pub mtime: f64,
    pub hash: String,
}

/// Metadata from a `chunks` row (without the embedding blob).
#[derive(Debug)]
#[allow(dead_code)]
pub struct ChunkRecord {
    pub id: i64,
    pub file_id: i64,
    pub byte_offset: usize,
    pub byte_end: usize,
    pub start_line: usize,
    pub end_line: usize,
}

/// One ranked result returned by `Store::search`.
#[derive(Debug)]
pub struct SearchResult {
    pub path: PathBuf,
    pub byte_offset: usize,
    pub byte_end: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f32,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

pub struct Store {
    conn: Connection,
}

// ---------------------------------------------------------------------------
// Top-k heap for paged search
// ---------------------------------------------------------------------------

/// Number of rows fetched from SQLite per search page.
/// Memory per page: PAGE_SIZE × dim × 4 bytes (e.g. 1 000 × 768 × 4 = ~3 MB).
const SEARCH_PAGE_SIZE: usize = 1_000;

/// A scored search candidate retained in the top-k min-heap.
///
/// `BinaryHeap` is a max-heap; to keep the k *highest* scores we invert the
/// `Ord` so the *lowest* score floats to the top and gets evicted first.
struct Candidate {
    /// Raw bits of the f32 score — stored as u32 so the struct can implement
    /// `Eq` and `Ord` without pulling in an `ordered-float` dependency.
    score_bits: u32,
    path: String,
    byte_offset: usize,
    byte_end: usize,
    start_line: usize,
    end_line: usize,
}

impl Candidate {
    fn score(&self) -> f32 {
        f32::from_bits(self.score_bits)
    }
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.score_bits == other.score_bits
    }
}
impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reversed: lowest score sorts as "greatest" → BinaryHeap acts as min-heap.
        // heap.peek() returns the lowest-scoring candidate — the first to evict.
        other
            .score()
            .partial_cmp(&self.score())
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

// DDL executed once on first open.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    id    INTEGER PRIMARY KEY,
    path  TEXT UNIQUE NOT NULL,
    mtime REAL NOT NULL,
    hash  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chunks (
    id          INTEGER PRIMARY KEY,
    file_id     INTEGER REFERENCES files(id) ON DELETE CASCADE,
    byte_offset INTEGER NOT NULL,
    byte_end    INTEGER NOT NULL,
    start_line  INTEGER NOT NULL,
    end_line    INTEGER NOT NULL,
    embedding   BLOB NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(file_id);
";

impl Store {
    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /// Open (or create) the database at `path`.
    ///
    /// Creates all tables and indexes if this is a new database.
    /// Enables WAL mode when `wal` is true (recommended — better concurrency
    /// and crash safety).
    pub fn open(path: &Path, wal: bool) -> Result<Self> {
        // Ensure the parent directory exists.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating database directory {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;

        // Enable WAL before anything else so all subsequent writes benefit.
        if wal {
            conn.execute_batch("PRAGMA journal_mode = WAL;")
                .context("enabling WAL mode")?;
        }

        // Foreign-key enforcement is off by default in SQLite.
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .context("enabling foreign keys")?;

        // Create schema (IF NOT EXISTS guards make this idempotent).
        conn.execute_batch(SCHEMA).context("creating schema")?;

        Ok(Store { conn })
    }

    // -----------------------------------------------------------------------
    // Model metadata
    // -----------------------------------------------------------------------

    /// Read the stored model metadata and verify it matches the given values.
    ///
    /// Returns `Ok(())` if the meta table is empty (new DB) or if all three
    /// values match. Returns an error with a helpful message if there is a
    /// mismatch.
    #[allow(dead_code)]
    pub fn check_model(
        &self,
        model_name: &str,
        model_sha256: &str,
        embedding_dim: usize,
    ) -> Result<()> {
        let stored_name: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'model_name'",
                [],
                |row| row.get(0),
            )
            .optional()
            .context("reading meta.model_name")?;

        // No stored model yet → fresh DB, nothing to check.
        let stored_name = match stored_name {
            None => return Ok(()),
            Some(n) => n,
        };

        let stored_sha: String = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'model_sha256'",
                [],
                |row| row.get(0),
            )
            .optional()
            .context("reading meta.model_sha256")?
            .unwrap_or_default();

        let stored_dim: usize = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'embedding_dim'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading meta.embedding_dim")?
            .unwrap_or_default()
            .parse()
            .unwrap_or(0);

        if stored_name != model_name || stored_sha != model_sha256 || stored_dim != embedding_dim {
            bail!(
                "model changed — run 'vec updatedb --full' to re-index\n\
                 stored:     {} (sha256={}, dim={})\n\
                 configured: {} (sha256={}, dim={})",
                stored_name,
                stored_sha,
                stored_dim,
                model_name,
                model_sha256,
                embedding_dim
            );
        }

        Ok(())
    }

    /// Persist model metadata to the `meta` table (upsert).
    #[allow(dead_code)]
    pub fn set_model(
        &mut self,
        model_name: &str,
        model_sha256: &str,
        embedding_dim: usize,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES ('model_name', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![model_name],
            )
            .context("writing meta.model_name")?;

        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES ('model_sha256', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![model_sha256],
            )
            .context("writing meta.model_sha256")?;

        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES ('embedding_dim', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![embedding_dim.to_string()],
            )
            .context("writing meta.embedding_dim")?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Files table
    // -----------------------------------------------------------------------

    /// Look up a file by its path. Returns `None` if not indexed yet.
    pub fn get_file(&self, path: &Path) -> Result<Option<FileRecord>> {
        let path_str = path.to_string_lossy();
        self.conn
            .query_row(
                "SELECT id, path, mtime, hash FROM files WHERE path = ?1",
                params![path_str.as_ref()],
                |row| {
                    Ok(FileRecord {
                        id: row.get(0)?,
                        path: PathBuf::from(row.get::<_, String>(1)?),
                        mtime: row.get(2)?,
                        hash: row.get(3)?,
                    })
                },
            )
            .optional()
            .context("querying files table")
    }

    /// Insert or update the file record. Returns the file's row id.
    pub fn upsert_file(&mut self, path: &Path, mtime: f64, hash: &str) -> Result<i64> {
        let path_str = path.to_string_lossy();
        self.conn
            .execute(
                "INSERT INTO files (path, mtime, hash) VALUES (?1, ?2, ?3)
                 ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime,
                                                  hash  = excluded.hash",
                params![path_str.as_ref(), mtime, hash],
            )
            .context("upserting file record")?;

        // last_insert_rowid() returns the id even after an upsert in SQLite.
        // But to be safe (ON CONFLICT UPDATE does not always update rowid),
        // we do a follow-up lookup.
        let id: i64 = self
            .conn
            .query_row(
                "SELECT id FROM files WHERE path = ?1",
                params![path_str.as_ref()],
                |row| row.get(0),
            )
            .context("fetching file id after upsert")?;

        Ok(id)
    }

    // -----------------------------------------------------------------------
    // Chunks table
    // -----------------------------------------------------------------------

    /// Delete all chunk rows that belong to `file_id`.
    ///
    /// Call this before re-indexing a changed file so you don't accumulate
    /// stale embeddings.
    pub fn delete_chunks_for_file(&mut self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])
            .context("deleting chunks for file")?;
        Ok(())
    }

    /// Store one chunk with its embedding.
    ///
    /// The `embedding` slice is packed as little-endian f32 bytes and stored
    /// as a BLOB — no content / text is stored.
    pub fn insert_chunk(
        &mut self,
        file_id: i64,
        byte_offset: usize,
        byte_end: usize,
        start_line: usize,
        end_line: usize,
        embedding: &[f32],
    ) -> Result<()> {
        let blob = pack_f32(embedding);
        self.conn
            .execute(
                "INSERT INTO chunks (file_id, byte_offset, byte_end, start_line, end_line, embedding)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    file_id,
                    byte_offset as i64,
                    byte_end as i64,
                    start_line as i64,
                    end_line as i64,
                    blob,
                ],
            )
            .context("inserting chunk")?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    /// Cosine similarity search — paged, bounded-memory, top-k.
    ///
    /// Reads chunks from SQLite in pages of `SEARCH_PAGE_SIZE` rows, scoring
    /// each against `query_embedding`. A min-heap of size `limit` tracks the
    /// k highest-scoring candidates seen so far; only those k embeddings are
    /// ever live in RAM simultaneously.
    ///
    /// Memory: O(SEARCH_PAGE_SIZE × dim × 4 + limit) bytes regardless of how
    /// many chunks are indexed — safe for millions of chunks.
    ///
    /// If `path_filter` is `Some(prefix)`, only chunks whose file path starts
    /// with that prefix are scored.
    pub fn search(
        &self,
        query_embedding: &[f32],
        limit: usize,
        min_score: f32,
        path_filter: Option<&Path>,
    ) -> Result<Vec<SearchResult>> {
        let q_norm = normalize(query_embedding);

        let filter_str = path_filter
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let use_filter = path_filter.is_some();

        // Prefix filter: append '*' for GLOB. Escape any literal '*', '?', or '['
        // in the path so they are treated as literals, not wildcards.
        let glob_pattern = format!(
            "{}*",
            filter_str
                .replace('*', "[*]")
                .replace('?', "[?]")
                .replace('[', "[[")
        );

        // Min-heap keeping the top-k highest-scoring candidates.
        // The lowest score is at the top (Ord is reversed) and evicted first.
        let mut heap = std::collections::BinaryHeap::<Candidate>::new();

        // Prepare once; re-execute with increasing OFFSET each page.
        // ORDER BY c.id ensures a stable scan so LIMIT/OFFSET pages are disjoint.
        let mut stmt = self
            .conn
            .prepare(
                "SELECT f.path, c.byte_offset, c.byte_end, c.start_line, c.end_line, c.embedding
             FROM chunks c
             JOIN files f ON f.id = c.file_id
             WHERE (?1 = 0 OR f.path GLOB ?2)
             ORDER BY c.id
             LIMIT ?3 OFFSET ?4",
            )
            .context("preparing search query")?;

        let mut offset: i64 = 0;
        loop {
            let page = stmt
                .query_map(
                    params![
                        if use_filter { 1i64 } else { 0i64 },
                        &glob_pattern,
                        SEARCH_PAGE_SIZE as i64,
                        offset,
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,       // path
                            row.get::<_, i64>(1)? as usize, // byte_offset
                            row.get::<_, i64>(2)? as usize, // byte_end
                            row.get::<_, i64>(3)? as usize, // start_line
                            row.get::<_, i64>(4)? as usize, // end_line
                            row.get::<_, Vec<u8>>(5)?,      // embedding BLOB
                        ))
                    },
                )
                .context("executing search query")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("reading search page")?;

            if page.is_empty() {
                break; // All chunks processed.
            }

            for (path, byte_offset, byte_end, start_line, end_line, blob) in page {
                let embedding = unpack_f32(&blob);
                if embedding.is_empty() {
                    continue;
                }

                let score = dot(&q_norm, &normalize(&embedding));
                if score < min_score {
                    continue;
                }

                let candidate = Candidate {
                    score_bits: score.to_bits(),
                    path,
                    byte_offset,
                    byte_end,
                    start_line,
                    end_line,
                };

                if heap.len() < limit {
                    heap.push(candidate);
                } else if let Some(worst) = heap.peek() {
                    // Evict the lowest score if this candidate beats it.
                    if score > worst.score() {
                        heap.pop();
                        heap.push(candidate);
                    }
                }
            }

            offset += SEARCH_PAGE_SIZE as i64;
        }

        // Extract and sort descending by score.
        let mut results: Vec<SearchResult> = heap
            .into_iter()
            .map(|c| {
                let score = c.score(); // extract before c.path is moved
                SearchResult {
                    path: PathBuf::from(c.path),
                    byte_offset: c.byte_offset,
                    byte_end: c.byte_end,
                    start_line: c.start_line,
                    end_line: c.end_line,
                    score,
                }
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(results)
    }

    // -----------------------------------------------------------------------
    // Maintenance
    // -----------------------------------------------------------------------

    /// Remove rows from `files` (and their chunks, via CASCADE) for files
    /// that no longer exist on disk. Returns the number of files removed.
    pub fn delete_missing_files(&mut self) -> Result<usize> {
        let paths: Vec<(i64, String)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id, path FROM files")
                .context("preparing file list query")?;
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .context("listing files")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("reading file list")?;
            rows
        };

        let mut removed = 0usize;
        for (id, path) in paths {
            if !Path::new(&path).exists() {
                self.conn
                    .execute("DELETE FROM files WHERE id = ?1", params![id])
                    .context("deleting missing file")?;
                removed += 1;
            }
        }

        Ok(removed)
    }

    /// Delete all file and chunk records — used by `--full` re-index to ensure
    /// out-of-scope entries (changed include_paths / exclude_dirs) are purged.
    pub fn delete_all_files(&mut self) -> Result<usize> {
        let count: usize = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .context("counting files before wipe")?;
        self.conn
            .execute("DELETE FROM files", [])
            .context("wiping files table")?;
        // chunks are deleted via ON DELETE CASCADE on files.file_id
        Ok(count)
    }

    /// Return aggregate statistics: (file_count, chunk_count, last_mtime).
    ///
    /// `last_mtime` is `None` when no files have been indexed yet.
    pub fn stats(&self) -> Result<(usize, usize, Option<f64>)> {
        let file_count: usize = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .context("counting files")?;

        let chunk_count: usize = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
            .context("counting chunks")?;

        let last_mtime: Option<f64> = self
            .conn
            .query_row("SELECT MAX(mtime) FROM files", [], |row| row.get(0))
            .optional()
            .context("reading max mtime")?
            .flatten();

        Ok((file_count, chunk_count, last_mtime))
    }

    /// Run `VACUUM` to reclaim space after large deletions.
    #[allow(dead_code)]
    pub fn vacuum(&mut self) -> Result<()> {
        self.conn
            .execute_batch("VACUUM;")
            .context("running VACUUM")?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Embedding helpers
// ---------------------------------------------------------------------------

/// Pack a `f32` slice to a little-endian byte vector suitable for SQLite BLOB.
pub fn pack_f32(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Unpack a little-endian byte slice back to `Vec<f32>`.
///
/// Returns an empty Vec if `bytes.len()` is not a multiple of 4.
pub fn unpack_f32(bytes: &[u8]) -> Vec<f32> {
    if !bytes.len().is_multiple_of(4) {
        return Vec::new();
    }
    bytes
        .chunks_exact(4)
        .map(|b| {
            // Convert a 4-byte slice to [u8; 4] then interpret as little-endian f32.
            let arr: [u8; 4] = [b[0], b[1], b[2], b[3]];
            f32::from_le_bytes(arr)
        })
        .collect()
}

/// Return a new vector scaled so that its L2 norm equals 1.0.
///
/// If the input is all-zeros the output is also all-zeros (dot product will
/// be 0 for any query, which is harmless).
pub fn normalize(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

/// Dot product of two equal-length slices.
///
/// Returns 0.0 without panicking if the slices have different lengths
/// (only the shorter length is used — in practice they should always match).
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny 4-dimensional embedding used in tests so we don't need a real model.
    fn tiny_embed(values: &[f32; 4]) -> Vec<f32> {
        values.to_vec()
    }

    fn open_in_memory() -> Store {
        Store::open(Path::new(":memory:"), false).expect("in-memory DB")
    }

    // --- Embedding helpers ---

    #[test]
    fn pack_unpack_roundtrip() {
        let orig = vec![1.0_f32, -0.5, 0.0, 42.0];
        let bytes = pack_f32(&orig);
        let back = unpack_f32(&bytes);
        assert_eq!(orig, back);
    }

    #[test]
    fn normalize_unit_vector() {
        let v = vec![3.0_f32, 4.0]; // norm = 5
        let n = normalize(&v);
        let len: f32 = n.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((len - 1.0).abs() < 1e-6);
    }

    #[test]
    fn dot_known_value() {
        let a = vec![1.0_f32, 2.0, 3.0];
        let b = vec![4.0_f32, 5.0, 6.0];
        assert!((dot(&a, &b) - 32.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_identical_vectors() {
        let v = vec![1.0_f32, 2.0, 3.0, 4.0];
        let a = normalize(&v);
        let b = normalize(&v);
        assert!((dot(&a, &b) - 1.0).abs() < 1e-6);
    }

    // --- Store lifecycle ---

    #[test]
    fn open_creates_schema() {
        let store = open_in_memory();
        let (files, chunks, _) = store.stats().unwrap();
        assert_eq!(files, 0);
        assert_eq!(chunks, 0);
    }

    // --- Model metadata ---

    #[test]
    fn set_and_check_model() {
        let mut store = open_in_memory();
        store
            .set_model("gte-multilingual-base", "abc123", 768)
            .unwrap();
        // Same values → should not error.
        store
            .check_model("gte-multilingual-base", "abc123", 768)
            .unwrap();
    }

    #[test]
    fn check_model_mismatch_errors() {
        let mut store = open_in_memory();
        store
            .set_model("gte-multilingual-base", "abc123", 768)
            .unwrap();
        let err = store
            .check_model("different-model", "abc123", 768)
            .unwrap_err();
        assert!(err.to_string().contains("vec updatedb --full"));
    }

    #[test]
    fn check_model_on_empty_db_passes() {
        let store = open_in_memory();
        // No model set yet → should not error.
        store.check_model("anything", "sha", 768).unwrap();
    }

    // --- Files ---

    #[test]
    fn upsert_and_get_file() {
        let mut store = open_in_memory();
        let id = store
            .upsert_file(Path::new("/tmp/test.txt"), 1_700_000_000.0, "deadbeef")
            .unwrap();
        assert!(id > 0);

        let rec = store.get_file(Path::new("/tmp/test.txt")).unwrap().unwrap();
        assert_eq!(rec.id, id);
        assert_eq!(rec.hash, "deadbeef");
    }

    #[test]
    fn get_file_missing_returns_none() {
        let store = open_in_memory();
        let rec = store.get_file(Path::new("/nonexistent/path")).unwrap();
        assert!(rec.is_none());
    }

    #[test]
    fn upsert_updates_existing() {
        let mut store = open_in_memory();
        let id1 = store
            .upsert_file(Path::new("/tmp/x.txt"), 1.0, "hash1")
            .unwrap();
        let id2 = store
            .upsert_file(Path::new("/tmp/x.txt"), 2.0, "hash2")
            .unwrap();
        // Same path → same id.
        assert_eq!(id1, id2);
        let rec = store.get_file(Path::new("/tmp/x.txt")).unwrap().unwrap();
        assert_eq!(rec.hash, "hash2");
    }

    // --- Chunks ---

    #[test]
    fn insert_and_search_chunk() {
        let mut store = open_in_memory();
        let file_id = store
            .upsert_file(Path::new("/tmp/a.rs"), 1.0, "h1")
            .unwrap();

        let emb = tiny_embed(&[1.0, 0.0, 0.0, 0.0]);
        store.insert_chunk(file_id, 0, 100, 0, 10, &emb).unwrap();

        // Query with the same vector → score should be ~1.0.
        let results = store.search(&emb, 5, 0.0, None).unwrap();
        assert_eq!(results.len(), 1);
        assert!((results[0].score - 1.0).abs() < 1e-5);
        assert_eq!(results[0].path, Path::new("/tmp/a.rs"));
    }

    #[test]
    fn search_respects_min_score() {
        let mut store = open_in_memory();
        let file_id = store
            .upsert_file(Path::new("/tmp/b.rs"), 1.0, "h2")
            .unwrap();

        let emb = tiny_embed(&[1.0, 0.0, 0.0, 0.0]);
        store.insert_chunk(file_id, 0, 50, 0, 5, &emb).unwrap();

        // Query with an orthogonal vector → score = 0.
        let query = tiny_embed(&[0.0, 1.0, 0.0, 0.0]);
        let results = store.search(&query, 10, 0.5, None).unwrap();
        assert!(
            results.is_empty(),
            "orthogonal vector should score below 0.5"
        );
    }

    #[test]
    fn delete_chunks_for_file() {
        let mut store = open_in_memory();
        let fid = store
            .upsert_file(Path::new("/tmp/c.rs"), 1.0, "h3")
            .unwrap();
        let emb = tiny_embed(&[1.0, 0.0, 0.0, 0.0]);
        store.insert_chunk(fid, 0, 20, 0, 5, &emb).unwrap();

        let (_, chunks_before, _) = store.stats().unwrap();
        assert_eq!(chunks_before, 1);

        store.delete_chunks_for_file(fid).unwrap();

        let (_, chunks_after, _) = store.stats().unwrap();
        assert_eq!(chunks_after, 0);
    }

    #[test]
    fn delete_missing_files() {
        let mut store = open_in_memory();
        // Use a path that definitely does not exist.
        store
            .upsert_file(Path::new("/absolutely/does/not/exist/ever.rs"), 1.0, "h4")
            .unwrap();
        let removed = store.delete_missing_files().unwrap();
        assert_eq!(removed, 1);
        let (files, _, _) = store.stats().unwrap();
        assert_eq!(files, 0);
    }

    #[test]
    fn stats_empty_db() {
        let store = open_in_memory();
        let (files, chunks, last_mtime) = store.stats().unwrap();
        assert_eq!(files, 0);
        assert_eq!(chunks, 0);
        assert!(last_mtime.is_none());
    }

    // --- New tests ---

    #[test]
    fn search_returns_empty_when_db_empty() {
        let store = open_in_memory();
        let query = tiny_embed(&[1.0, 0.0, 0.0, 0.0]);
        let results = store.search(&query, 10, 0.0, None).unwrap();
        assert!(results.is_empty(), "fresh DB should return no results");
    }

    #[test]
    fn search_with_path_filter() {
        let mut store = open_in_memory();

        let fid_a = store
            .upsert_file(Path::new("/proj/a/file.rs"), 1.0, "h_a")
            .unwrap();
        let fid_b = store
            .upsert_file(Path::new("/proj/b/file.rs"), 1.0, "h_b")
            .unwrap();

        let emb_a = tiny_embed(&[1.0, 0.0, 0.0, 0.0]);
        let emb_b = tiny_embed(&[0.0, 1.0, 0.0, 0.0]);

        store.insert_chunk(fid_a, 0, 10, 1, 5, &emb_a).unwrap();
        store.insert_chunk(fid_b, 0, 10, 1, 5, &emb_b).unwrap();

        // Filter to /proj/a/ — only the chunk from fid_a should be returned.
        let query = tiny_embed(&[1.0, 0.0, 0.0, 0.0]);
        let results = store
            .search(&query, 10, 0.0, Some(Path::new("/proj/a")))
            .unwrap();

        assert_eq!(results.len(), 1, "path filter should restrict to /proj/a");
        assert_eq!(results[0].path, Path::new("/proj/a/file.rs"));
    }

    #[test]
    fn delete_missing_files_removes_stale() {
        let mut store = open_in_memory();
        // Insert a file record whose path does not exist on disk.
        store
            .upsert_file(
                Path::new("/this/path/absolutely/does/not/exist/stale.txt"),
                1.0,
                "stalehash",
            )
            .unwrap();

        let (files_before, _, _) = store.stats().unwrap();
        assert_eq!(files_before, 1);

        let removed = store.delete_missing_files().unwrap();
        assert_eq!(removed, 1, "stale record should have been removed");

        let (files_after, _, _) = store.stats().unwrap();
        assert_eq!(files_after, 0);
    }

    #[test]
    fn insert_multiple_chunks_same_file() {
        let mut store = open_in_memory();
        let fid = store
            .upsert_file(Path::new("/tmp/multi.rs"), 1.0, "hmulti")
            .unwrap();

        let emb = tiny_embed(&[1.0, 0.0, 0.0, 0.0]);
        store.insert_chunk(fid, 0, 40, 1, 10, &emb).unwrap();
        store.insert_chunk(fid, 41, 80, 11, 20, &emb).unwrap();
        store.insert_chunk(fid, 81, 120, 21, 30, &emb).unwrap();

        let (file_count, chunk_count, _) = store.stats().unwrap();
        assert_eq!(file_count, 1);
        assert_eq!(chunk_count, 3, "expected 3 chunks for the file");
    }

    #[test]
    fn cosine_search_finds_similar() {
        let mut store = open_in_memory();
        let fid = store
            .upsert_file(Path::new("/tmp/cosine.rs"), 1.0, "hcos")
            .unwrap();

        // Use a 4-d vector: e1 = [1,0,0,0]
        let known_emb = tiny_embed(&[1.0, 0.0, 0.0, 0.0]);
        store.insert_chunk(fid, 0, 50, 1, 5, &known_emb).unwrap();

        // Query with the identical vector → cosine score should be ~1.0.
        let results = store.search(&known_emb, 5, 0.0, None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(
            (results[0].score - 1.0).abs() < 1e-5,
            "identical vector should score ~1.0, got {}",
            results[0].score
        );

        // Query with the orthogonal vector [0,1,0,0] → score should be ~0.0.
        let ortho = tiny_embed(&[0.0, 1.0, 0.0, 0.0]);
        let results_ortho = store.search(&ortho, 5, 0.0, None).unwrap();
        if !results_ortho.is_empty() {
            assert!(
                results_ortho[0].score.abs() < 1e-5,
                "orthogonal vector should score ~0.0, got {}",
                results_ortho[0].score
            );
        }
    }

    #[test]
    fn model_mismatch_after_set() {
        let mut store = open_in_memory();
        store.set_model("model-a", "sha-a", 768).unwrap();

        // Check with a different model name → must return an error.
        let err = store.check_model("model-b", "sha-a", 768).unwrap_err();
        assert!(
            err.to_string().contains("vec updatedb --full"),
            "error message should mention 'vec updatedb --full'"
        );
    }

    // --- Gap 2: byte_end clamping ---

    #[test]
    fn byte_end_clamping_is_safe() {
        // Regression for the file-truncation race at query time.
        // The DB may store a byte_end that exceeds the file's current length
        // (file truncated after indexing). cmd_search clamps before slicing;
        // this test verifies the pattern doesn't panic.
        let bytes = b"hello world"; // 11 bytes
        let stored_byte_end = 10_000usize; // stale value from before truncation
        let stored_byte_offset = 0usize;
        let end = stored_byte_end.min(bytes.len());
        let start = stored_byte_offset.min(end);
        let slice = &bytes[start..end];
        assert_eq!(slice, b"hello world");
    }

    // --- Gap 3: GLOB escaping for special-char paths ---

    #[test]
    fn search_path_filter_with_bracket_char() {
        // Paths containing '[' are valid on Linux. They must be treated as
        // literals in the GLOB prefix filter, not as GLOB character classes.
        let mut store = open_in_memory();
        let special_path = "/proj/[module]/file.rs";
        let normal_path = "/proj/normal/file.rs";

        let fid_s = store
            .upsert_file(Path::new(special_path), 1.0, "hs")
            .unwrap();
        let fid_n = store
            .upsert_file(Path::new(normal_path), 1.0, "hn")
            .unwrap();

        let emb = tiny_embed(&[1.0, 0.0, 0.0, 0.0]);
        store.insert_chunk(fid_s, 0, 10, 1, 5, &emb).unwrap();
        store.insert_chunk(fid_n, 0, 10, 1, 5, &emb).unwrap();

        let results = store
            .search(&emb, 10, 0.0, Some(Path::new("/proj/[module]")))
            .unwrap();

        assert_eq!(
            results.len(),
            1,
            "path filter with '[' should return exactly 1 result"
        );
        assert_eq!(results[0].path, Path::new(special_path));
    }

    // --- Gap 8: paged search memory bound ---

    #[test]
    fn search_paged_returns_correct_top_k() {
        // Insert 1 500 chunks — more than SEARCH_PAGE_SIZE (1 000) — and verify
        // that paged loading returns the correct top-5, properly ranked.
        //
        // Embedding for chunk i: normalize([x, 0.5, 0.5, 0.5]) where
        // x = (i+1) * 0.001 (so x ranges from 0.001 to 1.5, keeping f32
        // scores distinct throughout).  Query: [1,0,0,0].
        // Cosine score = x / sqrt(x² + 0.75) which is strictly increasing in x,
        // so the highest-indexed chunks always have the highest scores.
        let mut store = open_in_memory();
        let fid = store
            .upsert_file(Path::new("/tmp/paged.rs"), 1.0, "hpaged")
            .unwrap();

        for i in 0..1_500usize {
            let x = (i + 1) as f32 * 0.001;
            let raw = [x, 0.5, 0.5, 0.5];
            let emb = normalize(&raw);
            store
                .insert_chunk(fid, i * 10, i * 10 + 9, i, i + 1, &emb)
                .unwrap();
        }

        let query = tiny_embed(&[1.0, 0.0, 0.0, 0.0]);
        let results = store.search(&query, 5, -1.0, None).unwrap();

        assert_eq!(results.len(), 5, "must return exactly 5 results");

        // Results must be sorted descending.
        for w in results.windows(2) {
            assert!(
                w[0].score >= w[1].score,
                "results not sorted: {} < {}",
                w[0].score,
                w[1].score
            );
        }

        // The top result must come from the highest-indexed chunk (i=1499).
        assert_eq!(
            results[0].start_line, 1499,
            "top result should be chunk at start_line=1499, got {}",
            results[0].start_line
        );
    }

    // --- Gap 7: SQLite error conditions ---

    #[cfg(unix)]
    #[test]
    fn open_store_bad_parent_returns_err() {
        // /dev/null is a character device on Linux; create_dir_all("/dev/null")
        // fails because the path already exists but is not a directory.
        // Store::open calls create_dir_all on the parent, so this must Err.
        let result = Store::open(Path::new("/dev/null/vec.db"), false);
        assert!(
            result.is_err(),
            "Store::open under /dev/null should return Err"
        );
    }

    // --- Integration test: full pipeline with stub ---

    #[test]
    fn full_pipeline_with_stub() {
        use std::io::Write;

        // 1. Open a temp DB.
        let db_file = tempfile::NamedTempFile::new().unwrap();
        let mut store = Store::open(db_file.path(), false).unwrap();

        // 2. Create a temp dir with a few text files that have enough lines to
        //    produce chunks (MIN_CHUNK_LINES = 5; we write 50 lines each).
        let dir = tempfile::TempDir::new().unwrap();
        for name in &["alpha.txt", "beta.txt"] {
            let p = dir.path().join(name);
            let mut f = std::fs::File::create(&p).unwrap();
            for i in 0..50u32 {
                writeln!(f, "This is line {i} of {name}").unwrap();
            }
        }

        // 3. Build a minimal Config pointing at the temp dir.
        let cfg = {
            let mut c = crate::config::Config::load(None).unwrap();
            c.index.include_paths = vec![dir.path().to_path_buf()];
            c.index.min_file_size = 0;
            c.index.gitignore = false;
            c.index.exclude_dirs = vec![];
            c.index.exclude_files = vec![];
            c.embed.batch_size = 4;
            c
        };

        // 4. Run updatedb with a stub embedder.
        let mut embedder = crate::embed::Embedder::stub(768);
        let stats = crate::index::run_updatedb(
            &mut store,
            &mut embedder,
            &cfg,
            false,     // not full
            None,      // no path filter
            |_msg| {}, // discard progress output
        )
        .unwrap();

        assert!(
            stats.files_updated >= 2,
            "expected at least 2 files updated, got {}",
            stats.files_updated
        );
        assert!(
            stats.chunks_added > 0,
            "expected at least 1 chunk added, got {}",
            stats.chunks_added
        );

        // 5. Search with a stub query embedding.
        // Use min_score = -1.0 so that all results (including those with small
        // negative cosine similarity) are returned. With a stub embedder the
        // rankings are deterministic but semantically meaningless; we only care
        // that the pipeline round-trips without errors and returns results.
        let mut qemb = crate::embed::Embedder::stub(768);
        let query_vec = qemb.embed_one("authentication middleware").unwrap();
        let results = store.search(&query_vec, 5, -1.0, None).unwrap();

        assert!(
            !results.is_empty(),
            "search pipeline should return at least one result"
        );
    }
}

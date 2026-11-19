//! vec-store — the SQLite index: files, chunks and their embeddings.
//!
//! Schema version 1 (kept in `PRAGMA user_version`; older databases are
//! upgraded in place when opened for writing):
//!
//! - `meta(key, value)`: the model identity (`model_name`, `embedding_dim`,
//!   `model_sha256`) and free-form keys (watcher status, ...).
//! - `files(id, path UNIQUE, mtime, size, hash)`: one row per indexed file.
//!   `size` is NULL for rows written before version 1.
//! - `chunks(id, file_id, byte_offset, byte_end, start_line, end_line, text_hash,
//!   embedding)`: one row per chunk. `embedding` is the L2-normalised vector as
//!   little-endian f32 (`dim × 4` bytes); `text_hash` identifies the chunk text so
//!   an unchanged chunk can reuse its vector (NULL for pre-version-1 rows).
//!
//! Invariants: every vector has the dimension of the recorded model identity
//! ([`Store::ensure_model`] must run before the first write), and one file's
//! record and chunks change together ([`Store::replace_file`] is one transaction).
//!
//! Security note: there is NO content column. Text is always read from the live
//! file at query time, through `byte_offset` / `byte_end`, after an access check.
//!
//! Requires SQLite 3.35 or later (`RETURNING`).

// The SQLite source is chosen at build time and must be explicit: a build
// that silently fell back to the other one would ship a different SQLite
// than the packager asked for.
#[cfg(all(feature = "bundled-sqlite", feature = "system-sqlite"))]
compile_error!(
    "vec-store: features `bundled-sqlite` and `system-sqlite` are mutually exclusive; \
     build with --no-default-features --features system-sqlite to link the system libsqlite3"
);
#[cfg(not(any(feature = "bundled-sqlite", feature = "system-sqlite")))]
compile_error!(
    "vec-store: select a SQLite source: feature `bundled-sqlite` (default) or `system-sqlite`"
);

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use vec_embed::ModelIdentity;

/// `true` only when `path` definitely does not exist. A permission or I/O
/// error is not proof of absence (`Path::exists` would return `false` and
/// a prune would delete a record that is still valid).
pub fn is_gone(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// How to open the database (values come from the `[database]` config section).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreOptions {
    /// Use write-ahead logging (with `synchronous = NORMAL`). Ignored read-only.
    pub wal: bool,
    /// How long to wait for a lock held by another connection (ms).
    pub busy_timeout_ms: u64,
    /// Open read-only: no directory or schema is created, nothing is migrated,
    /// and the database must already exist at the current schema version.
    pub read_only: bool,
    /// Read-write: how long to keep retrying for the lock of a schema upgrade
    /// that another process holds (ms), e.g. while it upgrades the same index.
    pub migration_wait_ms: u64,
}

impl StoreOptions {
    /// The options of the `[database]` config section.
    #[must_use]
    pub fn from_config(db: &vec_core::config::DatabaseConfig, read_only: bool) -> Self {
        Self {
            wal: db.wal,
            busy_timeout_ms: db.busy_timeout_ms,
            read_only,
            migration_wait_ms: db.migration_wait_ms,
        }
    }
}

/// The index was built with another model than the one given. Returned
/// inside the `anyhow::Error` of [`Store::check_model`],
/// [`Store::ensure_model`] and [`Store::replace_file`]; find it with
/// `err.downcast_ref::<ModelMismatch>()`. Any other error of these methods
/// (a busy database, an I/O error) is not a mismatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelMismatch {
    /// The index records another model identity.
    Recorded {
        /// The database file.
        db_path: PathBuf,
        /// The identity recorded in the index.
        stored: ModelIdentity,
        /// The identity given by the caller.
        expected: ModelIdentity,
    },
    /// The index records no identity (written before identities were
    /// recorded) and its vectors have another dimension.
    Unrecorded {
        /// The database file.
        db_path: PathBuf,
        /// Dimension of the stored vectors.
        stored_dim: usize,
        /// The identity given by the caller.
        expected: ModelIdentity,
    },
}

impl std::fmt::Display for ModelMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Recorded {
                db_path,
                stored,
                expected,
            } => write!(
                f,
                "model mismatch: the index at {} was built with {stored}, but the configured \
                 model is {expected}. Restore the configured model, or rebuild the index for \
                 the new one with `vec updatedb --full`.",
                db_path.display()
            ),
            Self::Unrecorded {
                db_path,
                stored_dim,
                expected,
            } => write!(
                f,
                "the index at {} holds vectors of dimension {stored_dim} written before model \
                 identities were recorded, but the configured model {expected} has dimension \
                 {}. Rebuild it with `vec updatedb --full`.",
                db_path.display(),
                expected.dim
            ),
        }
    }
}

impl std::error::Error for ModelMismatch {}

/// A row from the `files` table.
#[derive(Debug, Clone, PartialEq)]
pub struct FileRecord {
    pub id: i64,
    pub path: PathBuf,
    pub mtime: f64,
    /// File size in bytes; `None` for records written before schema version 1.
    pub size: Option<u64>,
    pub hash: String,
}

/// Metadata of a `chunks` row (without the embedding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRecord {
    pub id: i64,
    pub file_id: i64,
    pub byte_offset: usize,
    pub byte_end: usize,
    pub start_line: usize,
    pub end_line: usize,
}

/// A chunk to write with [`Store::replace_file`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewChunk {
    pub byte_offset: usize,
    pub byte_end: usize,
    pub start_line: usize,
    pub end_line: usize,
    /// Hash of the chunk text, for reusing its vector when the text is unchanged.
    pub text_hash: String,
    /// The vector; stored L2-normalised. Its length must be the model dimension.
    pub embedding: Vec<f32>,
}

/// One ranked result returned by [`Store::search`].
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub path: PathBuf,
    pub byte_offset: usize,
    pub byte_end: usize,
    pub start_line: usize,
    pub end_line: usize,
    /// Cosine similarity to the query.
    pub score: f32,
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// The schema version this code reads and writes.
pub const SCHEMA_VERSION: i64 = 1;

/// Version-1 tables. `IF NOT EXISTS` leaves a version-0 database's tables in
/// place; [`migrate_to_v1`] then adds the missing columns.
const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    id    INTEGER PRIMARY KEY,
    path  TEXT UNIQUE NOT NULL,
    mtime REAL NOT NULL,
    size  INTEGER,
    hash  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chunks (
    id          INTEGER PRIMARY KEY,
    file_id     INTEGER REFERENCES files(id) ON DELETE CASCADE,
    byte_offset INTEGER NOT NULL,
    byte_end    INTEGER NOT NULL,
    start_line  INTEGER NOT NULL,
    end_line    INTEGER NOT NULL,
    text_hash   TEXT,
    embedding   BLOB NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(file_id);
";

const META_MODEL_NAME: &str = "model_name";
const META_EMBEDDING_DIM: &str = "embedding_dim";
const META_MODEL_SHA256: &str = "model_sha256";

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// The open index database.
pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /// Open the database at `path`.
    ///
    /// Read-write: creates the parent directory and the database if needed and
    /// upgrades an older schema in place (the write lock is taken only when an
    /// upgrade is due, so an up-to-date index stays usable by users who cannot
    /// write it). Read-only: the database must exist at the current version.
    pub fn open(path: &Path, opts: &StoreOptions) -> Result<Self> {
        let conn = if opts.read_only {
            open_read_only(path, opts)?
        } else {
            open_read_write(path, opts)?
        };
        Ok(Store {
            conn,
            path: path.to_path_buf(),
        })
    }

    // -----------------------------------------------------------------------
    // Model identity
    // -----------------------------------------------------------------------

    /// The model identity recorded in the index, if any.
    pub fn model(&self) -> Result<Option<ModelIdentity>> {
        read_model(&self.conn)
    }

    /// Check that vectors of `id` may be compared with the index (readers).
    /// An index without a recorded identity accepts any model.
    pub fn check_model(&self, id: &ModelIdentity) -> Result<()> {
        match read_model(&self.conn)? {
            Some(stored) if !stored.is_compatible_with(id) => {
                Err(mismatch(&self.path, &stored, id))
            }
            _ => Ok(()),
        }
    }

    /// Record `id` in an index that has none, or check it against the recorded
    /// one (writers call this before writing). Name and dimension must match;
    /// the sha256 must match when both are known, and an unknown stored sha256
    /// is filled in.
    ///
    /// An index written before identities were recorded is adopted when its
    /// vectors have `id`'s dimension, and refused otherwise.
    pub fn ensure_model(&mut self, id: &ModelIdentity) -> Result<()> {
        if id.dim == 0 {
            bail!("model {id} has dimension 0");
        }
        let tx = write_tx(&mut self.conn)?;
        match read_model(&tx)? {
            None => {
                let stored_len: Option<usize> = tx
                    .query_row("SELECT length(embedding) FROM chunks LIMIT 1", [], |r| {
                        r.get(0)
                    })
                    .optional()
                    .context("reading a stored vector size")?;
                if let Some(len) = stored_len
                    && len != id.dim * 4
                {
                    return Err(ModelMismatch::Unrecorded {
                        db_path: self.path.clone(),
                        stored_dim: len / 4,
                        expected: id.clone(),
                    }
                    .into());
                }
                write_model(&tx, id)?;
            }
            Some(stored) => {
                if !stored.is_compatible_with(id) {
                    return Err(mismatch(&self.path, &stored, id));
                }
                if stored.sha256.is_none()
                    && let Some(sha) = &id.sha256
                {
                    set_meta(&tx, META_MODEL_SHA256, sha)?;
                }
            }
        }
        tx.commit().context("committing the model identity")
    }

    /// Empty the index and record `id` as its model, in one transaction (for a
    /// full rebuild with a different model).
    pub fn reset_model(&mut self, id: &ModelIdentity) -> Result<()> {
        if id.dim == 0 {
            bail!("model {id} has dimension 0");
        }
        let tx = write_tx(&mut self.conn)?;
        tx.execute_batch("DELETE FROM chunks; DELETE FROM files;")
            .context("emptying the index")?;
        write_model(&tx, id)?;
        tx.commit().context("committing the model reset")
    }

    // -----------------------------------------------------------------------
    // Files and chunks
    // -----------------------------------------------------------------------

    /// Look up a file by path. `None` if it is not indexed (a non-UTF-8 path
    /// never is).
    pub fn get_file(&self, path: &Path) -> Result<Option<FileRecord>> {
        let Some(path_str) = path.to_str() else {
            return Ok(None);
        };
        self.conn
            .query_row(
                "SELECT id, path, mtime, size, hash FROM files WHERE path = ?1",
                params![path_str],
                |row| {
                    Ok(FileRecord {
                        id: row.get(0)?,
                        path: PathBuf::from(row.get::<_, String>(1)?),
                        mtime: row.get(2)?,
                        size: row.get(3)?,
                        hash: row.get(4)?,
                    })
                },
            )
            .optional()
            .context("querying files table")
    }

    /// Replace everything stored for `path` (its record and all its chunks) in
    /// one transaction: readers see the old or the new state, never a mix, and
    /// a failure leaves the old state. Vectors are validated against the model
    /// dimension and stored normalised.
    ///
    /// `model` is the identity of the model that computed the vectors. It is
    /// checked against the recorded identity inside the transaction, so a
    /// writer that checked the model earlier cannot slip vectors of the old
    /// model into an index another process has meanwhile reset for a new one
    /// ([`Store::reset_model`]). Requires a recorded model identity
    /// ([`Store::ensure_model`]).
    ///
    /// # Errors
    ///
    /// [`ModelMismatch`] when the index records another model; an error for a
    /// non-UTF-8 path, a vector of the wrong dimension or with non-finite
    /// values, or a database error.
    pub fn replace_file(
        &mut self,
        model: &ModelIdentity,
        path: &Path,
        mtime: f64,
        size: u64,
        hash: &str,
        chunks: &[NewChunk],
    ) -> Result<()> {
        let path_str = utf8_path(path)?;
        let tx = write_tx(&mut self.conn)?;
        let stored = read_model(&tx)?.ok_or_else(|| {
            anyhow!("the index records no model identity; call Store::ensure_model before writing")
        })?;
        if !stored.is_compatible_with(model) {
            return Err(mismatch(&self.path, &stored, model));
        }
        let model = stored;
        let file_id: i64 = tx
            .query_row(
                "INSERT INTO files (path, mtime, size, hash) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime,
                                                  size  = excluded.size,
                                                  hash  = excluded.hash
                 RETURNING id",
                params![path_str, mtime, size, hash],
                |r| r.get(0),
            )
            .with_context(|| format!("writing the file record of {path_str}"))?;
        tx.execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])
            .with_context(|| format!("deleting the old chunks of {path_str}"))?;
        {
            let mut insert = tx
                .prepare_cached(
                    "INSERT INTO chunks
                       (file_id, byte_offset, byte_end, start_line, end_line, text_hash, embedding)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .context("preparing chunk insert")?;
            for c in chunks {
                let lines = || format!("lines {}-{} of {path_str}", c.start_line, c.end_line);
                let blob = normalized_blob(&c.embedding, model.dim).with_context(lines)?;
                insert
                    .execute(params![
                        file_id,
                        c.byte_offset,
                        c.byte_end,
                        c.start_line,
                        c.end_line,
                        c.text_hash,
                        blob
                    ])
                    .with_context(|| format!("inserting chunk at {}", lines()))?;
            }
        }
        tx.commit()
            .with_context(|| format!("committing {path_str}"))
    }

    /// The stored vectors of `path`'s chunks, keyed by chunk text hash (chunks
    /// written before schema version 1 have no hash and are left out).
    pub fn embeddings_by_text_hash(&self, path: &Path) -> Result<HashMap<String, Vec<f32>>> {
        let Some(path_str) = path.to_str() else {
            return Ok(HashMap::new());
        };
        let mut stmt = self
            .conn
            .prepare(
                "SELECT c.text_hash, c.embedding FROM chunks c JOIN files f ON f.id = c.file_id
                 WHERE f.path = ?1 AND c.text_hash IS NOT NULL",
            )
            .context("preparing embedding lookup")?;
        let mut rows = stmt
            .query(params![path_str])
            .context("looking up embeddings")?;
        let mut out = HashMap::new();
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            let vector = vector_from_blob(row.get_ref(1)?.as_blob()?)?;
            out.insert(hash, vector);
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    /// Top-`limit` chunks by cosine similarity to `query_embedding`, best first.
    ///
    /// The scan streams over one read snapshot (consistent while `vec watch`
    /// writes) and keeps only the `limit` best candidates in memory. Results
    /// below `min_score`, and non-finite scores, are dropped.
    ///
    /// `path_filter` keeps files equal to that path or inside that directory
    /// (`/a` matches `/a/x` but not `/abc`); it is an index range scan, and
    /// `[`, `*`, `?` have no special meaning.
    ///
    /// Errors when the query's dimension differs from the index's, or when a
    /// stored vector has the wrong size.
    pub fn search(
        &self,
        query_embedding: &[f32],
        limit: usize,
        min_score: f32,
        path_filter: Option<&Path>,
    ) -> Result<Vec<SearchResult>> {
        self.search_filtered(query_embedding, limit, min_score, path_filter, |_| true)
    }

    /// Like [`Store::search`], keeping only results for which `keep` returns
    /// `true`; the filter runs before the limit, so up to `limit` kept results
    /// are returned. `keep` is called only for a result that would enter the
    /// current top `limit` (so a costly check such as an access() call runs for
    /// few rows).
    pub fn search_filtered(
        &self,
        query_embedding: &[f32],
        limit: usize,
        min_score: f32,
        path_filter: Option<&Path>,
        mut keep: impl FnMut(&SearchResult) -> bool,
    ) -> Result<Vec<SearchResult>> {
        let tx = self
            .conn
            .unchecked_transaction()
            .context("starting the search snapshot")?;
        let model = read_model(&tx)?;
        let dim = model.as_ref().map_or(query_embedding.len(), |m| m.dim);
        if query_embedding.len() != dim {
            bail!(
                "the query vector has dimension {} but the index at {} holds {} vectors; \
                 query with the model the index was built with, or rebuild the index \
                 (`vec updatedb --full`)",
                query_embedding.len(),
                self.path.display(),
                model.map_or_else(|| format!("dimension-{dim}"), |m| m.to_string()),
            );
        }
        let query = normalize(query_embedding);
        if query.iter().any(|x| !x.is_finite()) {
            bail!("the query vector contains non-finite values");
        }
        if limit == 0 {
            return Ok(Vec::new());
        }

        const COLUMNS: &str = "SELECT f.path, c.byte_offset, c.byte_end, c.start_line, \
                               c.end_line, c.embedding FROM files f JOIN chunks c ON c.file_id = f.id";
        let bounds = path_filter.map(scope_bounds).transpose()?;
        let mut stmt;
        let mut rows = match &bounds {
            Some((base, lo, hi)) => {
                stmt = tx
                    .prepare(&format!(
                        "{COLUMNS} WHERE f.path = ?1 OR (f.path >= ?2 AND f.path < ?3)"
                    ))
                    .context("preparing search query")?;
                stmt.query(params![base, lo, hi])
            }
            None => {
                stmt = tx.prepare(COLUMNS).context("preparing search query")?;
                stmt.query([])
            }
        }
        .context("executing search query")?;

        // Grows with the results kept (a huge `limit` must not pre-allocate).
        let mut heap: BinaryHeap<Reverse<Candidate>> = BinaryHeap::new();
        while let Some(row) = rows.next().context("reading search rows")? {
            let blob = row.get_ref(5)?.as_blob()?;
            if blob.len() != dim * 4 {
                bail!(
                    "the index at {} holds a vector of {} bytes where {} were expected \
                     (dimension {dim}); rebuild it with `vec updatedb --full`",
                    self.path.display(),
                    blob.len(),
                    dim * 4
                );
            }
            let score = dot_blob(&query, blob);
            if !score.is_finite() || score < min_score {
                continue;
            }
            if heap.len() == limit && heap.peek().is_some_and(|Reverse(w)| score <= w.score) {
                continue;
            }
            let result = SearchResult {
                path: PathBuf::from(row.get_ref(0)?.as_str()?),
                byte_offset: row.get(1)?,
                byte_end: row.get(2)?,
                start_line: row.get(3)?,
                end_line: row.get(4)?,
                score,
            };
            if !keep(&result) {
                continue;
            }
            heap.push(Reverse(Candidate { score, result }));
            if heap.len() > limit {
                heap.pop();
            }
        }
        // Ascending order of Reverse = descending score.
        Ok(heap
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse(c)| c.result)
            .collect())
    }

    // -----------------------------------------------------------------------
    // Maintenance
    // -----------------------------------------------------------------------

    /// Call `f` with the path of every indexed file, in path order, from one
    /// read snapshot (constant memory). Read-only.
    pub fn for_each_indexed_path(&self, mut f: impl FnMut(&Path)) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM files ORDER BY path")
            .context("preparing file list query")?;
        let mut rows = stmt.query([]).context("listing files")?;
        while let Some(row) = rows.next().context("reading file list")? {
            f(Path::new(row.get_ref(0)?.as_str()?));
        }
        Ok(())
    }

    /// The schema version of the open database (`PRAGMA user_version`).
    pub fn schema_version(&self) -> Result<i64> {
        user_version(&self.conn)
    }

    /// Remove the records (and, by cascade, the chunks) of files that no
    /// longer exist. Returns the number removed.
    pub fn delete_missing_files(&mut self) -> Result<usize> {
        self.delete_files_where(None, is_gone)
    }

    /// Delete every file record (and, by cascade, its chunks) for which
    /// `stale(path)` is true, in one transaction. Returns the number deleted.
    ///
    /// `scopes = None` considers every record. `Some(scopes)` considers only
    /// records equal to or under one of them, with an index range scan, so
    /// the cost is proportional to the records in scope.
    pub fn delete_files_where(
        &mut self,
        scopes: Option<&[PathBuf]>,
        mut stale: impl FnMut(&Path) -> bool,
    ) -> Result<usize> {
        let mut rows: Vec<(i64, String)> = Vec::new();
        match scopes {
            None => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT id, path FROM files")
                    .context("preparing file list query")?;
                rows = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                    .context("listing files")?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .context("reading file list")?;
            }
            Some(scopes) => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT id, path FROM files WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
                    )
                    .context("preparing scoped file list query")?;
                for scope in scopes {
                    let (base, lo, hi) = scope_bounds(scope)?;
                    let found = stmt
                        .query_map(params![base, lo, hi], |row| Ok((row.get(0)?, row.get(1)?)))
                        .context("listing scoped files")?
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .context("reading scoped file list")?;
                    rows.extend(found);
                }
                rows.sort_unstable();
                rows.dedup();
            }
        }

        let tx = write_tx(&mut self.conn)?;
        let mut removed = 0usize;
        {
            let mut del = tx
                .prepare("DELETE FROM files WHERE id = ?1")
                .context("preparing delete")?;
            for (id, path) in rows {
                if stale(Path::new(&path)) {
                    del.execute(params![id]).context("deleting stale file")?;
                    removed += 1;
                }
            }
        }
        tx.commit().context("committing prune transaction")?;
        Ok(removed)
    }

    /// Store a key/value pair in the `meta` table (upsert).
    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        set_meta(&self.conn, key, value)
    }

    /// Read a value from the `meta` table.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        get_meta(&self.conn, key)
    }

    /// Aggregate statistics: (file count, chunk count, newest file mtime).
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
            .context("reading max mtime")?;
        Ok((file_count, chunk_count, last_mtime))
    }

    /// The stored (normalised) vector of the first chunk that covers `line` in
    /// `path`, or `None`.
    pub fn get_chunk_embedding_at(&self, path: &Path, line: usize) -> Result<Option<Vec<f32>>> {
        let Some(path_str) = path.to_str() else {
            return Ok(None);
        };
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT c.embedding FROM chunks c JOIN files f ON c.file_id = f.id
                 WHERE f.path = ?1 AND c.start_line <= ?2 AND c.end_line >= ?2
                 ORDER BY c.id LIMIT 1",
                params![path_str, line],
                |row| row.get(0),
            )
            .optional()
            .context("looking up chunk embedding")?;
        blob.map(|b| vector_from_blob(&b)).transpose()
    }

    /// All chunks that cover `line` in `path`.
    pub fn get_chunks_covering(&self, path: &Path, line: usize) -> Result<Vec<ChunkRecord>> {
        let Some(path_str) = path.to_str() else {
            return Ok(Vec::new());
        };
        let mut stmt = self
            .conn
            .prepare(
                "SELECT c.id, c.file_id, c.byte_offset, c.byte_end, c.start_line, c.end_line
                 FROM chunks c JOIN files f ON c.file_id = f.id
                 WHERE f.path = ?1 AND c.start_line <= ?2 AND c.end_line >= ?2
                 ORDER BY c.id",
            )
            .context("preparing chunks query")?;
        stmt.query_map(params![path_str, line], |row| {
            Ok(ChunkRecord {
                id: row.get(0)?,
                file_id: row.get(1)?,
                byte_offset: row.get(2)?,
                byte_end: row.get(3)?,
                start_line: row.get(4)?,
                end_line: row.get(5)?,
            })
        })
        .context("querying chunks")?
        .collect::<rusqlite::Result<_>>()
        .context("reading chunks")
    }

    /// Run `VACUUM` to reclaim space after large deletions.
    pub fn vacuum(&mut self) -> Result<()> {
        self.conn.execute_batch("VACUUM;").context("running VACUUM")
    }
}

/// A write transaction that takes the write lock at once (so a busy database
/// waits for `busy_timeout` instead of failing on lock upgrade).
fn write_tx(conn: &mut Connection) -> Result<Transaction<'_>> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
        .context("starting a write transaction")
}

fn mismatch(path: &Path, stored: &ModelIdentity, configured: &ModelIdentity) -> anyhow::Error {
    ModelMismatch::Recorded {
        db_path: path.to_path_buf(),
        stored: stored.clone(),
        expected: configured.clone(),
    }
    .into()
}

// ---------------------------------------------------------------------------
// Opening and migrations
// ---------------------------------------------------------------------------

fn open_read_only(path: &Path, opts: &StoreOptions) -> Result<Connection> {
    let exists = path
        .try_exists()
        .with_context(|| format!("checking for the index at {}", path.display()))?;
    if !exists {
        bail!(
            "no index at {}: build it with `vec updatedb`",
            path.display()
        );
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening the index {} read-only", path.display()))?;
    conn.busy_timeout(Duration::from_millis(opts.busy_timeout_ms))
        .context("setting busy timeout")?;
    let version = user_version(&conn)?;
    match version.cmp(&SCHEMA_VERSION) {
        Ordering::Equal => Ok(conn),
        Ordering::Less => bail!(
            "the index at {} uses schema version {version}, this vec needs version \
             {SCHEMA_VERSION}; run `vec updatedb` (as the index owner) to upgrade it in place \
             (if an upgrade is running right now, retry once it has finished)",
            path.display()
        ),
        Ordering::Greater => Err(newer_schema(path, version)),
    }
}

fn open_read_write(path: &Path, opts: &StoreOptions) -> Result<Connection> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating database directory {}", parent.display()))?;
    }
    let mut conn =
        Connection::open(path).with_context(|| format!("opening database {}", path.display()))?;
    conn.busy_timeout(Duration::from_millis(opts.busy_timeout_ms))
        .context("setting busy timeout")?;
    if opts.wal {
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enabling WAL mode")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("setting synchronous = NORMAL")?;
    }
    conn.pragma_update(None, "foreign_keys", true)
        .context("enabling foreign keys")?;
    migrate(
        &mut conn,
        path,
        Duration::from_millis(opts.migration_wait_ms),
    )?;
    Ok(conn)
}

fn user_version(conn: &Connection) -> Result<i64> {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
        .context("reading the schema version")
}

fn newer_schema(path: &Path, version: i64) -> anyhow::Error {
    anyhow!(
        "the index at {} uses schema version {version}, newer than this vec supports \
         ({SCHEMA_VERSION}); upgrade vec",
        path.display()
    )
}

/// Bring the schema to [`SCHEMA_VERSION`]. Checks without a lock first, so an
/// up-to-date database is never written (or locked) by opening it.
///
/// The upgrade takes the write lock. While another process holds it (most
/// likely upgrading the same index, which rewrites every vector), each
/// attempt waits `busy_timeout`; attempts are repeated until `wait` has
/// passed, with one message on stderr saying why this process waits.
fn migrate(conn: &mut Connection, path: &Path, wait: Duration) -> Result<()> {
    if user_version(conn)? == SCHEMA_VERSION {
        return Ok(());
    }
    let started = Instant::now();
    let mut told = false;
    let tx = loop {
        match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
            Ok(tx) => break tx,
            Err(e) if is_busy(&e) && started.elapsed() < wait => {
                if !told {
                    anstream::eprintln!(
                        "vec: the index at {} needs a schema upgrade and another process holds \
                         its lock (probably upgrading it); waiting up to {} s \
                         ([database] migration_wait_ms)",
                        path.display(),
                        wait.as_secs()
                    );
                    told = true;
                }
            }
            Err(e) if is_busy(&e) => {
                return Err(e).with_context(|| {
                    format!(
                        "the index at {} needs a schema upgrade, but another process held its \
                         lock for more than [database] migration_wait_ms ({} ms); run \
                         `vec updatedb` once when it is idle",
                        path.display(),
                        wait.as_millis()
                    )
                });
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("locking {} to upgrade its schema", path.display()));
            }
        }
    };
    let version = user_version(&tx)?;
    if version > SCHEMA_VERSION {
        return Err(newer_schema(path, version));
    }
    if version < 1 {
        migrate_to_v1(&tx).context("upgrading the index schema to version 1")?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)
        .context("writing the schema version")?;
    tx.commit().context("committing the schema upgrade")
}

/// `true` for "database is locked" (`SQLITE_BUSY`, after `busy_timeout`).
fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::DatabaseBusy)
}

/// Version 0 (no `user_version`) → 1: create missing tables, add `files.size`
/// and `chunks.text_hash`, and normalise stored vectors (version 1 stores them
/// normalised and scores without re-normalising).
fn migrate_to_v1(tx: &Transaction) -> Result<()> {
    tx.execute_batch(SCHEMA_V1).context("creating tables")?;
    add_column_if_missing(tx, "files", "size", "INTEGER")?;
    add_column_if_missing(tx, "chunks", "text_hash", "TEXT")?;

    // One row at a time by rowid: constant memory, and no row is updated
    // while a statement is still reading it.
    let mut next =
        tx.prepare("SELECT id, embedding FROM chunks WHERE id > ?1 ORDER BY id LIMIT 1")?;
    let mut update = tx.prepare("UPDATE chunks SET embedding = ?1 WHERE id = ?2")?;
    let mut last = i64::MIN;
    while let Some((id, blob)) = next
        .query_row(params![last], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .optional()?
    {
        last = id;
        if !blob.len().is_multiple_of(4) {
            continue; // Reported by search as an inconsistent row.
        }
        let normalised = pack_f32(&normalize(&unpack_f32(&blob)));
        if normalised != blob {
            update.execute(params![normalised, id])?;
        }
    }
    Ok(())
}

fn add_column_if_missing(tx: &Transaction, table: &str, column: &str, decl: &str) -> Result<()> {
    let exists = tx
        .prepare(&format!(
            "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
        ))?
        .exists(params![column])?;
    if !exists {
        tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))
            .with_context(|| format!("adding {table}.{column}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Meta helpers
// ---------------------------------------------------------------------------

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
        r.get(0)
    })
    .optional()
    .with_context(|| format!("reading meta.{key}"))
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .with_context(|| format!("writing meta.{key}"))?;
    Ok(())
}

fn read_model(conn: &Connection) -> Result<Option<ModelIdentity>> {
    let Some(name) = get_meta(conn, META_MODEL_NAME)? else {
        return Ok(None);
    };
    let dim = get_meta(conn, META_EMBEDDING_DIM)?.ok_or_else(|| {
        anyhow!("meta.{META_MODEL_NAME} is set but meta.{META_EMBEDDING_DIM} is not")
    })?;
    let dim = dim
        .parse()
        .with_context(|| format!("meta.{META_EMBEDDING_DIM} is not a number: {dim:?}"))?;
    let sha256 = get_meta(conn, META_MODEL_SHA256)?.filter(|s| !s.is_empty());
    Ok(Some(ModelIdentity { name, dim, sha256 }))
}

fn write_model(conn: &Connection, id: &ModelIdentity) -> Result<()> {
    set_meta(conn, META_MODEL_NAME, &id.name)?;
    set_meta(conn, META_EMBEDDING_DIM, &id.dim.to_string())?;
    match &id.sha256 {
        Some(sha) => set_meta(conn, META_MODEL_SHA256, sha),
        None => conn
            .execute(
                "DELETE FROM meta WHERE key = ?1",
                params![META_MODEL_SHA256],
            )
            .map(drop)
            .context("clearing meta.model_sha256"),
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn utf8_path(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", path.display()))
}

/// `(base, lo, hi)` such that a path is `scope` or inside it exactly when
/// `path = base OR (lo <= path < hi)`: descendants of `/a/b` are the strings in
/// `["/a/b/", "/a/b0")` because `'0'` is the byte after `'/'`.
fn scope_bounds(scope: &Path) -> Result<(String, String, String)> {
    let base = utf8_path(scope)?.trim_end_matches('/');
    Ok((base.to_owned(), format!("{base}/"), format!("{base}0")))
}

// ---------------------------------------------------------------------------
// Embedding helpers
// ---------------------------------------------------------------------------

/// A scored candidate in the top-k heap, ordered by score.
struct Candidate {
    score: f32,
    result: SearchResult,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score.total_cmp(&other.score)
    }
}

/// Dot product of `query` with a little-endian f32 blob of the same length,
/// without allocating.
fn dot_blob(query: &[f32], blob: &[u8]) -> f32 {
    let (words, _) = blob.as_chunks::<4>();
    words
        .iter()
        .zip(query)
        .map(|(w, q)| f32::from_le_bytes(*w) * q)
        .sum()
}

/// Validate `v` (length `dim`, finite values) and pack it normalised.
fn normalized_blob(v: &[f32], dim: usize) -> Result<Vec<u8>> {
    if v.len() != dim {
        bail!(
            "vector has dimension {}, the index model has {dim}",
            v.len()
        );
    }
    if v.iter().any(|x| !x.is_finite()) {
        bail!("vector contains non-finite values");
    }
    Ok(pack_f32(&normalize(v)))
}

fn vector_from_blob(blob: &[u8]) -> Result<Vec<f32>> {
    if !blob.len().is_multiple_of(4) {
        bail!(
            "stored vector has {} bytes, not a multiple of 4",
            blob.len()
        );
    }
    Ok(unpack_f32(blob))
}

/// Pack a `f32` slice to little-endian bytes (the BLOB format).
pub fn pack_f32(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Unpack little-endian bytes to `f32`s. Empty if the length is not a
/// multiple of 4.
pub fn unpack_f32(bytes: &[u8]) -> Vec<f32> {
    if !bytes.len().is_multiple_of(4) {
        return Vec::new();
    }
    let (words, _) = bytes.as_chunks::<4>();
    words.iter().map(|w| f32::from_le_bytes(*w)).collect()
}

/// `v` scaled to unit L2 norm; an all-zero vector is returned unchanged.
pub fn normalize(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

/// Dot product of two slices; an error when their lengths differ.
pub fn dot(a: &[f32], b: &[f32]) -> Result<f32> {
    if a.len() != b.len() {
        bail!(
            "dot product of vectors of dimension {} and {}",
            a.len(),
            b.len()
        );
    }
    Ok(a.iter().zip(b).map(|(x, y)| x * y).sum())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const DIM: usize = 4;

    fn opts(read_only: bool) -> StoreOptions {
        StoreOptions {
            wal: false,
            busy_timeout_ms: 1_000,
            read_only,
            migration_wait_ms: 1_000,
        }
    }

    fn model() -> ModelIdentity {
        ModelIdentity::new("test-model", DIM)
    }

    fn open_in_memory() -> Store {
        let mut store = Store::open(Path::new(":memory:"), &opts(false)).expect("in-memory DB");
        store.ensure_model(&model()).unwrap();
        store
    }

    fn chunk(start: usize, embedding: [f32; DIM]) -> NewChunk {
        NewChunk {
            byte_offset: start * 10,
            byte_end: start * 10 + 9,
            start_line: start,
            end_line: start + 1,
            text_hash: format!("h{start}"),
            embedding: embedding.to_vec(),
        }
    }

    fn put(store: &mut Store, path: &str, chunks: &[NewChunk]) {
        store
            .replace_file(&model(), Path::new(path), 1.0, 100, "hash", chunks)
            .unwrap();
    }

    const E1: [f32; DIM] = [1.0, 0.0, 0.0, 0.0];
    const E2: [f32; DIM] = [0.0, 1.0, 0.0, 0.0];

    // --- helpers ---

    #[test]
    fn pack_unpack_roundtrip_and_dot() {
        let orig = vec![1.0_f32, -0.5, 0.0, 42.0];
        assert_eq!(unpack_f32(&pack_f32(&orig)), orig);
        assert!(unpack_f32(&[0u8; 5]).is_empty());
        assert!((dot(&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]).unwrap() - 32.0).abs() < 1e-6);
        assert!(
            dot(&[1.0], &[1.0, 2.0]).is_err(),
            "length mismatch is an error"
        );
        let n = normalize(&[3.0, 4.0]);
        assert!((n[0] - 0.6).abs() < 1e-6 && (n[1] - 0.8).abs() < 1e-6);
        assert!((dot_blob(&[0.5, 2.0], &pack_f32(&[2.0, 1.0])) - 3.0).abs() < 1e-6);
    }

    // --- schema and migrations ---

    #[test]
    fn new_database_is_at_current_version() {
        let store = open_in_memory();
        assert_eq!(user_version(&store.conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(store.stats().unwrap(), (0, 0, None));
    }

    /// The schema written by vec before versioning (no user_version, no
    /// files.size, no chunks.text_hash, vectors not normalised).
    const LEGACY_SCHEMA: &str = "
        CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL,
                            mtime REAL NOT NULL, hash TEXT NOT NULL);
        CREATE TABLE chunks (id INTEGER PRIMARY KEY,
                             file_id INTEGER REFERENCES files(id) ON DELETE CASCADE,
                             byte_offset INTEGER NOT NULL, byte_end INTEGER NOT NULL,
                             start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
                             embedding BLOB NOT NULL);
        CREATE INDEX idx_chunks_file ON chunks(file_id);
        INSERT INTO files (id, path, mtime, hash) VALUES (1, '/old/a.rs', 5.0, 'h');
    ";

    fn legacy_db(dir: &Path, vector: &[f32]) -> PathBuf {
        let path = dir.join("legacy.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO chunks (file_id, byte_offset, byte_end, start_line, end_line, embedding)
             VALUES (1, 0, 10, 1, 2, ?1)",
            params![pack_f32(vector)],
        )
        .unwrap();
        path
    }

    #[test]
    fn legacy_database_is_upgraded_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path(), &[3.0, 4.0, 0.0, 0.0]);

        // Read-only cannot upgrade: a clear error.
        let err = Store::open(&path, &opts(true)).err().unwrap().to_string();
        assert!(err.contains("vec updatedb"), "{err}");

        let mut store = Store::open(&path, &opts(false)).unwrap();
        assert_eq!(user_version(&store.conn).unwrap(), SCHEMA_VERSION);
        let rec = store.get_file(Path::new("/old/a.rs")).unwrap().unwrap();
        assert_eq!((rec.size, rec.hash.as_str()), (None, "h"));
        let v = store
            .get_chunk_embedding_at(Path::new("/old/a.rs"), 1)
            .unwrap()
            .unwrap();
        assert!(
            (v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6,
            "{v:?}"
        );

        // The old vectors have dimension 4: a 4-d model is adopted, then the
        // index is usable read-only.
        store.ensure_model(&model()).unwrap();
        drop(store);
        let ro = Store::open(&path, &opts(true)).unwrap();
        assert_eq!(ro.search(&E1, 5, -1.0, None).unwrap().len(), 1);
    }

    #[test]
    fn legacy_vectors_of_another_dimension_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db(dir.path(), &[1.0, 0.0]);
        let mut store = Store::open(&path, &opts(false)).unwrap();
        let err = store.ensure_model(&model()).unwrap_err().to_string();
        assert!(err.contains("--full"), "{err}");
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.db");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert!(Store::open(&path, &opts(false)).is_err());
        assert!(Store::open(&path, &opts(true)).is_err());
    }

    #[test]
    fn read_only_open_needs_an_existing_index_and_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/vec.db");
        let err = Store::open(&path, &opts(true)).err().unwrap().to_string();
        assert!(err.contains("no index"), "{err}");
        assert!(
            !path.parent().unwrap().exists(),
            "read-only creates nothing"
        );

        let mut rw = Store::open(&path, &opts(false)).unwrap();
        rw.ensure_model(&model()).unwrap();
        put(&mut rw, "/p/a.rs", &[chunk(1, E1)]);
        drop(rw);
        let mut ro = Store::open(&path, &opts(true)).unwrap();
        assert_eq!(ro.search(&E1, 5, 0.0, None).unwrap().len(), 1);
        assert!(ro.set_meta("k", "v").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn up_to_date_index_opens_read_write_without_write_access() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vec.db");
        let mut rw = Store::open(&path, &opts(false)).unwrap();
        rw.ensure_model(&model()).unwrap();
        put(&mut rw, "/p/a.rs", &[chunk(1, E1)]);
        drop(rw);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        // Opening must not take a write lock when no migration is due.
        let store = Store::open(&path, &opts(false)).unwrap();
        assert_eq!(store.search(&E1, 5, 0.0, None).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn open_store_bad_parent_returns_err() {
        assert!(Store::open(Path::new("/dev/null/vec.db"), &opts(false)).is_err());
    }

    // --- model identity ---

    #[test]
    fn ensure_model_sets_then_checks() {
        let mut store = Store::open(Path::new(":memory:"), &opts(false)).unwrap();
        assert_eq!(store.model().unwrap(), None);
        store.check_model(&model()).unwrap();
        store.ensure_model(&model()).unwrap();
        assert_eq!(store.model().unwrap(), Some(model()));
        store.ensure_model(&model()).unwrap();

        for other in [
            ModelIdentity::new("other-model", DIM),
            ModelIdentity::new("test-model", DIM + 1),
        ] {
            let err = store.ensure_model(&other).unwrap_err().to_string();
            assert!(
                err.contains("model mismatch") && err.contains("--full"),
                "{err}"
            );
            assert!(store.check_model(&other).is_err());
        }
    }

    #[test]
    fn ensure_model_fills_in_and_then_enforces_sha() {
        let mut store = open_in_memory();
        let mut with_sha = model();
        with_sha.sha256 = Some("aaa".into());
        store.ensure_model(&with_sha).unwrap();
        assert_eq!(store.model().unwrap(), Some(with_sha.clone()));
        store.ensure_model(&model()).unwrap(); // unknown sha: compatible
        let mut other_sha = model();
        other_sha.sha256 = Some("bbb".into());
        assert!(store.ensure_model(&other_sha).is_err());
    }

    #[test]
    fn reset_model_empties_index_and_records_new_identity() {
        let mut store = open_in_memory();
        put(&mut store, "/p/a.rs", &[chunk(1, E1)]);
        let new = ModelIdentity::new("bigger", 8);
        store.reset_model(&new).unwrap();
        assert_eq!(store.stats().unwrap(), (0, 0, None));
        assert_eq!(store.model().unwrap(), Some(new));
    }

    // --- replace_file ---

    #[test]
    fn replace_file_requires_a_model() {
        let mut store = Store::open(Path::new(":memory:"), &opts(false)).unwrap();
        let err = store
            .replace_file(&model(), Path::new("/p/a.rs"), 1.0, 1, "h", &[chunk(1, E1)])
            .unwrap_err()
            .to_string();
        assert!(err.contains("ensure_model"), "{err}");
    }

    #[test]
    fn replace_file_writes_record_and_chunks() {
        let mut store = open_in_memory();
        store
            .replace_file(
                &model(),
                Path::new("/p/a.rs"),
                7.5,
                1234,
                "h1",
                &[chunk(1, [2.0, 0.0, 0.0, 0.0])],
            )
            .unwrap();
        let rec = store.get_file(Path::new("/p/a.rs")).unwrap().unwrap();
        assert_eq!(
            (rec.mtime, rec.size, rec.hash.as_str()),
            (7.5, Some(1234), "h1")
        );
        // Stored normalised.
        assert_eq!(
            store
                .get_chunk_embedding_at(Path::new("/p/a.rs"), 1)
                .unwrap(),
            Some(E1.to_vec())
        );

        store
            .replace_file(
                &model(),
                Path::new("/p/a.rs"),
                8.0,
                99,
                "h2",
                &[chunk(5, E2), chunk(6, E1)],
            )
            .unwrap();
        let rec2 = store.get_file(Path::new("/p/a.rs")).unwrap().unwrap();
        assert_eq!(rec2.id, rec.id, "same path keeps its id");
        assert_eq!(rec2.hash, "h2");
        assert_eq!(store.stats().unwrap().1, 2, "old chunks replaced");
        assert!(
            store
                .get_chunks_covering(Path::new("/p/a.rs"), 1)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn failed_replace_leaves_the_old_state() {
        // Regression: the hash used to be written before the chunks, so a
        // failure left a record that looked up to date with no chunks.
        let mut store = open_in_memory();
        put(&mut store, "/p/a.rs", &[chunk(1, E1)]);
        let bad = NewChunk {
            embedding: vec![1.0; DIM + 1],
            ..chunk(2, E2)
        };
        let err = store
            .replace_file(
                &model(),
                Path::new("/p/a.rs"),
                9.0,
                1,
                "new-hash",
                &[chunk(1, E2), bad],
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("dimension"), "{err:#}");
        assert_eq!(
            store.get_file(Path::new("/p/a.rs")).unwrap().unwrap().hash,
            "hash"
        );
        assert_eq!(
            store
                .get_chunk_embedding_at(Path::new("/p/a.rs"), 1)
                .unwrap(),
            Some(E1.to_vec())
        );
        let nan = chunk(3, [f32::NAN, 0.0, 0.0, 0.0]);
        assert!(
            store
                .replace_file(&model(), Path::new("/p/a.rs"), 9.0, 1, "x", &[nan])
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn replace_file_rejects_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt;
        let mut store = open_in_memory();
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/p/\xff.rs"));
        assert!(
            store
                .replace_file(&model(), path, 1.0, 1, "h", &[])
                .is_err()
        );
        assert!(store.get_file(path).unwrap().is_none());
    }

    #[test]
    fn embeddings_by_text_hash_returns_reusable_vectors() {
        let mut store = open_in_memory();
        put(&mut store, "/p/a.rs", &[chunk(1, E1), chunk(2, E2)]);
        let map = store.embeddings_by_text_hash(Path::new("/p/a.rs")).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map["h1"], E1.to_vec());
        assert_eq!(map["h2"], E2.to_vec());
        assert!(
            store
                .embeddings_by_text_hash(Path::new("/nope"))
                .unwrap()
                .is_empty()
        );
    }

    // --- search ---

    #[test]
    fn search_ranks_and_respects_min_score() {
        let mut store = open_in_memory();
        put(
            &mut store,
            "/p/a.rs",
            &[chunk(1, E1), chunk(2, [1.0, 1.0, 0.0, 0.0])],
        );
        put(&mut store, "/p/b.rs", &[chunk(1, E2)]);
        let results = store.search(&[3.0, 0.0, 0.0, 0.0], 10, 0.5, None).unwrap();
        assert_eq!(results.len(), 2, "the orthogonal chunk is below 0.5");
        assert!((results[0].score - 1.0).abs() < 1e-6);
        assert_eq!(results[0].start_line, 1);
        assert!(results[0].score > results[1].score);
        assert!(store.search(&E1, 0, -1.0, None).unwrap().is_empty());
    }

    #[test]
    fn search_filtered_filters_before_the_limit() {
        let mut store = open_in_memory();
        // Three files, best first; the filter rejects the best one.
        put(&mut store, "/p/best.rs", &[chunk(1, [1.0, 0.0, 0.0, 0.0])]);
        put(&mut store, "/p/mid.rs", &[chunk(1, [0.9, 0.1, 0.0, 0.0])]);
        put(&mut store, "/p/low.rs", &[chunk(1, [0.5, 0.5, 0.0, 0.0])]);
        let mut calls = 0;
        let hits = store
            .search_filtered(&E1, 2, -1.0, None, |r| {
                calls += 1;
                r.path != Path::new("/p/best.rs")
            })
            .unwrap();
        let paths: Vec<_> = hits.iter().map(|h| h.path.clone()).collect();
        assert_eq!(
            paths,
            [PathBuf::from("/p/mid.rs"), PathBuf::from("/p/low.rs")]
        );
        assert!(calls >= 3);
    }

    #[test]
    fn indexed_paths_are_listed_in_order() {
        let mut store = open_in_memory();
        put(&mut store, "/b", &[chunk(1, E1)]);
        put(&mut store, "/a", &[chunk(1, E2)]);
        let mut seen = Vec::new();
        store
            .for_each_indexed_path(|p| seen.push(p.to_path_buf()))
            .unwrap();
        assert_eq!(seen, [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn search_returns_top_k_over_many_rows() {
        let mut store = open_in_memory();
        let chunks: Vec<NewChunk> = (0..1_500)
            .map(|i| chunk(i, [(i + 1) as f32 * 0.001, 0.5, 0.5, 0.5]))
            .collect();
        put(&mut store, "/p/big.rs", &chunks);
        let results = store.search(&E1, 5, -1.0, None).unwrap();
        let lines: Vec<usize> = results.iter().map(|r| r.start_line).collect();
        assert_eq!(lines, [1499, 1498, 1497, 1496, 1495]);
    }

    #[test]
    fn search_rejects_a_query_of_another_dimension() {
        let mut store = open_in_memory();
        put(&mut store, "/p/a.rs", &[chunk(1, E1)]);
        let err = store
            .search(&[1.0, 0.0], 5, 0.0, None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("dimension 2") && err.contains("test-model"),
            "{err}"
        );
    }

    #[test]
    fn search_rejects_wrong_size_blobs_and_skips_nan_scores() {
        let mut store = open_in_memory();
        put(&mut store, "/p/a.rs", &[chunk(1, E1)]);
        store
            .conn
            .execute(
                "INSERT INTO chunks (file_id, byte_offset, byte_end, start_line, end_line, embedding)
                 VALUES (1, 0, 1, 9, 9, ?1)",
                params![pack_f32(&[f32::NAN, 0.0, 0.0, 0.0])],
            )
            .unwrap();
        let results = store.search(&E1, 5, -1.0, None).unwrap();
        assert_eq!(results.len(), 1, "the NaN-scored row is skipped");

        store
            .conn
            .execute(
                "INSERT INTO chunks (file_id, byte_offset, byte_end, start_line, end_line, embedding)
                 VALUES (1, 0, 1, 9, 9, ?1)",
                params![pack_f32(&[1.0, 0.0])],
            )
            .unwrap();
        let err = store.search(&E1, 5, -1.0, None).unwrap_err().to_string();
        assert!(err.contains("8 bytes"), "{err}");
    }

    fn filtered(store: &Store, filter: &str) -> Vec<String> {
        let mut paths: Vec<String> = store
            .search(&E1, 100, -1.0, Some(Path::new(filter)))
            .unwrap()
            .into_iter()
            .map(|r| r.path.display().to_string())
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn path_filter_respects_directory_boundaries() {
        let mut store = open_in_memory();
        for p in [
            "/proj/a/file.rs",
            "/proj/abc/file.rs",
            "/proj/a",
            "/proj/b/x.rs",
        ] {
            put(&mut store, p, &[chunk(1, E1)]);
        }
        assert_eq!(filtered(&store, "/proj/a"), ["/proj/a", "/proj/a/file.rs"]);
        assert_eq!(filtered(&store, "/proj/a/"), ["/proj/a", "/proj/a/file.rs"]);
        assert_eq!(filtered(&store, "/proj/a/file.rs"), ["/proj/a/file.rs"]);
        assert_eq!(filtered(&store, "/").len(), 4);
    }

    #[test]
    fn path_filter_treats_glob_characters_literally() {
        // Regression: the GLOB escaping turned "[" into "[[" after adding "[*]".
        let mut store = open_in_memory();
        for p in [
            "/proj/[module]/file.rs",
            "/proj/module2/x.rs",
            "/a[b/x.rs",
            "/a*b/x.rs",
            "/a?b/x.rs",
            "/axb/x.rs",
        ] {
            put(&mut store, p, &[chunk(1, E1)]);
        }
        assert_eq!(
            filtered(&store, "/proj/[module]"),
            ["/proj/[module]/file.rs"]
        );
        assert_eq!(filtered(&store, "/a[b"), ["/a[b/x.rs"]);
        assert_eq!(filtered(&store, "/a*b"), ["/a*b/x.rs"]);
        assert_eq!(filtered(&store, "/a?b"), ["/a?b/x.rs"]);
    }

    // --- maintenance ---

    #[test]
    fn delete_missing_files_scoped_and_unscoped() {
        let mut store = open_in_memory();
        for p in [
            "/nonexistent/scope/a.rs",
            "/nonexistent/scope-sibling/b.rs",
            "/nonexistent/other/c.rs",
        ] {
            put(&mut store, p, &[chunk(1, E1)]);
        }
        let removed = store
            .delete_files_where(Some(&[PathBuf::from("/nonexistent/scope")]), is_gone)
            .unwrap();
        assert_eq!(removed, 1, "only the record under the scope is pruned");
        assert_eq!(store.delete_missing_files().unwrap(), 2);
        assert_eq!(store.stats().unwrap(), (0, 0, None), "chunks cascade");
    }

    #[test]
    fn delete_files_where_uses_predicate_and_scope() {
        let mut store = open_in_memory();
        for p in ["/s/keep.rs", "/s/drop.rs", "/t/drop.rs"] {
            put(&mut store, p, &[]);
        }
        let removed = store
            .delete_files_where(Some(&[PathBuf::from("/s")]), |p| p.ends_with("drop.rs"))
            .unwrap();
        assert_eq!(removed, 1);
        assert!(store.get_file(Path::new("/t/drop.rs")).unwrap().is_some());
        assert!(store.get_file(Path::new("/s/keep.rs")).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn missing_file_paths_ignore_unreadable_paths() {
        // Regression: Path::exists() reported EACCES as "deleted".
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let hidden = locked.join("f.rs");
        std::fs::write(&hidden, "x").unwrap();
        let mut store = open_in_memory();
        store
            .replace_file(&model(), &hidden, 1.0, 1, "h", &[])
            .unwrap();
        store
            .replace_file(&model(), &dir.path().join("gone.rs"), 1.0, 1, "h", &[])
            .unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let removed = store.delete_missing_files();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(removed.unwrap(), 1, "only the file that is gone");
        assert!(store.get_file(&hidden).unwrap().is_some());
    }

    #[test]
    fn meta_roundtrip() {
        let mut store = open_in_memory();
        assert_eq!(store.get_meta("k").unwrap(), None);
        store.set_meta("k", "1").unwrap();
        store.set_meta("k", "2").unwrap();
        assert_eq!(store.get_meta("k").unwrap().as_deref(), Some("2"));
    }

    // --- typed model mismatch (review 2026-11-19 #5, #8) ---

    #[test]
    fn model_mismatch_is_typed_and_other_errors_are_not() {
        let mut store = open_in_memory();
        let other = ModelIdentity::new("other-model", DIM);
        let err = store.ensure_model(&other).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ModelMismatch>(),
            Some(ModelMismatch::Recorded { stored, .. }) if *stored == model()
        ));
        assert!(store.check_model(&other).unwrap_err().is::<ModelMismatch>());
        assert!(
            !store
                .ensure_model(&ModelIdentity::new("m", 0))
                .unwrap_err()
                .is::<ModelMismatch>()
        );
    }

    #[test]
    fn unrecorded_index_of_another_dimension_is_a_typed_mismatch() {
        let mut store = open_in_memory();
        put(&mut store, "/p/a.rs", &[chunk(1, E1)]);
        store.conn.execute("DELETE FROM meta", []).unwrap();
        let err = store
            .ensure_model(&ModelIdentity::new("m", DIM * 2))
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ModelMismatch>(),
            Some(ModelMismatch::Unrecorded {
                stored_dim: DIM,
                ..
            })
        ));
    }

    /// Regression (#8): a writer that passed `ensure_model` before another
    /// process reset the index for a new model of the same dimension must not
    /// write its old-model vectors.
    #[test]
    fn replace_file_refuses_vectors_of_a_model_reset_away() {
        let mut store = open_in_memory();
        let new = ModelIdentity::new("new-model", DIM);
        store.reset_model(&new).unwrap();
        let err = store
            .replace_file(&model(), Path::new("/p/a.rs"), 1.0, 1, "h", &[chunk(1, E1)])
            .unwrap_err();
        assert!(err.is::<ModelMismatch>(), "{err:#}");
        assert!(store.get_file(Path::new("/p/a.rs")).unwrap().is_none());
        store
            .replace_file(&new, Path::new("/p/a.rs"), 1.0, 1, "h", &[chunk(1, E1)])
            .unwrap();
    }

    // --- schema upgrade under contention (review 2026-11-19 #11) ---

    /// A version-0 database whose write lock another connection holds for
    /// `hold`.
    fn locked_v0_db(hold: Duration) -> (tempfile::TempDir, PathBuf, std::thread::JoinHandle<()>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v0.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let mut conn = conn;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            locked_tx.send(()).unwrap();
            std::thread::sleep(hold);
            tx.commit().unwrap();
        });
        locked_rx.recv().unwrap();
        (dir, path, holder)
    }

    #[test]
    fn schema_upgrade_waits_for_a_lock_held_by_another_process() {
        let hold = Duration::from_millis(600);
        let (_dir, path, holder) = locked_v0_db(hold);
        let opts = StoreOptions {
            busy_timeout_ms: 100,
            migration_wait_ms: 30_000,
            ..opts(false)
        };
        let store = Store::open(&path, &opts).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        holder.join().unwrap();
    }

    #[test]
    fn schema_upgrade_gives_up_after_migration_wait_with_a_clear_message() {
        let (_dir, path, holder) = locked_v0_db(Duration::from_millis(1_500));
        let opts = StoreOptions {
            busy_timeout_ms: 100,
            migration_wait_ms: 300,
            ..opts(false)
        };
        let err = format!("{:#}", Store::open(&path, &opts).err().unwrap());
        assert!(err.contains("migration_wait_ms"), "{err}");
        holder.join().unwrap();
    }
}

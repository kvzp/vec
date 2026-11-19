//! Model identities with the model file's sha256, without hashing a large
//! model file on every run.
//!
//! Writers (`vec updatedb`, `vec watch`, the daemon) record the sha256 of
//! the model file in the index, so they always know it: from the cache when
//! it holds an entry for the file as it is now, else by hashing the file
//! (then the cache is updated). Readers (queries) check name and dimension,
//! and the sha256 only when the cache already knows it: they never hash.
//!
//! The cache is a small TOML file, `user_sha_cache` of `locations.toml` under
//! the user's cache directory. An entry is used only when the model file's
//! path, size, mtime, device and inode are all unchanged. The cache is an
//! optimisation: an unreadable or corrupt cache is a miss, and a cache that
//! cannot be written is skipped (a system service may have no writable cache
//! directory).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use vec_embed::{Embedder, ModelIdentity};

use crate::config::Locations;

/// A cache of model file hashes, stored in one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaCache {
    file: PathBuf,
}

/// What must be unchanged for a cached hash to be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FileKey {
    size: u64,
    mtime_secs: i64,
    mtime_nanos: i64,
    dev: u64,
    ino: u64,
}

impl FileKey {
    fn of(path: &Path) -> std::io::Result<Self> {
        let md = std::fs::metadata(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                size: md.len(),
                mtime_secs: md.mtime(),
                mtime_nanos: md.mtime_nsec(),
                dev: md.dev(),
                ino: md.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let mtime = md
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            Ok(Self {
                size: md.len(),
                mtime_secs: i64::try_from(mtime.as_secs()).unwrap_or(i64::MAX),
                mtime_nanos: i64::from(mtime.subsec_nanos()),
                dev: 0,
                ino: 0,
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    key: FileKey,
    sha256: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    model: Vec<Entry>,
}

impl ShaCache {
    /// A cache stored in `file`.
    #[must_use]
    pub fn at(file: impl Into<PathBuf>) -> Self {
        Self { file: file.into() }
    }

    /// The current user's cache (`user_sha_cache` of `locations.toml` under
    /// the user's cache directory); `None` when the user has no cache
    /// directory.
    ///
    /// # Errors
    ///
    /// The compiled-in `locations.toml` cannot be parsed.
    pub fn for_user() -> Result<Option<Self>> {
        let rel = Locations::get()?.user_sha_cache;
        Ok(dirs::cache_dir().map(|d| Self::at(d.join(rel))))
    }

    /// The file the cache is stored in.
    #[must_use]
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// The cached sha256 of `model`, when the file is unchanged since it was
    /// hashed. Any problem (no cache, a corrupt one, an unreadable model) is
    /// a miss.
    #[must_use]
    pub fn lookup(&self, model: &Path) -> Option<String> {
        let key = FileKey::of(model).ok()?;
        self.read()
            .model
            .into_iter()
            .find(|e| e.path == model && e.key == key)
            .map(|e| e.sha256)
    }

    /// Record `sha256` for `model` as the file is now. Entries of model files
    /// that no longer exist are dropped. The file is replaced atomically.
    ///
    /// # Errors
    ///
    /// The model file cannot be inspected, or the cache cannot be written.
    pub fn store(&self, model: &Path, sha256: &str) -> Result<()> {
        let key = FileKey::of(model)
            .with_context(|| format!("inspecting the model file {}", model.display()))?;
        self.store_key(model, key, sha256)
    }

    fn store_key(&self, model: &Path, key: FileKey, sha256: &str) -> Result<()> {
        let mut cache = self.read();
        cache
            .model
            .retain(|e| e.path != model && FileKey::of(&e.path).is_ok());
        cache.model.push(Entry {
            path: model.to_path_buf(),
            key,
            sha256: sha256.to_owned(),
        });
        let text = toml::to_string(&cache).context("encoding the model hash cache")?;
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = self
            .file
            .with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.file).with_context(|| {
            let _ = std::fs::remove_file(&tmp);
            format!("replacing {}", self.file.display())
        })
    }

    fn read(&self) -> CacheFile {
        std::fs::read_to_string(&self.file)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }
}

/// The identity a reader checks the index with: name and dimension, plus the
/// sha256 when `cache` knows it for the model file as it is now. Never hashes.
#[must_use]
pub fn reader_identity(embedder: &Embedder, cache: Option<&ShaCache>) -> ModelIdentity {
    let mut id = embedder.identity().clone();
    if let (Some(model), Some(cache)) = (embedder.model_file(), cache) {
        id.sha256 = cache.lookup(model);
    }
    id
}

/// The identity a writer records: with the sha256 of the model file (ONNX),
/// from `cache` when it knows the file as it is now, else by hashing the
/// file (the cache is then updated, best effort).
///
/// # Errors
///
/// The model file cannot be read.
pub fn writer_identity(embedder: &Embedder, cache: Option<&ShaCache>) -> Result<ModelIdentity> {
    let Some(model) = embedder.model_file() else {
        return embedder.identity_with_sha();
    };
    let mut id = embedder.identity().clone();
    if let Some(sha) = cache.and_then(|c| c.lookup(model)) {
        id.sha256 = Some(sha);
        return Ok(id);
    }
    let before = FileKey::of(model).ok();
    let sha = Embedder::file_sha256(model)?;
    // Cache only when the file did not change while it was hashed.
    if let (Some(cache), Some(before)) = (cache, before)
        && FileKey::of(model).ok() == Some(before)
    {
        // Best effort: see the module documentation.
        let _ = cache.store_key(model, before, &sha);
    }
    id.sha256 = Some(sha);
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_file(dir: &Path, content: &[u8]) -> PathBuf {
        let p = dir.join("model.onnx");
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn lookup_hits_only_for_an_unchanged_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ShaCache::at(dir.path().join("cache/sha.toml"));
        let model = model_file(dir.path(), b"weights v1");
        assert_eq!(cache.lookup(&model), None, "empty cache");

        cache.store(&model, "aa").unwrap();
        assert_eq!(cache.lookup(&model).as_deref(), Some("aa"));
        assert_eq!(cache.lookup(&dir.path().join("other.onnx")), None);

        // Replaced (new inode, other size): a miss.
        let replacement = dir.path().join("new.onnx");
        std::fs::write(&replacement, b"weights version 2").unwrap();
        std::fs::rename(&replacement, &model).unwrap();
        assert_eq!(cache.lookup(&model), None);
    }

    #[test]
    fn corrupt_cache_is_a_miss_and_is_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sha.toml");
        std::fs::write(&file, "this is [not toml").unwrap();
        let cache = ShaCache::at(&file);
        let model = model_file(dir.path(), b"weights");
        assert_eq!(cache.lookup(&model), None);
        cache.store(&model, "bb").unwrap();
        assert_eq!(cache.lookup(&model).as_deref(), Some("bb"));
    }

    #[test]
    fn entries_of_deleted_models_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ShaCache::at(dir.path().join("sha.toml"));
        let a = dir.path().join("a.onnx");
        let b = dir.path().join("b.onnx");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        cache.store(&a, "aa").unwrap();
        std::fs::remove_file(&a).unwrap();
        cache.store(&b, "bb").unwrap();
        assert_eq!(cache.read().model.len(), 1);
    }

    #[test]
    fn identities_of_a_backend_without_a_file_have_no_sha() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ShaCache::at(dir.path().join("sha.toml"));
        let stub = Embedder::stub(4);
        assert_eq!(reader_identity(&stub, Some(&cache)), *stub.identity());
        assert_eq!(
            writer_identity(&stub, Some(&cache)).unwrap(),
            *stub.identity()
        );
        assert!(!cache.file().exists());
    }

    /// Regression (review 2026-11-19 #9): readers never hash the model file;
    /// a writer hashes it once, then readers and writers use the cache.
    #[test]
    #[ignore = "needs a real ONNX model in VEC_TEST_MODEL"]
    fn real_model_readers_never_hash_and_writers_fill_the_cache() {
        let model = PathBuf::from(
            std::env::var("VEC_TEST_MODEL").expect("VEC_TEST_MODEL = path of an .onnx file"),
        );
        let cfg = crate::config::Config::defaults().unwrap();
        let e = Embedder::load(&model, "test-model", &cfg.onnx_options()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cache = ShaCache::at(dir.path().join("sha.toml"));

        let reader = reader_identity(&e, Some(&cache));
        assert_eq!(reader.sha256, None, "a cache miss: no hash, not computed");
        assert_eq!(reader_identity(&e, None).sha256, None);

        let writer = writer_identity(&e, Some(&cache)).unwrap();
        let sha = Embedder::file_sha256(&model).unwrap();
        assert_eq!(writer.sha256.as_deref(), Some(sha.as_str()));
        assert_eq!(cache.lookup(&model).as_deref(), Some(sha.as_str()));
        assert_eq!(reader_identity(&e, Some(&cache)), writer);
        assert_eq!(writer_identity(&e, Some(&cache)).unwrap(), writer);
    }

    #[test]
    fn for_user_is_under_the_cache_directory() {
        if let (Some(cache), Some(dir)) = (ShaCache::for_user().unwrap(), dirs::cache_dir()) {
            assert!(cache.file().starts_with(dir));
        }
    }
}

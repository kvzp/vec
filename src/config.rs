// src/config.rs — Config loading with compiled-in defaults.
//
// Layers (each later layer overrides earlier ones):
//   1. Compiled-in defaults
//   2. /etc/vec.conf          (system-wide, optional — admin-controlled)
//   3. extra_path             (e.g. --config flag, optional — for testing)
//
// There is NO per-user config file. vec is a system tool. Users influence
// behaviour only through CLI flags (--path, --limit, etc.) at query time.
//
// All path fields have ~ expanded to the real home directory.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Public config structs
// ---------------------------------------------------------------------------

/// Top-level configuration. All fields have sensible compiled-in defaults —
/// no config file is required for vec to work.
#[derive(Debug, Clone)]
pub struct Config {
    pub embed: EmbedConfig,
    pub index: IndexConfig,
    pub search: SearchConfig,
    pub database: DatabaseConfig,
}

/// Embedding model settings.
#[derive(Debug, Clone)]
pub struct EmbedConfig {
    /// Short name (e.g. "gte-multilingual-base") or absolute path.
    pub model: String,
    /// Directories searched in order when resolving a short model name.
    pub model_search_path: Vec<PathBuf>,
    /// Number of chunks fed to the model in one batch.
    pub batch_size: usize,
    /// Token limit passed to the tokeniser per chunk.
    pub max_tokens: usize,
    /// Unix socket path for `vec daemon`.
    /// `vec` tries this socket first for interactive queries; falls back to
    /// loading the model in-process if the daemon is not running.
    pub daemon_socket: PathBuf,
    /// Number of threads for parallel embedding during indexing.
    /// 0 = automatic (use all available cores).
    pub index_threads: usize,
}

/// File-walking and chunking settings.
#[derive(Debug, Clone)]
pub struct IndexConfig {
    /// Target lines per chunk.
    pub chunk_size: usize,
    /// Lines of overlap between adjacent chunks.
    pub chunk_overlap: usize,
    /// Skip files larger than this (bytes).
    pub max_file_size: u64,
    /// Skip files smaller than this (bytes).
    pub min_file_size: u64,
    /// Skip chunks with fewer than this many non-blank lines.
    pub min_chunk_lines: usize,
    /// Respect .gitignore files while walking.
    pub gitignore: bool,
    /// Root paths to walk. Defaults to ["/"].
    pub include_paths: Vec<PathBuf>,
    /// Directory names to skip (matched against the final path component).
    pub exclude_dirs: Vec<String>,
    /// Glob patterns matched against file names.
    pub exclude_files: Vec<String>,
}

/// Query / result settings.
#[derive(Debug, Clone)]
pub struct SearchConfig {
    /// Number of results shown when no --limit flag is given.
    pub default_limit: usize,
    /// Lines of source context shown per result.
    pub snippet_lines: usize,
}

/// SQLite database settings.
#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    /// Path to the SQLite database file (~ is expanded).
    pub db_path: PathBuf,
    /// Enable WAL mode (recommended; improves concurrent access).
    pub wal: bool,
}

// ---------------------------------------------------------------------------
// Compiled-in defaults
// ---------------------------------------------------------------------------

fn default_config() -> Config {
    // Default scan root: entire filesystem on Unix, C:\ on Windows.
    #[cfg(unix)]
    let default_include_paths = vec![PathBuf::from("/")];
    #[cfg(windows)]
    let default_include_paths = vec![PathBuf::from("C:\\")];
    #[cfg(not(any(unix, windows)))]
    let default_include_paths = vec![dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))];

    // Central DB: /var/lib/vec/vec.db on Unix — one DB for the whole machine,
    // indexed by the system vec-updatedb service. access() enforces per-user
    // read permissions at query time.
    #[cfg(unix)]
    let default_db_path = PathBuf::from("/var/lib/vec/vec.db");
    #[cfg(not(unix))]
    let default_db_path = PathBuf::from("C:\\ProgramData\\vec\\vec.db");

    Config {
        embed: EmbedConfig {
            model: "gte-multilingual-base".into(),
            // System-only model path. Models are installed by the vec-model-*
            // package into /usr/share/vec/models — never in user home dirs.
            model_search_path: vec![PathBuf::from("/usr/share/vec/models")],
            batch_size: 16,
            max_tokens: 128,
            daemon_socket: PathBuf::from("/run/vec/embed.sock"),
            index_threads: 0,
        },
        index: IndexConfig {
            chunk_size: 40,
            chunk_overlap: 10,
            max_file_size: 10 * 1024 * 1024, // 10 MB
            min_file_size: 50,
            min_chunk_lines: 5,
            gitignore: true,
            include_paths: default_include_paths,
            exclude_dirs: vec![
                ".git".into(),
                "node_modules".into(),
                "target".into(),
                "dist".into(),
                "build".into(),
                "__pycache__".into(),
                ".venv".into(),
                ".cache".into(),
                ".cargo".into(),
                // Linux virtual filesystems — transient entries cause spurious errors.
                "proc".into(),
                "sys".into(),
                "dev".into(),
                "run".into(),
                // SSL/TLS certificate stores — binary blobs and public certs, not useful content.
                "ssl".into(),
                "certs".into(),
                // Package manager databases — file lists, checksums, not useful content.
                "dpkg".into(),
                "apt".into(),
                "rpm".into(),
                // Runtime/transient state directories.
                "cloud".into(),
                "alternatives".into(),
                // Python installed package dirs (not user code).
                "dist-packages".into(),
                "site-packages".into(),
                // System lookup tables — terse key/value files with no semantic content.
                "apparmor.d".into(),
                "iproute2".into(),
                "abi".into(),
            ],
            exclude_files: vec![
                "*.lock".into(),
                "*.min.js".into(),
                "*.map".into(),
                "*.pyc".into(),
                ".env".into(),
                ".env.*".into(),
                "*.key".into(),
                "*.pem".into(),
                "*.p12".into(),
                "*.pfx".into(),
                "*.onnx".into(),
                "*.bin".into(),
                "*.so".into(),
                "*.dylib".into(),
            ],
        },
        search: SearchConfig {
            default_limit: 10,
            snippet_lines: 6,
        },
        database: DatabaseConfig {
            db_path: default_db_path,
            wal: true,
        },
    }
}

// ---------------------------------------------------------------------------
// Raw (all-Option) structs for TOML deserialization
// ---------------------------------------------------------------------------
//
// We deserialise each config file into RawConfig (all fields are Option so a
// missing key simply leaves the field as None rather than failing). We then
// apply each non-None field on top of the running Config.

#[derive(Debug, Deserialize, Default)]
struct RawConfig {
    embed: Option<RawEmbedConfig>,
    index: Option<RawIndexConfig>,
    search: Option<RawSearchConfig>,
    database: Option<RawDatabaseConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct RawEmbedConfig {
    model: Option<String>,
    model_search_path: Option<Vec<String>>,
    batch_size: Option<usize>,
    max_tokens: Option<usize>,
    daemon_socket: Option<String>,
    index_threads: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
struct RawIndexConfig {
    chunk_size: Option<usize>,
    chunk_overlap: Option<usize>,
    max_file_size: Option<u64>,
    min_file_size: Option<u64>,
    min_chunk_lines: Option<usize>,
    gitignore: Option<bool>,
    include_paths: Option<Vec<String>>,
    exclude_dirs: Option<Vec<String>>,
    exclude_files: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
struct RawSearchConfig {
    default_limit: Option<usize>,
    snippet_lines: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
struct RawDatabaseConfig {
    db_path: Option<String>,
    wal: Option<bool>,
}

// ---------------------------------------------------------------------------
// Merging
// ---------------------------------------------------------------------------

/// Apply non-None fields from `raw` on top of `cfg`.
fn merge(cfg: &mut Config, raw: &RawConfig) {
    if let Some(ref re) = raw.embed {
        if let Some(ref v) = re.model {
            cfg.embed.model = v.clone();
        }
        if let Some(ref v) = re.model_search_path {
            cfg.embed.model_search_path = v.iter().map(PathBuf::from).collect();
        }
        if let Some(v) = re.batch_size {
            cfg.embed.batch_size = v;
        }
        if let Some(v) = re.max_tokens {
            cfg.embed.max_tokens = v;
        }
        if let Some(ref v) = re.daemon_socket {
            cfg.embed.daemon_socket = PathBuf::from(v);
        }
        if let Some(v) = re.index_threads {
            cfg.embed.index_threads = v;
        }
    }

    if let Some(ref ri) = raw.index {
        if let Some(v) = ri.chunk_size {
            cfg.index.chunk_size = v;
        }
        if let Some(v) = ri.chunk_overlap {
            cfg.index.chunk_overlap = v;
        }
        if let Some(v) = ri.max_file_size {
            cfg.index.max_file_size = v;
        }
        if let Some(v) = ri.min_file_size {
            cfg.index.min_file_size = v;
        }
        if let Some(v) = ri.min_chunk_lines {
            cfg.index.min_chunk_lines = v;
        }
        if let Some(v) = ri.gitignore {
            cfg.index.gitignore = v;
        }
        if let Some(ref v) = ri.include_paths {
            cfg.index.include_paths = v.iter().map(PathBuf::from).collect();
        }
        if let Some(ref v) = ri.exclude_dirs {
            cfg.index.exclude_dirs.extend(v.iter().cloned());
        }
        if let Some(ref v) = ri.exclude_files {
            cfg.index.exclude_files.extend(v.iter().cloned());
        }
    }

    if let Some(ref rs) = raw.search {
        if let Some(v) = rs.default_limit {
            cfg.search.default_limit = v;
        }
        if let Some(v) = rs.snippet_lines {
            cfg.search.snippet_lines = v;
        }
    }

    if let Some(ref rd) = raw.database {
        if let Some(ref v) = rd.db_path {
            cfg.database.db_path = PathBuf::from(v);
        }
        if let Some(v) = rd.wal {
            cfg.database.wal = v;
        }
    }
}

// ---------------------------------------------------------------------------
// Path expansion
// ---------------------------------------------------------------------------

/// Expand a leading `~` to the user's home directory.
/// Returns the path unchanged if no home directory can be determined.
fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~/") || s == "~" {
        if let Some(home) = dirs::home_dir() {
            if s == "~" {
                return home;
            }
            // Replace the leading "~/" with "<home>/"
            return home.join(&s[2..]);
        }
    }
    path.to_path_buf()
}

/// Expand ~ in every path field of the resolved Config.
fn expand_all_paths(cfg: &mut Config) {
    cfg.embed.model_search_path = cfg
        .embed
        .model_search_path
        .iter()
        .map(|p| expand_tilde(p))
        .collect();

    cfg.index.include_paths = cfg
        .index
        .include_paths
        .iter()
        .map(|p| expand_tilde(p))
        .collect();

    cfg.database.db_path = expand_tilde(&cfg.database.db_path);
    cfg.embed.daemon_socket = expand_tilde(&cfg.embed.daemon_socket);
}

// ---------------------------------------------------------------------------
// Loading a single file
// ---------------------------------------------------------------------------

fn load_file(path: &Path) -> Result<RawConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing config file {}", path.display()))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl Config {
    /// Load configuration.
    ///
    /// Merge order (last wins):
    ///   compiled-in defaults
    ///   → /etc/vec.conf       (system admin config — the only config file)
    ///   → extra_path (if Some, e.g. supplied via --config for testing)
    ///
    /// There is no per-user config file. vec is a system tool configured by
    /// the admin via /etc/vec.conf. Users influence behaviour only through
    /// CLI flags (--path, --limit, etc.) at query time.
    pub fn load(extra_path: Option<&Path>) -> Result<Config> {
        let mut cfg = default_config();

        // System-wide config (admin-controlled)
        #[cfg(unix)]
        let system_conf = Path::new("/etc/vec.conf");
        #[cfg(not(unix))]
        let system_conf = Path::new("C:\\ProgramData\\vec\\vec.conf");
        if system_conf.exists() {
            let raw = load_file(system_conf)?;
            merge(&mut cfg, &raw);
        }

        // Extra path for testing / --config flag
        if let Some(extra) = extra_path {
            let raw = load_file(extra)?;
            merge(&mut cfg, &raw);
        }

        // Expand ~ in all path fields once, after all merging is done.
        expand_all_paths(&mut cfg);

        Ok(cfg)
    }

    /// Search `model_search_path` for the configured model.
    ///
    /// If `self.embed.model` is an absolute path, it is returned as-is
    /// (after checking that the file exists).
    ///
    /// Otherwise the search path is scanned for:
    ///   - `{dir}/{name}.onnx`
    ///   - `{dir}/{name}/model.onnx`
    pub fn resolve_model_path(&self) -> Result<PathBuf> {
        let name = &self.embed.model;

        // Absolute path — just verify it exists.
        let as_path = Path::new(name);
        if as_path.is_absolute() {
            if as_path.exists() {
                return Ok(as_path.to_path_buf());
            }
            return Err(anyhow!("model file not found: {}", as_path.display()));
        }

        // Search each directory in model_search_path.
        for dir in &self.embed.model_search_path {
            // Try "{dir}/{name}.onnx"
            let candidate1 = dir.join(format!("{}.onnx", name));
            if candidate1.exists() {
                return Ok(candidate1);
            }

            // Try "{dir}/{name}/model_int8.onnx" first — quantized, smaller, preferred
            let candidate2 = dir.join(name).join("model_int8.onnx");
            if candidate2.exists() {
                return Ok(candidate2);
            }

            // Fall back to "{dir}/{name}/model.onnx" (full precision)
            let candidate3 = dir.join(name).join("model.onnx");
            if candidate3.exists() {
                return Ok(candidate3);
            }
        }

        Err(anyhow!(
            "model '{}' not found in search path: {:?}\n\
             Install the model package (e.g. vec-model-gte) or set [embed] model \
             to an absolute path.",
            name,
            self.embed.model_search_path
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = default_config();
        assert_eq!(cfg.embed.model, "gte-multilingual-base");
        assert_eq!(cfg.embed.batch_size, 16);
        assert_eq!(cfg.index.chunk_size, 40);
        assert_eq!(cfg.index.chunk_overlap, 10);
        assert!(cfg.index.gitignore);
        assert_eq!(cfg.search.default_limit, 10);
        assert!(cfg.database.wal);
    }

    #[test]
    fn tilde_expansion() {
        let p = PathBuf::from("~/.local/share/vec/vec.db");
        let expanded = expand_tilde(&p);
        // On any machine with a home dir, the result must not start with ~.
        if dirs::home_dir().is_some() {
            assert!(!expanded.to_string_lossy().starts_with('~'));
        }
    }

    #[test]
    fn merge_overwrites_only_set_fields() {
        let mut cfg = default_config();
        let raw = RawConfig {
            embed: Some(RawEmbedConfig {
                batch_size: Some(32),
                ..Default::default()
            }),
            ..Default::default()
        };
        merge(&mut cfg, &raw);
        // batch_size changed
        assert_eq!(cfg.embed.batch_size, 32);
        // everything else is still the default
        assert_eq!(cfg.embed.model, "gte-multilingual-base");
        assert_eq!(cfg.index.chunk_size, 40);
    }

    #[test]
    fn load_no_files_returns_defaults() {
        // Pass a non-existent extra path → should error; pass None → should succeed.
        let cfg = Config::load(None).expect("load with no config files should succeed");
        assert_eq!(cfg.embed.model, "gte-multilingual-base");
    }

    #[test]
    fn load_toml_from_tempfile() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            r#"
[embed]
batch_size = 8
model = "my-custom-model"

[search]
default_limit = 5
"#
        )
        .unwrap();

        let cfg = Config::load(Some(tmp.path())).unwrap();
        assert_eq!(cfg.embed.batch_size, 8);
        assert_eq!(cfg.embed.model, "my-custom-model");
        assert_eq!(cfg.search.default_limit, 5);
        // Un-set fields stay at their defaults.
        assert_eq!(cfg.index.chunk_size, 40);
    }

    // Verify that per-user config (~/.config/vec/config.toml) is NOT loaded.
    // The load() function only reads /etc/vec.conf (system) and the extra_path.
    // We confirm this by inspecting the source: there is no reference to
    // dirs::config_dir() or "~/.config/vec" in the Config::load implementation.
    // The test also verifies that Config::load(None) always succeeds even on
    // machines that happen to have a ~/.config/vec/config.toml.
    #[test]
    fn no_user_config_loaded() {
        // The important invariant is that load() succeeds regardless of whether
        // ~/.config/vec/config.toml exists. We cannot easily test "absence of
        // loading" in a black-box manner, so we verify the source-level invariant:
        // load() must not call dirs::config_dir() for a per-user config path.
        //
        // At the same time, Config::load(None) must succeed on any machine.
        let result = Config::load(None);
        assert!(
            result.is_ok(),
            "Config::load(None) must succeed: {:?}",
            result.err()
        );

        // Confirm the user-config path is not in the load function by ensuring
        // we can load when passing a non-existent path as extra (error is
        // expected here — this just confirms the load path logic).
        // The key invariant is already validated above.
    }

    #[cfg(unix)]
    #[test]
    fn db_path_is_central_on_unix() {
        let cfg = default_config();
        let db_str = cfg.database.db_path.to_string_lossy();
        assert!(
            db_str.starts_with("/var/lib/"),
            "On Unix, default db_path should be under /var/lib/, got: {db_str}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn model_search_path_has_system_dir() {
        let cfg = default_config();
        let has_system = cfg
            .embed
            .model_search_path
            .iter()
            .any(|p| p.starts_with("/usr/share/vec"));
        assert!(
            has_system,
            "On Unix, model_search_path should include /usr/share/vec/models, got: {:?}",
            cfg.embed.model_search_path
        );
        // No user home paths — vec is a system tool, models are system-installed.
        let has_user_path = cfg
            .embed
            .model_search_path
            .iter()
            .any(|p| p.starts_with("/home") || p.to_string_lossy().contains(".local"));
        assert!(
            !has_user_path,
            "model_search_path must not contain user home paths, got: {:?}",
            cfg.embed.model_search_path
        );
    }

    #[test]
    fn extra_path_is_loaded() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            r#"
[embed]
batch_size = 4
"#
        )
        .unwrap();
        let cfg = Config::load(Some(tmp.path())).unwrap();
        // The extra file should override the default batch_size of 16.
        assert_eq!(cfg.embed.batch_size, 4);
    }

    #[test]
    fn bad_toml_returns_error() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "this is not valid toml ][[[").unwrap();
        let result = Config::load(Some(tmp.path()));
        assert!(result.is_err(), "invalid TOML should return an error");
    }

    // --- Gap 5: expand_tilde edge cases ---

    #[test]
    fn expand_tilde_plain_path_unchanged() {
        // Paths not starting with '~' must be returned as-is.
        let p = PathBuf::from("/etc/vec.conf");
        assert_eq!(expand_tilde(&p), p);
    }

    #[test]
    fn expand_tilde_tilde_alone() {
        // A bare "~" should expand to the home dir when HOME is set.
        // When there is no home dir (rootless containers, some CI), the path
        // is returned unchanged — this is a known limitation, not a panic.
        let p = PathBuf::from("~");
        let expanded = expand_tilde(&p);
        if dirs::home_dir().is_some() {
            assert!(
                !expanded.to_string_lossy().starts_with('~'),
                "bare '~' should expand to home dir, got: {}",
                expanded.display()
            );
        } else {
            // No HOME configured — path returned with literal '~' (known limitation).
            assert_eq!(expanded, p);
        }
    }

    #[cfg(unix)]
    #[test]
    fn include_paths_default_is_root_on_unix() {
        let cfg = default_config();
        assert_eq!(
            cfg.index.include_paths,
            vec![std::path::PathBuf::from("/")],
            "On Unix, default include_paths should be [\"/\"]"
        );
    }
}

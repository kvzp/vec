//! Configuration: compiled-in defaults plus optional TOML layers.
//!
//! Layers, each overriding the previous one:
//!
//! 1. [`DEFAULTS_TOML`], the file `crates/vec-core/defaults.toml` compiled into the
//!    binary. It holds every default value; the Rust code holds none.
//! 2. The system file (`/etc/vec.conf` by default), optional.
//! 3. The per-user file (`~/.config/vec/config.toml` by default), optional, and
//!    skipped when an explicit file is given.
//! 4. The explicit file (`--config`), which must exist.
//!
//! The locations of layers 2 and 3 come from `crates/vec-core/locations.toml`.
//!
//! Merging works on TOML tables: a key replaces the previous value, except
//! `index.exclude_dirs` and `index.exclude_files`, which append, and where an
//! entry `"!pattern"` removes `pattern` from the list built so far. Every layer
//! is checked on its own on top of the defaults, so an unknown key or a value of
//! the wrong type is reported with the file that contains it. The merged table
//! is then deserialized into [`Config`] (`deny_unknown_fields` everywhere), `~/` is
//! expanded in path values, and [`Config::validate`] checks value ranges and
//! relations, naming the keys involved.
//!
//! [`Config::provenance`] lists each effective key with the layer(s) that set it
//! (for `vec config show`).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Deserializer};
use toml::{Table, Value};
use vec_embed::{HttpOptions, OnnxOptions};

/// The compiled-in defaults (`crates/vec-core/defaults.toml`), also the source of
/// the `vec init` templates.
pub const DEFAULTS_TOML: &str = include_str!("../defaults.toml");

/// Where the system and user configuration files live (`crates/vec-core/locations.toml`).
const LOCATIONS_TOML: &str = include_str!("../locations.toml");

/// List keys whose layers append instead of replace (section, key).
const APPEND_KEYS: &[(&str, &str)] = &[("index", "exclude_dirs"), ("index", "exclude_files")];

/// Deprecated keys still accepted for one release: (section, old key, new
/// section, new key). The old key's value is used for the new key (unless the
/// same layer sets the new key too), with a warning.
const DEPRECATED_KEYS: &[(&str, &str, &str, &str)] =
    &[("embed", "daemon_socket", "daemon", "socket")];

// ---------------------------------------------------------------------------
// Public config structs
// ---------------------------------------------------------------------------

/// The effective configuration. Build it with [`Config::load`],
/// [`Config::defaults`] or [`Config::from_toml`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub embed: EmbedConfig,
    pub index: IndexConfig,
    pub search: SearchConfig,
    pub database: DatabaseConfig,
    pub watch: WatchConfig,
    pub daemon: DaemonConfig,
    /// Effective value and origin of every key, sorted by key.
    #[serde(skip)]
    provenance: Vec<KeyOrigin>,
    /// The layers that were loaded, defaults first.
    #[serde(skip)]
    layers: Vec<ConfigLayer>,
    /// Deprecation warnings raised while loading (see [`Config::warnings`]).
    #[serde(skip)]
    warnings: Vec<String>,
}

/// Which embedding backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedBackend {
    /// A local ONNX model run in-process.
    Onnx,
    /// An Ollama-compatible HTTP embedding API.
    Ollama,
}

impl fmt::Display for EmbedBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Onnx => "onnx",
            Self::Ollama => "ollama",
        })
    }
}

/// `[embed]`: embedding model settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbedConfig {
    pub backend: EmbedBackend,
    /// ONNX: short name or absolute path of the model. Ollama: model name.
    pub model: String,
    /// Ollama: base URL of the server. No default.
    #[serde(default)]
    pub embed_url: Option<String>,
    pub http_endpoint_path: String,
    pub http_connect_timeout_ms: u64,
    pub http_read_timeout_ms: u64,
    pub http_concurrency: usize,
    pub http_max_input_bytes: usize,
    /// Ollama: characters of an error response body quoted in error messages.
    pub http_error_preview_chars: usize,
    /// Directories searched for a short ONNX model name.
    pub model_search_path: Vec<PathBuf>,
    /// File names tried inside `{dir}/{model}/`, in order.
    pub model_files: Vec<String>,
    /// Texts per embedding call during indexing (HTTP request / thread work unit).
    pub batch_size: usize,
    /// ONNX: texts per forward pass.
    pub onnx_batch_size: usize,
    /// ONNX: token limit per chunk.
    pub max_tokens: usize,
    /// Embedding threads during indexing; 0 = one per core.
    pub index_threads: usize,
    /// `vec model download`: base URL the model files are fetched from.
    pub model_download_url: String,
    /// `vec model download`: files fetched, relative to `model_download_url`;
    /// each is saved under its last path component.
    pub model_download_files: Vec<String>,
}

/// `[index]`: file walking and chunking.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexConfig {
    pub chunk_size: usize,
    pub chunk_overlap: usize,
    pub max_file_size: u64,
    pub min_file_size: u64,
    pub min_chunk_lines: usize,
    pub gitignore: bool,
    pub include_paths: Vec<PathBuf>,
    pub exclude_dirs: Vec<String>,
    pub exclude_files: Vec<String>,
    /// Lines a chunk boundary may move to land on a boundary pattern.
    pub boundary_scan: usize,
    /// Line prefixes (after indentation) that make good chunk boundaries.
    pub boundary_patterns: Vec<String>,
}

/// `[search]`: query and result settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    pub default_limit: usize,
    /// Snippet radius: lines shown before and after the best line (a
    /// snippet has up to `2 * snippet_lines + 1` lines).
    pub snippet_lines: usize,
    pub path_boost: f32,
    /// Query words shorter than this (in characters) never boost a path.
    pub path_boost_min_word_chars: usize,
    /// Most queries accepted by one search (CLI and MCP).
    pub max_queries: usize,
    /// Results scoring below this are dropped.
    pub min_score: f32,
    /// Lines around the requested line shown by `vec context`.
    pub context_window: usize,
}

/// `[database]`: SQLite settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub db_path: PathBuf,
    pub wal: bool,
    /// How long to wait for a lock held by another connection (ms).
    pub busy_timeout_ms: u64,
    /// How long to keep retrying for the lock of a schema upgrade held by
    /// another process (ms).
    pub migration_wait_ms: u64,
}

/// `[watch]`: real-time watcher.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchConfig {
    pub debounce_ms: u64,
    pub max_wait_ms: u64,
    pub max_pending: usize,
    pub startup_rescan: bool,
    pub retry_max_ms: u64,
}

/// `[daemon]`: the embedding daemon.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    pub socket: PathBuf,
    pub socket_mode: SocketMode,
    pub max_request_bytes: usize,
    /// Most texts in one request.
    pub max_texts_per_request: usize,
    /// Bound of a stalled exchange (ms): on the daemon, the deadline of each
    /// whole request and each whole reply write.
    pub io_timeout_ms: u64,
    /// How long a client waits for the daemon to answer (ms): its handshake
    /// (with socket activation, daemon start-up) or a request's inference.
    pub reply_timeout_ms: u64,
    /// 0 = never exit when idle.
    pub idle_exit_secs: u64,
    pub max_connections: usize,
    /// Pause after a failed accept() (ms).
    pub accept_retry_ms: u64,
}

/// Unix permission bits, written in the config as an octal string (`"0666"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketMode(u32);

impl SocketMode {
    /// The permission bits.
    #[must_use]
    pub fn bits(self) -> u32 {
        self.0
    }

    fn parse(s: &str) -> Result<Self> {
        let digits = s.strip_prefix("0o").unwrap_or(s);
        let bits = u32::from_str_radix(digits, 8)
            .map_err(|_| anyhow!("{s:?} is not an octal mode such as \"0660\""))?;
        if bits > 0o777 {
            bail!("{s:?} has bits beyond 0777");
        }
        Ok(Self(bits))
    }
}

impl fmt::Display for SocketMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04o}", self.0)
    }
}

impl<'de> Deserialize<'de> for SocketMode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------------

/// A configuration layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigLayer {
    /// The compiled-in defaults.
    Defaults,
    /// The system file.
    System(PathBuf),
    /// The per-user file.
    User(PathBuf),
    /// The file given with `--config`.
    Explicit(PathBuf),
    /// Text passed to [`Config::from_toml`].
    Inline,
}

impl fmt::Display for ConfigLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Defaults => f.write_str("defaults"),
            Self::System(p) => write!(f, "system file {}", p.display()),
            Self::User(p) => write!(f, "user file {}", p.display()),
            Self::Explicit(p) => write!(f, "--config file {}", p.display()),
            Self::Inline => f.write_str("inline config"),
        }
    }
}

/// One effective key, its merged value, and the layers that set it (more
/// than one for the appending lists).
#[derive(Debug, Clone, PartialEq)]
pub struct KeyOrigin {
    /// `section.key`.
    pub key: String,
    /// The merged value as written in TOML (before `~` expansion).
    pub value: Value,
    /// Layers that contributed, lowest priority first.
    pub set_by: Vec<ConfigLayer>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// The compiled-in `locations.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Locations {
    pub(crate) system: PathBuf,
    pub(crate) user: PathBuf,
    pub(crate) user_model_dir: PathBuf,
    pub(crate) user_db_path: PathBuf,
    pub(crate) user_socket: PathBuf,
    pub(crate) user_sha_cache: PathBuf,
    pub(crate) system_binaries: Vec<PathBuf>,
}

impl Locations {
    pub(crate) fn get() -> Result<Self> {
        toml::from_str(LOCATIONS_TOML).context("parsing the compiled-in locations.toml")
    }
}

impl Config {
    /// Load and validate the configuration: defaults, then the system file,
    /// then the user file (only when `extra` is `None`), then `extra`.
    pub fn load(extra: Option<&Path>) -> Result<Config> {
        let cfg = Self::load_unvalidated(extra)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Like [`Config::load`] without [`Config::validate`], for tools that show
    /// a configuration even when its values are out of range. Unknown keys and
    /// wrong types are still errors.
    pub fn load_unvalidated(extra: Option<&Path>) -> Result<Config> {
        let system = Self::system_config_path()?;
        let user = match extra {
            None => Self::user_config_path()?,
            Some(_) => None,
        };
        let cfg = Self::load_layers(&system, user.as_deref(), extra)?;
        for w in cfg.warnings() {
            anstream::eprintln!("vec: warning: {w}");
        }
        Ok(cfg)
    }

    /// The compiled-in defaults alone, validated.
    pub fn defaults() -> Result<Config> {
        let cfg = Self::build(Vec::new())?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// The defaults with `text` (TOML) applied as one layer, validated.
    /// Hermetic: no file is read. Meant for tests and embedding.
    pub fn from_toml(text: &str) -> Result<Config> {
        let table = parse_layer(text).context("parsing inline config")?;
        let cfg = Self::build(vec![(ConfigLayer::Inline, table)])?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// The defaults with the file at `path` applied as the only layer,
    /// validated: checks one file on its own, whatever the system and user
    /// files contain (`vec config check FILE`).
    ///
    /// # Errors
    ///
    /// The file cannot be read or parsed, has an unknown key or a value of
    /// the wrong type, or a value out of range; the error names the file.
    pub fn from_file(path: &Path) -> Result<Config> {
        let layer = ConfigLayer::Explicit(path.to_path_buf());
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let table = parse_layer(&text)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        let cfg = Self::build(vec![(layer.clone(), table)])?;
        cfg.validate().with_context(|| format!("in {layer}"))?;
        Ok(cfg)
    }

    /// Path of the system configuration file.
    pub fn system_config_path() -> Result<PathBuf> {
        Ok(Locations::get()?.system)
    }

    /// Path of the per-user configuration file; `None` when the user has no
    /// configuration directory.
    pub fn user_config_path() -> Result<Option<PathBuf>> {
        let rel = Locations::get()?.user;
        Ok(dirs::config_dir().map(|d| d.join(rel)))
    }

    /// Every effective key with its value and the layer(s) that set it,
    /// sorted by key.
    pub fn provenance(&self) -> &[KeyOrigin] {
        &self.provenance
    }

    /// The layers that were loaded, defaults first.
    pub fn layers(&self) -> &[ConfigLayer] {
        &self.layers
    }

    /// Warnings raised while loading: deprecated keys that were accepted
    /// and mapped to their new location. [`Config::load`] and
    /// [`Config::load_unvalidated`] also print them on stderr.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Merge the defaults with the layers that exist, lowest priority first.
    /// A missing system or user file is skipped (an unreadable one is an
    /// error); a missing explicit file is an error.
    fn load_layers(system: &Path, user: Option<&Path>, extra: Option<&Path>) -> Result<Config> {
        let mut layers = Vec::new();
        if let Some(table) = read_optional_layer(system)? {
            layers.push((ConfigLayer::System(system.to_path_buf()), table));
        }
        if let Some(user) = user
            && let Some(table) = read_optional_layer(user)?
        {
            layers.push((ConfigLayer::User(user.to_path_buf()), table));
        }
        if let Some(extra) = extra {
            let text = std::fs::read_to_string(extra)
                .with_context(|| format!("reading config file {}", extra.display()))?;
            let table = parse_layer(&text)
                .with_context(|| format!("parsing config file {}", extra.display()))?;
            layers.push((ConfigLayer::Explicit(extra.to_path_buf()), table));
        }
        Self::build(layers)
    }

    /// Merge `layers` over the defaults and deserialize the result.
    fn build(layers: Vec<(ConfigLayer, Table)>) -> Result<Config> {
        let defaults =
            parse_layer(DEFAULTS_TOML).context("parsing the compiled-in defaults.toml")?;
        let mut warnings = Vec::new();
        let layers: Vec<(ConfigLayer, Table)> = layers
            .into_iter()
            .map(|(layer, mut table)| {
                warnings.extend(apply_deprecations(&mut table, &layer));
                (layer, table)
            })
            .collect();
        let mut origins: BTreeMap<String, KeyOrigin> = BTreeMap::new();
        let mut merged = Table::new();
        merge_layer(
            &mut merged,
            &defaults,
            &ConfigLayer::Defaults,
            Some(&mut origins),
        )
        .context("merging the compiled-in defaults.toml")?;

        for (layer, table) in &layers {
            // Check the layer alone over the defaults, so that an unknown key
            // or a bad type is reported with the file that holds it.
            let mut alone = defaults.clone();
            merge_layer(&mut alone, table, layer, None)
                .and_then(|()| deserialize(alone).map(drop))
                .with_context(|| format!("in {layer}"))?;
            merge_layer(&mut merged, table, layer, Some(&mut origins))?;
        }

        let mut cfg = deserialize(merged)?;
        cfg.expand_paths();
        cfg.provenance = origins.into_values().collect();
        cfg.warnings = warnings;
        cfg.layers = std::iter::once(ConfigLayer::Defaults)
            .chain(layers.into_iter().map(|(layer, _)| layer))
            .collect();
        Ok(cfg)
    }

    /// Expand a leading `~` in every path value.
    fn expand_paths(&mut self) {
        for p in self
            .embed
            .model_search_path
            .iter_mut()
            .chain(&mut self.index.include_paths)
            .chain([&mut self.database.db_path, &mut self.daemon.socket])
        {
            *p = expand_tilde(p);
        }
        if self.embed.model.starts_with('~') {
            self.embed.model = expand_tilde(Path::new(&self.embed.model))
                .to_string_lossy()
                .into_owned();
        }
    }

    // -----------------------------------------------------------------------
    // Validation
    // -----------------------------------------------------------------------

    /// Check value ranges and relations. The error lists every problem, each
    /// naming its key(s).
    pub fn validate(&self) -> Result<()> {
        let mut problems: Vec<String> = Vec::new();
        let mut check = |ok: bool, problem: String| {
            if !ok {
                problems.push(problem);
            }
        };
        let positive = |v: u64| v > 0;

        let e = &self.embed;
        check(
            !e.model.is_empty(),
            "[embed] model must not be empty".into(),
        );
        check(
            !e.model_search_path.is_empty(),
            "[embed] model_search_path must name at least one directory".into(),
        );
        for p in &e.model_search_path {
            check(
                p.is_absolute(),
                format!(
                    "[embed] model_search_path entry {} must be absolute",
                    p.display()
                ),
            );
        }
        check(
            !e.model_files.is_empty() && e.model_files.iter().all(|f| !f.is_empty()),
            "[embed] model_files must list at least one non-empty file name".into(),
        );
        for (key, value) in [
            ("batch_size", e.batch_size as u64),
            ("onnx_batch_size", e.onnx_batch_size as u64),
            ("max_tokens", e.max_tokens as u64),
            ("http_connect_timeout_ms", e.http_connect_timeout_ms),
            ("http_read_timeout_ms", e.http_read_timeout_ms),
            ("http_concurrency", e.http_concurrency as u64),
            ("http_max_input_bytes", e.http_max_input_bytes as u64),
            (
                "http_error_preview_chars",
                e.http_error_preview_chars as u64,
            ),
        ] {
            check(
                positive(value),
                format!("[embed] {key} must be greater than 0"),
            );
        }
        check(
            e.http_endpoint_path.starts_with('/'),
            format!(
                "[embed] http_endpoint_path must start with '/' (got {:?})",
                e.http_endpoint_path
            ),
        );
        check(
            e.model_download_url.starts_with("https://")
                || e.model_download_url.starts_with("http://"),
            format!(
                "[embed] model_download_url must be an http(s) URL (got {:?})",
                e.model_download_url
            ),
        );
        check(
            !e.model_download_files.is_empty()
                && e.model_download_files.iter().all(|f| {
                    !f.is_empty() && !f.ends_with('/') && !f.starts_with('/') && !f.contains("..")
                }),
            "[embed] model_download_files must list relative file paths (no '..', no leading or \
             trailing '/')"
                .into(),
        );
        match (&e.backend, e.embed_url.as_deref()) {
            (EmbedBackend::Ollama, None | Some("")) => check(
                false,
                "[embed] backend = \"ollama\" needs [embed] embed_url \
                 (e.g. \"http://ollama.example:11434\")"
                    .into(),
            ),
            (_, Some(url)) if !url.is_empty() => check(
                url.starts_with("http://"),
                format!("[embed] embed_url must start with http:// (got {url:?})"),
            ),
            _ => {}
        }

        let i = &self.index;
        check(
            i.chunk_size > 0,
            "[index] chunk_size must be greater than 0".into(),
        );
        check(
            i.chunk_overlap < i.chunk_size,
            format!(
                "[index] chunk_overlap ({}) must be smaller than [index] chunk_size ({})",
                i.chunk_overlap, i.chunk_size
            ),
        );
        check(
            i.max_file_size >= i.min_file_size,
            format!(
                "[index] max_file_size ({}) must be at least [index] min_file_size ({})",
                i.max_file_size, i.min_file_size
            ),
        );
        check(
            !i.include_paths.is_empty(),
            "[index] include_paths must name at least one path".into(),
        );
        for p in &i.include_paths {
            check(
                p.is_absolute(),
                format!(
                    "[index] include_paths entry {} must be absolute",
                    p.display()
                ),
            );
        }
        check(
            i.boundary_patterns.iter().all(|p| !p.trim().is_empty()),
            "[index] boundary_patterns must not contain blank entries (they match every line)"
                .into(),
        );

        let s = &self.search;
        check(
            s.default_limit > 0,
            "[search] default_limit must be greater than 0".into(),
        );
        check(
            s.path_boost.is_finite() && s.path_boost >= 0.0,
            format!(
                "[search] path_boost must be 0 or more (got {})",
                s.path_boost
            ),
        );
        check(
            s.max_queries > 0,
            "[search] max_queries must be greater than 0".into(),
        );
        check(
            s.min_score.is_finite() && (-1.0..=1.0).contains(&s.min_score),
            format!(
                "[search] min_score must be between -1 and 1 (got {})",
                s.min_score
            ),
        );

        check(
            self.database.db_path.is_absolute(),
            format!(
                "[database] db_path {} must be absolute",
                self.database.db_path.display()
            ),
        );

        let w = &self.watch;
        check(
            w.debounce_ms > 0,
            "[watch] debounce_ms must be greater than 0".into(),
        );
        check(
            w.max_wait_ms >= w.debounce_ms,
            format!(
                "[watch] max_wait_ms ({}) must be at least [watch] debounce_ms ({})",
                w.max_wait_ms, w.debounce_ms
            ),
        );
        check(
            w.retry_max_ms >= w.debounce_ms,
            format!(
                "[watch] retry_max_ms ({}) must be at least [watch] debounce_ms ({})",
                w.retry_max_ms, w.debounce_ms
            ),
        );
        check(
            w.max_pending > 0,
            "[watch] max_pending must be greater than 0".into(),
        );

        let d = &self.daemon;
        check(
            d.socket.is_absolute(),
            format!("[daemon] socket {} must be absolute", d.socket.display()),
        );
        for (key, value) in [
            ("max_request_bytes", d.max_request_bytes as u64),
            ("max_texts_per_request", d.max_texts_per_request as u64),
            ("io_timeout_ms", d.io_timeout_ms),
            ("reply_timeout_ms", d.reply_timeout_ms),
            ("max_connections", d.max_connections as u64),
            ("accept_retry_ms", d.accept_retry_ms),
        ] {
            check(
                positive(value),
                format!("[daemon] {key} must be greater than 0"),
            );
        }
        check(
            d.reply_timeout_ms >= d.io_timeout_ms,
            format!(
                "[daemon] reply_timeout_ms ({}) must be at least [daemon] io_timeout_ms ({})",
                d.reply_timeout_ms, d.io_timeout_ms
            ),
        );

        check(
            self.database.busy_timeout_ms > 0,
            "[database] busy_timeout_ms must be greater than 0".into(),
        );
        check(
            self.database.migration_wait_ms >= self.database.busy_timeout_ms,
            format!(
                "[database] migration_wait_ms ({}) must be at least [database] busy_timeout_ms ({})",
                self.database.migration_wait_ms, self.database.busy_timeout_ms
            ),
        );

        if problems.is_empty() {
            return Ok(());
        }
        let problems: Vec<String> = problems
            .iter()
            .map(|p| format!("{p}{}", self.origin_note(p)))
            .collect();
        bail!("invalid configuration:\n  {}", problems.join("\n  "))
    }

    /// `" (set in <layer>)"` naming the configuration files that set the keys
    /// a validation problem names (`[section] key`), or `""` when they all
    /// come from the compiled-in defaults.
    fn origin_note(&self, problem: &str) -> String {
        let mut layers: Vec<String> = Vec::new();
        let named = keys_named(problem);
        let set_by = self
            .provenance
            .iter()
            .filter(|o| named.contains(&o.key))
            .flat_map(|o| &o.set_by)
            .filter(|l| **l != ConfigLayer::Defaults);
        for layer in set_by {
            let layer = layer.to_string();
            if !layers.contains(&layer) {
                layers.push(layer);
            }
        }
        if layers.is_empty() {
            String::new()
        } else {
            format!(" (set in {})", layers.join(", "))
        }
    }

    // -----------------------------------------------------------------------
    // Derived settings
    // -----------------------------------------------------------------------

    /// Options for [`vec_embed::Embedder::load`].
    pub fn onnx_options(&self) -> OnnxOptions {
        OnnxOptions {
            max_tokens: self.embed.max_tokens,
            batch_size: self.embed.onnx_batch_size,
        }
    }

    /// Options for [`vec_embed::Embedder::http`]; an error when `embed_url` is unset.
    pub fn http_options(&self) -> Result<HttpOptions> {
        let url = self
            .embed
            .embed_url
            .as_deref()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "[embed] embed_url is not set; the HTTP backend needs the server URL \
                     (e.g. \"http://ollama.example:11434\")"
                )
            })?;
        Ok(HttpOptions {
            url: url.to_owned(),
            model: self.embed.model.clone(),
            endpoint_path: self.embed.http_endpoint_path.clone(),
            connect_timeout_ms: self.embed.http_connect_timeout_ms,
            read_timeout_ms: self.embed.http_read_timeout_ms,
            concurrency: self.embed.http_concurrency,
            max_input_bytes: self.embed.http_max_input_bytes,
            error_preview_chars: self.embed.http_error_preview_chars,
            batch_size: self.embed.batch_size,
        })
    }

    /// Find the ONNX file of the configured model.
    ///
    /// An absolute `[embed] model` is used as is (it must exist). A short name
    /// is looked up in each `model_search_path` directory, in order, as
    /// `{dir}/{name}.onnx`, then `{dir}/{name}/{file}` for each `model_files` entry.
    pub fn resolve_model_path(&self) -> Result<PathBuf> {
        let name = &self.embed.model;
        let as_path = Path::new(name);
        if as_path.is_absolute() {
            if as_path.is_file() {
                return Ok(as_path.to_path_buf());
            }
            bail!("[embed] model file not found: {}", as_path.display());
        }

        for dir in &self.embed.model_search_path {
            let candidates = std::iter::once(dir.join(format!("{name}.onnx"))).chain(
                self.embed
                    .model_files
                    .iter()
                    .map(|f| dir.join(name).join(f)),
            );
            for candidate in candidates {
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }

        let searched: Vec<String> = self
            .embed
            .model_search_path
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        bail!(
            "model '{name}' not found in [embed] model_search_path ({}).\n\
             Install the model package (vec-model-base), run `vec model download` for \
             instructions, or set [embed] model to the absolute path of an .onnx file.",
            searched.join(", ")
        )
    }
}

// ---------------------------------------------------------------------------
// Merging helpers
// ---------------------------------------------------------------------------

/// Parse one layer's TOML text.
fn parse_layer(text: &str) -> Result<Table> {
    Ok(toml::from_str::<Table>(text)?)
}

/// Read and parse a layer file; `None` when the file does not exist.
fn read_optional_layer(path: &Path) -> Result<Option<Table>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_layer(&text)
            .with_context(|| format!("parsing config file {}", path.display()))
            .map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading config file {}", path.display())),
    }
}

/// Apply `layer` on top of `merged`, recording origins when asked.
fn merge_layer(
    merged: &mut Table,
    layer: &Table,
    source: &ConfigLayer,
    mut origins: Option<&mut BTreeMap<String, KeyOrigin>>,
) -> Result<()> {
    for (section, value) in layer {
        let Value::Table(entries) = value else {
            bail!("key `{section}` must be inside a [section]");
        };
        let Value::Table(target) = merged
            .entry(section.clone())
            .or_insert_with(|| Value::Table(Table::new()))
        else {
            bail!("[{section}] is not a table");
        };
        for (key, new) in entries {
            let appends = APPEND_KEYS.iter().any(|(s, k)| s == section && k == key);
            let value = if appends {
                append_list(target.get(key), new).with_context(|| format!("[{section}] {key}"))?
            } else {
                new.clone()
            };
            target.insert(key.clone(), value.clone());
            if let Some(origins) = origins.as_deref_mut() {
                let full = format!("{section}.{key}");
                let entry = origins.entry(full.clone()).or_insert_with(|| KeyOrigin {
                    key: full,
                    value: value.clone(),
                    set_by: Vec::new(),
                });
                entry.value = value;
                if !appends {
                    entry.set_by.clear();
                }
                entry.set_by.push(source.clone());
            }
        }
    }
    Ok(())
}

/// Move the deprecated keys of one layer to their new location
/// ([`DEPRECATED_KEYS`]); one warning per key found. When the layer also sets
/// the new key, that one wins and the old key is dropped.
fn apply_deprecations(table: &mut Table, layer: &ConfigLayer) -> Vec<String> {
    let mut warnings = Vec::new();
    for (section, old, new_section, new_key) in DEPRECATED_KEYS {
        let Some(value) = table
            .get_mut(*section)
            .and_then(Value::as_table_mut)
            .and_then(|t| t.remove(*old))
        else {
            continue;
        };
        let target = table
            .entry((*new_section).to_owned())
            .or_insert_with(|| Value::Table(Table::new()));
        let Some(target) = target.as_table_mut() else {
            // Not a table: deserialization reports it, naming the section.
            continue;
        };
        if target.contains_key(*new_key) {
            warnings.push(format!(
                "{layer}: [{section}] {old} is deprecated and ignored, because [{new_section}] \
                 {new_key} is set too; remove [{section}] {old}"
            ));
        } else {
            target.insert((*new_key).to_owned(), value);
            warnings.push(format!(
                "{layer}: [{section}] {old} is deprecated (it will be removed in the next \
                 release); its value is used as [{new_section}] {new_key}, write it there instead"
            ));
        }
    }
    warnings
}

/// The `section.key` of every `[section] key` mentioned in `text`.
fn keys_named(text: &str) -> Vec<String> {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut keys = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find(']') else { break };
        let section = &rest[..close];
        let after = &rest[close + 1..];
        if section.is_empty() || !section.chars().all(is_word) {
            continue;
        }
        let Some(after) = after.strip_prefix(' ') else {
            continue;
        };
        let end = after.find(|c: char| !is_word(c)).unwrap_or(after.len());
        if end > 0 {
            keys.push(format!("{section}.{}", &after[..end]));
        }
    }
    keys
}

/// Append the string list `new` to `old`: `"!x"` removes `x`, other entries are
/// added unless already present.
fn append_list(old: Option<&Value>, new: &Value) -> Result<Value> {
    let strings = |v: &Value| -> Result<Vec<String>> {
        v.as_array()
            .ok_or_else(|| anyhow!("must be a list of strings"))?
            .iter()
            .map(|e| {
                e.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("must be a list of strings (found {e})"))
            })
            .collect()
    };
    let mut list = old.map(strings).transpose()?.unwrap_or_default();
    for entry in strings(new)? {
        if let Some(removed) = entry.strip_prefix('!') {
            list.retain(|e| e != removed);
        } else if !list.contains(&entry) {
            list.push(entry);
        }
    }
    Ok(Value::Array(list.into_iter().map(Value::String).collect()))
}

/// Deserialize a merged table; errors name the offending key.
fn deserialize(table: Table) -> Result<Config> {
    serde_path_to_error::deserialize(Value::Table(table)).map_err(|e| {
        let path = e.path().to_string();
        let mut parts = path.split('.').filter(|p| !p.is_empty() && *p != "?");
        let at = match (parts.next(), parts.next()) {
            (Some(section), Some(key)) => format!("[{section}] {key}"),
            (Some(section), None) => format!("[{section}]"),
            _ => "top level".to_owned(),
        };
        anyhow!("{at}: {}", e.into_inner())
    })
}

/// Expand a leading `~` to the home directory. Left unchanged when there is no
/// home directory; validation then rejects the relative path by key.
fn expand_tilde(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    match dirs::home_dir() {
        Some(home) => home.join(rest),
        None => path.to_path_buf(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults_table() -> Table {
        parse_layer(DEFAULTS_TOML).unwrap()
    }

    fn err_text(r: Result<Config>) -> String {
        format!("{:#}", r.expect_err("expected an error"))
    }

    #[test]
    fn defaults_file_is_complete_and_valid() {
        // Deserializing proves every field has a default and no default key is
        // unknown; validate() proves the defaults are in range.
        let cfg = Config::defaults().unwrap();
        assert_eq!(cfg.layers(), &[ConfigLayer::Defaults]);
        assert_eq!(cfg.embed.embed_url, None);
        assert!(
            cfg.provenance()
                .iter()
                .all(|o| o.set_by == [ConfigLayer::Defaults])
        );
    }

    #[test]
    fn every_default_key_is_in_provenance() {
        let cfg = Config::defaults().unwrap();
        let keys: usize = defaults_table()
            .values()
            .map(|v| v.as_table().map_or(0, Table::len))
            .sum();
        assert_eq!(cfg.provenance().len(), keys);
        let model = cfg
            .provenance()
            .iter()
            .find(|o| o.key == "embed.model")
            .unwrap();
        assert_eq!(model.value.as_str(), Some(cfg.embed.model.as_str()));
    }

    #[test]
    fn locations_parse() {
        assert!(Config::system_config_path().unwrap().is_absolute());
        if let Some(user) = Config::user_config_path().unwrap() {
            assert!(user.is_absolute());
        }
    }

    #[test]
    fn layers_override_in_order_and_extra_skips_user() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path().join("vec.conf");
        let user = dir.path().join("config.toml");
        let extra = dir.path().join("extra.toml");
        std::fs::write(&sys, "[embed]\nbatch_size = 8\nmax_tokens = 100\n").unwrap();
        std::fs::write(&user, "[embed]\nbatch_size = 24\n").unwrap();
        std::fs::write(&extra, "[embed]\nmax_tokens = 300\n").unwrap();

        let cfg = Config::load_layers(&sys, Some(&user), None).unwrap();
        assert_eq!((cfg.embed.batch_size, cfg.embed.max_tokens), (24, 100));
        assert_eq!(
            cfg.layers(),
            &[
                ConfigLayer::Defaults,
                ConfigLayer::System(sys.clone()),
                ConfigLayer::User(user.clone())
            ]
        );
        let origin = |cfg: &Config, key: &str| {
            cfg.provenance()
                .iter()
                .find(|o| o.key == key)
                .unwrap()
                .set_by
                .clone()
        };
        assert_eq!(
            origin(&cfg, "embed.batch_size"),
            [ConfigLayer::User(user.clone())]
        );
        assert_eq!(
            origin(&cfg, "embed.max_tokens"),
            [ConfigLayer::System(sys.clone())]
        );
        assert_eq!(origin(&cfg, "index.chunk_size"), [ConfigLayer::Defaults]);

        let cfg = Config::load_layers(&sys, None, Some(&extra)).unwrap();
        assert_eq!((cfg.embed.batch_size, cfg.embed.max_tokens), (8, 300));
    }

    #[test]
    fn missing_optional_layers_are_skipped_but_missing_extra_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_layers(
            &dir.path().join("none.conf"),
            Some(&dir.path().join("none.toml")),
            None,
        )
        .unwrap();
        assert_eq!(cfg.layers(), &[ConfigLayer::Defaults]);
        assert!(
            Config::load_layers(
                &dir.path().join("none.conf"),
                None,
                Some(&dir.path().join("missing.toml"))
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_keys_are_errors_naming_key_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path().join("vec.conf");
        std::fs::write(&sys, "[embed]\nbach_size = 8\n").unwrap();
        let err = err_text(Config::load_layers(&sys, None, None));
        assert!(err.contains("bach_size"), "{err}");
        assert!(err.contains("[embed]"), "{err}");
        assert!(err.contains(&sys.display().to_string()), "{err}");

        let err = err_text(Config::from_toml("[serch]\ndefault_limit = 5\n"));
        assert!(err.contains("serch"), "{err}");
        let err = err_text(Config::from_toml("model = \"x\"\n"));
        assert!(err.contains("inside a [section]"), "{err}");
    }

    #[test]
    fn wrong_types_name_the_key() {
        let err = err_text(Config::from_toml("[index]\nchunk_size = \"forty\"\n"));
        assert!(err.contains("[index] chunk_size"), "{err}");
        let err = err_text(Config::from_toml("[embed]\nbackend = \"tensorflow\"\n"));
        assert!(err.contains("[embed] backend"), "{err}");
    }

    /// Regression: `[embed] daemon_socket` was a hard error after the
    /// upgrade; for one release it is mapped to `[daemon] socket` with a
    /// warning.
    #[test]
    fn deprecated_daemon_socket_is_mapped_with_a_warning() {
        let cfg = Config::from_toml("[embed]\ndaemon_socket = \"/run/x.sock\"\n").unwrap();
        assert_eq!(cfg.daemon.socket, PathBuf::from("/run/x.sock"));
        assert_eq!(cfg.warnings().len(), 1);
        let w = &cfg.warnings()[0];
        assert!(
            w.contains("deprecated") && w.contains("[daemon] socket"),
            "{w}"
        );
        let origin = cfg
            .provenance()
            .iter()
            .find(|o| o.key == "daemon.socket")
            .unwrap();
        assert_eq!(origin.set_by, [ConfigLayer::Inline]);

        // The new key wins when a layer sets both.
        let cfg = Config::from_toml(
            "[embed]\ndaemon_socket = \"/run/old.sock\"\n[daemon]\nsocket = \"/run/new.sock\"\n",
        )
        .unwrap();
        assert_eq!(cfg.daemon.socket, PathBuf::from("/run/new.sock"));
        assert!(
            cfg.warnings()[0].contains("ignored"),
            "{:?}",
            cfg.warnings()
        );

        // Still validated like the new key.
        let err = err_text(Config::from_toml("[embed]\ndaemon_socket = \"x.sock\"\n"));
        assert!(err.contains("[daemon] socket"), "{err}");
        assert!(Config::defaults().unwrap().warnings().is_empty());
    }

    #[test]
    fn validation_errors_name_the_file_that_set_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path().join("vec.conf");
        let user = dir.path().join("config.toml");
        std::fs::write(&sys, "[index]\nchunk_size = 20\n").unwrap();
        std::fs::write(&user, "[index]\nchunk_overlap = 30\n").unwrap();
        let cfg = Config::load_layers(&sys, Some(&user), None).unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("[index] chunk_overlap"), "{err}");
        assert!(err.contains(&sys.display().to_string()), "{err}");
        assert!(err.contains(&user.display().to_string()), "{err}");

        // An inline layer is named too; keys left at their defaults are not.
        let err = err_text(Config::from_toml("[search]\ndefault_limit = 0\n"));
        assert!(err.contains("(set in inline config)"), "{err}");
        assert_eq!(
            Config::defaults()
                .unwrap()
                .origin_note("[index] chunk_size"),
            ""
        );
    }

    #[test]
    fn keys_named_finds_section_key_pairs() {
        assert_eq!(
            keys_named("[index] chunk_overlap (40) must be smaller than [index] chunk_size (4)"),
            ["index.chunk_overlap", "index.chunk_size"]
        );
        assert!(keys_named("[not a key] x [] y [a]").is_empty());
    }

    #[test]
    fn exclude_lists_append_and_bang_removes() {
        let base = Config::defaults().unwrap();
        assert!(base.index.exclude_dirs.iter().any(|d| d == "build"));
        let cfg = Config::from_toml(
            "[index]\nexclude_dirs = [\"vendor\", \"!build\", \".git\"]\nexclude_files = [\"!*.lock\"]\n",
        )
        .unwrap();
        let dirs = &cfg.index.exclude_dirs;
        assert!(dirs.iter().any(|d| d == "vendor"));
        assert!(
            !dirs.iter().any(|d| d == "build"),
            "!build removes the default"
        );
        assert_eq!(
            dirs.iter().filter(|d| *d == ".git").count(),
            1,
            "no duplicates"
        );
        assert_eq!(dirs.len(), base.index.exclude_dirs.len());
        assert!(!cfg.index.exclude_files.iter().any(|f| f == "*.lock"));
        let origin = cfg
            .provenance()
            .iter()
            .find(|o| o.key == "index.exclude_dirs")
            .unwrap();
        assert_eq!(origin.set_by, [ConfigLayer::Defaults, ConfigLayer::Inline]);
    }

    #[test]
    fn other_lists_replace() {
        let cfg = Config::from_toml("[index]\ninclude_paths = [\"/srv\"]\n").unwrap();
        assert_eq!(cfg.index.include_paths, [PathBuf::from("/srv")]);
    }

    #[test]
    fn validation_names_keys() {
        let cases = [
            ("[index]\nchunk_size = 0\n", "[index] chunk_size"),
            (
                "[index]\nchunk_overlap = 40\n",
                "[index] chunk_overlap (40)",
            ),
            ("[embed]\nbatch_size = 0\n", "[embed] batch_size"),
            ("[embed]\nonnx_batch_size = 0\n", "[embed] onnx_batch_size"),
            (
                "[embed]\nmodel_search_path = []\n",
                "[embed] model_search_path",
            ),
            (
                "[embed]\nmodel_search_path = [\"models\"]\n",
                "must be absolute",
            ),
            ("[index]\nmax_file_size = 10\n", "[index] max_file_size"),
            (
                "[index]\ninclude_paths = [\"src\"]\n",
                "[index] include_paths",
            ),
            (
                "[index]\nboundary_patterns = [\" \"]\n",
                "[index] boundary_patterns",
            ),
            ("[database]\ndb_path = \"vec.db\"\n", "[database] db_path"),
            ("[search]\nmin_score = 2.0\n", "[search] min_score"),
            ("[search]\ndefault_limit = 0\n", "[search] default_limit"),
            ("[watch]\nmax_wait_ms = 1\n", "[watch] max_wait_ms"),
            ("[embed]\nbackend = \"ollama\"\n", "[embed] embed_url"),
            ("[embed]\nembed_url = \"https://x.example\"\n", "http://"),
            ("[daemon]\nsocket = \"embed.sock\"\n", "[daemon] socket"),
            (
                "[daemon]\nmax_connections = 0\n",
                "[daemon] max_connections",
            ),
            (
                "[daemon]\nreply_timeout_ms = 0\n",
                "[daemon] reply_timeout_ms",
            ),
            (
                "[daemon]\nio_timeout_ms = 2000\nreply_timeout_ms = 1000\n",
                "at least [daemon] io_timeout_ms",
            ),
            (
                "[daemon]\nmax_texts_per_request = 0\n",
                "[daemon] max_texts_per_request",
            ),
            (
                "[embed]\nhttp_error_preview_chars = 0\n",
                "[embed] http_error_preview_chars",
            ),
            (
                "[database]\nbusy_timeout_ms = 0\n",
                "[database] busy_timeout_ms",
            ),
            (
                "[database]\nbusy_timeout_ms = 2000\nmigration_wait_ms = 1000\n",
                "[database] migration_wait_ms",
            ),
        ];
        for (toml, expected) in cases {
            let err = err_text(Config::from_toml(toml));
            assert!(err.contains(expected), "{toml:?} → {err}");
        }
    }

    #[test]
    fn validation_reports_every_problem() {
        let err = err_text(Config::from_toml(
            "[index]\nchunk_size = 0\n[search]\ndefault_limit = 0\n",
        ));
        assert!(
            err.contains("chunk_size") && err.contains("default_limit"),
            "{err}"
        );
    }

    #[test]
    fn socket_mode_is_octal() {
        let cfg = Config::from_toml("[daemon]\nsocket_mode = \"0660\"\n").unwrap();
        assert_eq!(cfg.daemon.socket_mode.bits(), 0o660);
        assert_eq!(cfg.daemon.socket_mode.to_string(), "0660");
        let err = err_text(Config::from_toml("[daemon]\nsocket_mode = \"0999\"\n"));
        assert!(err.contains("[daemon] socket_mode"), "{err}");
        assert!(Config::from_toml("[daemon]\nsocket_mode = \"01777\"\n").is_err());
    }

    #[test]
    fn tilde_is_expanded_in_paths() {
        let Some(home) = dirs::home_dir() else { return };
        let cfg = Config::from_toml(
            "[database]\ndb_path = \"~/.local/share/vec/vec.db\"\n[index]\ninclude_paths = [\"~\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.database.db_path, home.join(".local/share/vec/vec.db"));
        assert_eq!(cfg.index.include_paths, [home]);
        assert_eq!(expand_tilde(Path::new("/etc/x")), PathBuf::from("/etc/x"));
        assert_eq!(expand_tilde(Path::new("~user/x")), PathBuf::from("~user/x"));
    }

    #[test]
    fn http_options_need_url() {
        let cfg = Config::defaults().unwrap();
        assert!(cfg.http_options().is_err());
        let cfg = Config::from_toml(
            "[embed]\nbackend = \"ollama\"\nembed_url = \"http://ollama.example:11434\"\nmodel = \"m\"\n",
        )
        .unwrap();
        let opts = cfg.http_options().unwrap();
        assert_eq!(opts.url, "http://ollama.example:11434");
        assert_eq!(opts.model, "m");
        assert_eq!(opts.batch_size, cfg.embed.batch_size);
        assert_eq!(cfg.onnx_options().batch_size, cfg.embed.onnx_batch_size);
    }

    fn model_cfg(dir: &Path, model: &str) -> Config {
        let mut cfg = Config::defaults().unwrap();
        cfg.embed.model_search_path = vec![dir.to_path_buf()];
        cfg.embed.model = model.to_owned();
        cfg
    }

    #[test]
    fn resolve_model_path_search_order() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = model_cfg(dir.path(), "m");
        let err = cfg.resolve_model_path().unwrap_err().to_string();
        assert!(err.contains("vec-model-base"), "{err}");

        // {name}/{model_files...}: earlier entries win.
        let sub = dir.path().join("m");
        std::fs::create_dir(&sub).unwrap();
        let files: Vec<PathBuf> = cfg.embed.model_files.iter().map(|f| sub.join(f)).collect();
        for f in &files {
            std::fs::write(f, b"x").unwrap();
        }
        assert_eq!(cfg.resolve_model_path().unwrap(), files[0]);

        // {name}.onnx beats the directory form.
        let flat = dir.path().join("m.onnx");
        std::fs::write(&flat, b"x").unwrap();
        assert_eq!(cfg.resolve_model_path().unwrap(), flat);

        // An absolute model path is used as is, and must exist.
        let abs = model_cfg(dir.path(), &flat.display().to_string());
        assert_eq!(abs.resolve_model_path().unwrap(), flat);
        let gone = model_cfg(
            dir.path(),
            &dir.path().join("gone.onnx").display().to_string(),
        );
        assert!(gone.resolve_model_path().is_err());
    }
}

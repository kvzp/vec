//! vec-embed — text embedding backends for vec.
//!
//! [`Embedder`] wraps one of three backends, all returning L2-normalised vectors:
//!
//! - **ONNX** ([`Embedder::load`]): a local model run with `tract-onnx` (pure Rust)
//!   and the HuggingFace `tokenizer.json` that sits next to the model file.
//! - **HTTP** ([`Embedder::http`]): an Ollama-compatible embedding endpoint
//!   (`POST {"model", "input": [..]}` → `{"embeddings": [[..], ..]}`).
//! - **Stub** ([`Embedder::stub`]): deterministic pseudo-random unit vectors, for tests.
//!
//! [`Embedder::identity`] names the model (configured name and dimension) so the
//! store can refuse to mix vectors from different models.
//! [`Embedder::identity_with_sha`] adds the sha256 of the ONNX file. That hash is
//! computed only on request, because query paths must not hash a large model file
//! on every call. Writers (index, watch) record it.
//!
//! ## ONNX inputs, outputs and batching
//!
//! Inputs are matched by name (`input_ids`, `attention_mask`, optional
//! `token_type_ids`), never by position. The output is `sentence_embedding` when
//! the graph has one (already pooled), else `last_hidden_state` /
//! `token_embeddings`, mean-pooled over the attention mask.
//!
//! The graph is compiled once with symbolic batch and sequence dimensions.
//! [`Embedder::embed_batch`] sorts the texts by token count, groups them into
//! batches of [`OnnxOptions::batch_size`] texts, pads each batch only to its own
//! longest text, and runs the batches in parallel (rayon). With `batch_size = 1`
//! nothing is padded and every text gets exactly the vector it would get alone.
//!
//! Dynamically quantized (int8) models compute their quantization scale over the
//! whole input tensor, padding and the other rows included, so with
//! `batch_size > 1` their vectors move slightly (cosine about 0.98 to the
//! unbatched vector for gte-multilingual-base int8). Float models are unaffected
//! beyond rounding.

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tract_onnx::prelude::*;

/// The compiled, runnable tract plan.
type TractPlan = TypedSimplePlan<TypedModel>;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// What produced a vector: the model name, the vector dimension and, for an
/// ONNX model when known, the sha256 of the model file.
///
/// Vectors from two identities are comparable only when names and dimensions
/// are equal and, when both hashes are known, the hashes are equal too
/// (see [`ModelIdentity::is_compatible_with`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelIdentity {
    /// The configured model name (ONNX short name or path, or the HTTP model name).
    pub name: String,
    /// Embedding dimension.
    pub dim: usize,
    /// Lowercase hex sha256 of the ONNX file, when known.
    pub sha256: Option<String>,
}

impl ModelIdentity {
    /// An identity without a file hash.
    pub fn new(name: impl Into<String>, dim: usize) -> Self {
        Self {
            name: name.into(),
            dim,
            sha256: None,
        }
    }

    /// `true` when vectors of `self` and `other` may be compared: same name,
    /// same dimension, and the same sha256 when both are known.
    #[must_use]
    pub fn is_compatible_with(&self, other: &ModelIdentity) -> bool {
        let sha_ok = match (&self.sha256, &other.sha256) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        self.name == other.name && self.dim == other.dim && sha_ok
    }
}

impl fmt::Display for ModelIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (dim {}", self.name, self.dim)?;
        if let Some(sha) = &self.sha256 {
            write!(f, ", sha256 {sha}")?;
        }
        f.write_str(")")
    }
}

/// Settings of the local ONNX backend (from the `[embed]` config section).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnnxOptions {
    /// Token limit per text; longer texts are truncated.
    pub max_tokens: usize,
    /// Texts per forward pass. 1 = no padding (see the module docs).
    pub batch_size: usize,
}

/// Settings of the HTTP backend (from the `[embed]` config section).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpOptions {
    /// Base URL, `http://host[:port][/base/path]`.
    pub url: String,
    /// Model name sent with every request.
    pub model: String,
    /// Endpoint path appended to the URL's own path (e.g. `/api/embed`).
    pub endpoint_path: String,
    /// TCP connect timeout (milliseconds).
    pub connect_timeout_ms: u64,
    /// Read/write timeout per request (milliseconds).
    pub read_timeout_ms: u64,
    /// Requests in flight at once.
    pub concurrency: usize,
    /// Texts longer than this many bytes are cut (at a character boundary)
    /// before sending.
    pub max_input_bytes: usize,
    /// Characters of an error response body quoted in error messages.
    pub error_preview_chars: usize,
    /// Texts per request.
    pub batch_size: usize,
}

/// The embedding engine.
pub struct Embedder {
    identity: ModelIdentity,
    backend: Backend,
}

impl fmt::Debug for Embedder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Embedder")
            .field("identity", &self.identity)
            .field("backend", &self.backend_name())
            .finish()
    }
}

enum Backend {
    Onnx(Box<OnnxModel>),
    Http(HttpClient),
    Stub,
}

// ---------------------------------------------------------------------------
// impl Embedder
// ---------------------------------------------------------------------------

impl Embedder {
    /// Load the ONNX model at `model_path`; `{dir}/tokenizer.json` must exist
    /// next to it. `name` is the configured model name, recorded in the identity.
    pub fn load(model_path: &Path, name: &str, opts: &OnnxOptions) -> Result<Self> {
        let model = OnnxModel::load(model_path, opts)?;
        Ok(Self {
            identity: ModelIdentity::new(name, model.dim),
            backend: Backend::Onnx(Box::new(model)),
        })
    }

    /// Connect to an Ollama-compatible HTTP endpoint. One probe request learns
    /// the embedding dimension, so an unreachable endpoint fails here.
    pub fn http(opts: &HttpOptions) -> Result<Self> {
        let client = HttpClient::new(opts)?;
        let probe = client
            .request(&[PROBE_TEXT])
            .with_context(|| format!("probing embed endpoint {}", client.describe()))?;
        let dim = probe
            .first()
            .map(Vec::len)
            .filter(|&d| d > 0)
            .ok_or_else(|| anyhow!("embed endpoint {} returned no vector", client.describe()))?;
        Ok(Self {
            identity: ModelIdentity::new(opts.model.clone(), dim),
            backend: Backend::Http(client),
        })
    }

    /// A stub embedder: deterministic unit vectors of length `dim` derived from a
    /// hash of the text. Not semantically meaningful; for tests and explicit use.
    pub fn stub(dim: usize) -> Self {
        Self {
            identity: ModelIdentity::new(STUB_MODEL_NAME, dim),
            backend: Backend::Stub,
        }
    }

    /// The model identity, without the file hash (see [`Embedder::identity_with_sha`]).
    pub fn identity(&self) -> &ModelIdentity {
        &self.identity
    }

    /// Embedding dimension.
    pub fn dim(&self) -> usize {
        self.identity.dim
    }

    /// The ONNX model file; `None` for the HTTP and stub backends.
    #[must_use]
    pub fn model_file(&self) -> Option<&Path> {
        match &self.backend {
            Backend::Onnx(m) => Some(&m.path),
            Backend::Http(_) | Backend::Stub => None,
        }
    }

    /// Short backend name for messages: `onnx`, `http` or `stub`.
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            Backend::Onnx(_) => "onnx",
            Backend::Http(_) => "http",
            Backend::Stub => "stub",
        }
    }

    /// sha256 of the ONNX model file (lowercase hex), computed on the first call
    /// and cached. `None` for the HTTP and stub backends.
    pub fn model_sha256(&self) -> Result<Option<String>> {
        match &self.backend {
            Backend::Onnx(m) => m.sha256().map(|s| Some(s.to_owned())),
            Backend::Http(_) | Backend::Stub => Ok(None),
        }
    }

    /// The identity including the model file hash (hashes the file once).
    pub fn identity_with_sha(&self) -> Result<ModelIdentity> {
        let mut id = self.identity.clone();
        id.sha256 = self.model_sha256()?;
        Ok(id)
    }

    /// Lowercase hex sha256 of the file at `path`, streamed (constant memory).
    pub fn file_sha256(path: &Path) -> Result<String> {
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("opening model file {}", path.display()))?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)
            .with_context(|| format!("reading model file {}", path.display()))?;
        Ok(hex::encode(hasher.finalize()))
    }

    /// Embed `texts` and return one unit-length vector of [`Embedder::dim`]
    /// floats per text, in input order.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = match &self.backend {
            Backend::Stub => texts.iter().map(|t| stub_embed(t, self.dim())).collect(),
            Backend::Onnx(m) => m.embed(texts)?,
            Backend::Http(c) => c.embed(texts)?,
        };
        if vectors.len() != texts.len() {
            bail!(
                "{} backend returned {} vectors for {} texts",
                self.backend_name(),
                vectors.len(),
                texts.len()
            );
        }
        if let Some(v) = vectors.iter().find(|v| v.len() != self.dim()) {
            bail!(
                "{} backend returned a vector of dimension {}, expected {}",
                self.backend_name(),
                v.len(),
                self.dim()
            );
        }
        Ok(vectors)
    }

    /// Convenience wrapper: embed a single text.
    pub fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_batch(&[text])?
            .pop()
            .ok_or_else(|| anyhow!("embedding backend returned no vector"))
    }
}

/// Identity name of the stub backend.
const STUB_MODEL_NAME: &str = "stub";

/// A short text used to check that a backend works: [`Embedder::http`] embeds
/// it to learn the dimension, and the indexer embeds it to tell a failing
/// file from a failing backend. Not a tunable: any short text does.
pub const PROBE_TEXT: &str = "dimension probe";

// ---------------------------------------------------------------------------
// ONNX backend
// ---------------------------------------------------------------------------

/// A model input, identified by its ONNX name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputRole {
    Ids,
    Mask,
    TypeIds,
}

impl InputRole {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "input_ids" => Some(Self::Ids),
            "attention_mask" => Some(Self::Mask),
            "token_type_ids" => Some(Self::TypeIds),
            _ => None,
        }
    }
}

/// How the chosen model output becomes one vector per text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pooling {
    /// `[batch, dim]`, already pooled (`sentence_embedding`).
    Pooled,
    /// `[batch, seq, dim]`, mean-pooled over the attention mask.
    Mean,
}

/// Output names, best first, with how each is pooled.
///
/// These are names of the ONNX export format of sentence-embedding models
/// (sentence-transformers / Optimum exports), not tunables: a model either
/// has one of these outputs or cannot be used. They are fixed by the model
/// format, like the input names in [`InputRole::from_name`], so they are not
/// configuration.
const OUTPUTS: &[(&str, Pooling)] = &[
    ("sentence_embedding", Pooling::Pooled),
    ("last_hidden_state", Pooling::Mean),
    ("token_embeddings", Pooling::Mean),
];

/// Pad tokens tried, in order, when `tokenizer.json` has no padding
/// configuration: the spellings of the HuggingFace tokenizer formats
/// (SentencePiece `<pad>`, WordPiece `[PAD]`). Model-format knowledge, not a
/// tunable (see [`OUTPUTS`]).
const PAD_TOKENS: &[&str] = &["<pad>", "[PAD]"];

struct OnnxModel {
    plan: TractPlan,
    tokenizer: tokenizers::Tokenizer,
    /// Model inputs in graph order.
    inputs: Vec<InputRole>,
    pooling: Pooling,
    /// Pad token id; needed only when `batch_size > 1`.
    pad_id: Option<u32>,
    batch_size: usize,
    dim: usize,
    path: PathBuf,
    sha256: OnceLock<String>,
}

impl OnnxModel {
    fn load(model_path: &Path, opts: &OnnxOptions) -> Result<Self> {
        if opts.max_tokens == 0 {
            bail!("[embed] max_tokens must be greater than 0");
        }
        if opts.batch_size == 0 {
            bail!("[embed] onnx_batch_size must be greater than 0");
        }
        let model_dir = model_path
            .parent()
            .with_context(|| format!("model path has no parent: {}", model_path.display()))?;

        // --- Tokenizer: truncate to max_tokens, never pad (we pad per batch). ---
        let tokenizer_path = model_dir.join("tokenizer.json");
        let mut tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            anyhow!(
                "failed to load tokenizer from {}: {e}",
                tokenizer_path.display()
            )
        })?;
        let pad_id = tokenizer
            .get_padding()
            .map(|p| p.pad_id)
            .or_else(|| PAD_TOKENS.iter().find_map(|t| tokenizer.token_to_id(t)));
        if opts.batch_size > 1 && pad_id.is_none() {
            bail!(
                "the tokenizer at {} defines no pad token, so texts cannot be batched; \
                 set [embed] onnx_batch_size = 1",
                tokenizer_path.display()
            );
        }
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: opts.max_tokens,
                strategy: tokenizers::TruncationStrategy::LongestFirst,
                stride: 0,
                direction: tokenizers::TruncationDirection::Right,
            }))
            .map_err(|e| anyhow!("failed to set tokenizer truncation: {e}"))?;

        // --- Graph ---
        let mut model = read_patched_onnx(model_path)?;

        let batch = model.symbols.sym("B");
        let seq = model.symbols.sym("S");
        let fact = InferenceFact::dt_shape(i64::datum_type(), tvec!(batch.to_dim(), seq.to_dim()));
        let input_outlets = model
            .input_outlets()
            .context("reading model inputs")?
            .to_vec();
        let mut inputs = Vec::with_capacity(input_outlets.len());
        for (i, outlet) in input_outlets.iter().enumerate() {
            let name = model.node(outlet.node).name.clone();
            let role = InputRole::from_name(&name).ok_or_else(|| {
                anyhow!(
                    "unsupported model input '{name}' in {} (expected input_ids, \
                     attention_mask, token_type_ids)",
                    model_path.display()
                )
            })?;
            inputs.push(role);
            model
                .set_input_fact(i, fact.clone())
                .with_context(|| format!("setting input fact for input '{name}'"))?;
        }
        for required in [InputRole::Ids, InputRole::Mask] {
            if !inputs.contains(&required) {
                bail!(
                    "model {} has no {required:?} input (input_ids and attention_mask are required)",
                    model_path.display()
                );
            }
        }

        // Keep only the output we use, so tract does not compute the others.
        let output_outlets = model
            .output_outlets()
            .context("reading model outputs")?
            .to_vec();
        let output_name = |o: &OutletId| {
            model
                .outlet_label(*o)
                .unwrap_or(model.node(o.node).name.as_str())
                .to_owned()
        };
        let (outlet, pooling) = OUTPUTS
            .iter()
            .find_map(|(wanted, pooling)| {
                output_outlets
                    .iter()
                    .find(|o| output_name(o) == *wanted)
                    .map(|o| (*o, *pooling))
            })
            .ok_or_else(|| {
                let found: Vec<String> = output_outlets.iter().map(output_name).collect();
                anyhow!(
                    "model {} has none of the outputs sentence_embedding, last_hidden_state, \
                     token_embeddings (found: {found:?})",
                    model_path.display()
                )
            })?;
        model
            .set_output_outlets(&[outlet])
            .context("selecting the model output")?;

        let typed = model.into_optimized().context("optimizing ONNX model")?;
        let dim = output_dim(&typed, pooling)?;
        let plan = typed
            .into_runnable()
            .context("compiling ONNX model to a runnable plan")?;

        Ok(Self {
            plan,
            tokenizer,
            inputs,
            pooling,
            pad_id,
            batch_size: opts.batch_size,
            dim,
            path: model_path.to_path_buf(),
            sha256: OnceLock::new(),
        })
    }

    fn sha256(&self) -> Result<&str> {
        if let Some(s) = self.sha256.get() {
            return Ok(s);
        }
        let digest = Embedder::file_sha256(&self.path)?;
        // A concurrent caller may have won the race; both computed the same value.
        Ok(self.sha256.get_or_init(|| digest))
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let encodings: Vec<tokenizers::Encoding> = texts
            .par_iter()
            .map(|t| {
                self.tokenizer
                    .encode(*t, true)
                    .map_err(|e| anyhow!("tokenization failed: {e}"))
            })
            .collect::<Result<_>>()?;

        // Similar lengths share a batch, so batches pad as little as possible.
        let mut order: Vec<usize> = (0..encodings.len()).collect();
        order.sort_by_key(|&i| encodings[i].len());
        let groups: Vec<&[usize]> = order.chunks(self.batch_size).collect();
        let pooled: Vec<Vec<Vec<f32>>> = groups
            .par_iter()
            .map(|group| {
                let batch: Vec<&tokenizers::Encoding> =
                    group.iter().map(|&i| &encodings[i]).collect();
                self.run_batch(&batch)
            })
            .collect::<Result<_>>()?;

        let mut out = vec![Vec::new(); texts.len()];
        for (group, vectors) in groups.iter().zip(pooled) {
            for (&i, v) in group.iter().zip(vectors) {
                out[i] = v;
            }
        }
        Ok(out)
    }

    /// One forward pass over `batch`, padded to its longest encoding.
    fn run_batch(&self, batch: &[&tokenizers::Encoding]) -> Result<Vec<Vec<f32>>> {
        let rows = batch.len();
        let seq = batch.iter().map(|e| e.len()).max().unwrap_or(0);
        if seq == 0 {
            bail!("tokenizer produced no tokens");
        }
        // Padding positions are masked out. pad_id is None only with
        // batch_size = 1, where a single row is never padded.
        let pad = self.pad_id.map_or(0, i64::from);
        let mut ids = vec![pad; rows * seq];
        let mut mask = vec![0i64; rows * seq];
        let mut types = vec![0i64; rows * seq];
        for (r, enc) in batch.iter().enumerate() {
            let row = r * seq;
            for (c, ((&id, &m), &t)) in enc
                .get_ids()
                .iter()
                .zip(enc.get_attention_mask())
                .zip(enc.get_type_ids())
                .enumerate()
            {
                ids[row + c] = i64::from(id);
                mask[row + c] = i64::from(m);
                types[row + c] = i64::from(t);
            }
        }

        let tensor = |data: &[i64]| -> Result<TValue> {
            let t: Tensor = tract_ndarray::Array2::from_shape_vec((rows, seq), data.to_vec())
                .context("building input tensor")?
                .into();
            Ok(t.into())
        };
        let mut inputs: TVec<TValue> = TVec::with_capacity(self.inputs.len());
        for role in &self.inputs {
            inputs.push(match role {
                InputRole::Ids => tensor(&ids)?,
                InputRole::Mask => tensor(&mask)?,
                InputRole::TypeIds => tensor(&types)?,
            });
        }
        let outputs = self.plan.run(inputs).context("tract inference")?;
        let output = outputs
            .first()
            .ok_or_else(|| anyhow!("model returned no output"))?;
        pool(output, self.pooling, &mask, rows, seq, self.dim)
    }
}

/// Read an ONNX file and make it loadable by tract: every graph input gets the
/// symbolic dims `B` (batch) and `S` (sequence), and shape annotations that tract
/// cannot parse (e.g. `Min(65536, sequence_length)` in gte-multilingual-base) are
/// dropped; tract infers them from the input facts.
fn read_patched_onnx(model_path: &Path) -> Result<InferenceModel> {
    use prost::Message as _;
    use tract_onnx::pb::tensor_shape_proto::dimension::Value;

    let raw = std::fs::read(model_path)
        .with_context(|| format!("reading ONNX file {}", model_path.display()))?;
    let mut proto = tract_onnx::pb::ModelProto::decode(raw.as_slice())
        .with_context(|| format!("decoding ONNX protobuf from {}", model_path.display()))?;
    if let Some(graph) = proto.graph.as_mut() {
        for input in &mut graph.input {
            if let Some(type_proto) = input.r#type.as_mut()
                && let Some(tract_onnx::pb::type_proto::Value::TensorType(ref mut tt)) =
                    type_proto.value
                && let Some(shape) = tt.shape.as_mut()
            {
                for (i, dim) in shape.dim.iter_mut().enumerate() {
                    let sym = if i == 0 { "B" } else { "S" };
                    dim.value = Some(Value::DimParam(sym.to_owned()));
                }
            }
        }
        graph.value_info.clear();
        for output in &mut graph.output {
            output.r#type = None;
        }
    }
    let patched = proto.encode_to_vec();
    tract_onnx::onnx()
        .model_for_read(&mut patched.as_slice())
        .with_context(|| format!("loading ONNX model from {}", model_path.display()))
}

/// The embedding dimension: the last axis of the (single) output.
fn output_dim(model: &TypedModel, pooling: Pooling) -> Result<usize> {
    let fact = model.output_fact(0).context("reading the output shape")?;
    let rank = match pooling {
        Pooling::Pooled => 2,
        Pooling::Mean => 3,
    };
    if fact.rank() != rank {
        bail!("model output has rank {}, expected {rank}", fact.rank());
    }
    let last = &fact.shape[rank - 1];
    last.as_i64()
        .and_then(|d| usize::try_from(d).ok())
        .ok_or_else(|| anyhow!("model output dimension is not fixed: {last}"))
}

/// Turn the model output into `rows` unit vectors.
fn pool(
    output: &Tensor,
    pooling: Pooling,
    mask: &[i64],
    rows: usize,
    seq: usize,
    dim: usize,
) -> Result<Vec<Vec<f32>>> {
    let data = output
        .as_slice::<f32>()
        .context("reading the model output as f32")?;
    let shape = output.shape();
    match pooling {
        Pooling::Pooled => {
            if shape != [rows, dim] {
                bail!("unexpected sentence_embedding shape {shape:?}, expected [{rows}, {dim}]");
            }
            Ok(data
                .chunks_exact(dim)
                .map(|row| {
                    let mut v = row.to_vec();
                    normalise(&mut v);
                    v
                })
                .collect())
        }
        Pooling::Mean => {
            if shape != [rows, seq, dim] {
                bail!("unexpected hidden-state shape {shape:?}, expected [{rows}, {seq}, {dim}]");
            }
            Ok(data
                .chunks_exact(seq * dim)
                .zip(mask.chunks_exact(seq))
                .map(|(tokens, row_mask)| {
                    let mut pooled = vec![0f32; dim];
                    let mut weight = 0f32;
                    for (token, &m) in tokens.chunks_exact(dim).zip(row_mask) {
                        if m != 0 {
                            weight += 1.0;
                            pooled.iter_mut().zip(token).for_each(|(p, x)| *p += x);
                        }
                    }
                    if weight > 0.0 {
                        pooled.iter_mut().for_each(|p| *p /= weight);
                    }
                    normalise(&mut pooled);
                    pooled
                })
                .collect())
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP backend (Ollama-compatible)
// ---------------------------------------------------------------------------

/// Port used when the URL names none (the http scheme's default).
const HTTP_DEFAULT_PORT: u16 = 80;

struct HttpClient {
    /// `host:port` to connect to.
    authority: String,
    /// Value of the Host header (the URL's authority as written).
    host_header: String,
    /// Request path: the URL's own path followed by the endpoint path.
    path: String,
    opts: HttpOptions,
}

impl HttpClient {
    fn new(opts: &HttpOptions) -> Result<Self> {
        for (key, value) in [
            ("http_connect_timeout_ms", opts.connect_timeout_ms),
            ("http_read_timeout_ms", opts.read_timeout_ms),
        ] {
            if value == 0 {
                bail!("[embed] {key} must be greater than 0");
            }
        }
        for (key, value) in [
            ("http_concurrency", opts.concurrency),
            ("http_max_input_bytes", opts.max_input_bytes),
            ("http_error_preview_chars", opts.error_preview_chars),
            ("batch_size", opts.batch_size),
        ] {
            if value == 0 {
                bail!("[embed] {key} must be greater than 0");
            }
        }
        if opts.model.is_empty() {
            bail!("[embed] model must name the HTTP model");
        }
        let rest = opts.url.strip_prefix("http://").ok_or_else(|| {
            anyhow!(
                "[embed] embed_url must start with http:// (https is not supported): {}",
                opts.url
            )
        })?;
        let (host_header, base) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        if host_header.is_empty() {
            bail!("[embed] embed_url has no host: {}", opts.url);
        }
        if !opts.endpoint_path.starts_with('/') {
            bail!(
                "[embed] http_endpoint_path must start with '/': {}",
                opts.endpoint_path
            );
        }
        // A port is present when the authority ends in ":digits" (an IPv6
        // literal without port ends in ']').
        let has_port = host_header
            .rsplit_once(':')
            .is_some_and(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        let authority = if has_port {
            host_header.to_owned()
        } else {
            format!("{host_header}:{HTTP_DEFAULT_PORT}")
        };
        Ok(Self {
            authority,
            host_header: host_header.to_owned(),
            path: format!("{}{}", base.trim_end_matches('/'), opts.endpoint_path),
            opts: opts.clone(),
        })
    }

    fn describe(&self) -> String {
        format!("http://{}{}", self.host_header, self.path)
    }

    /// Embed all texts: requests of `batch_size` texts, `concurrency` in flight.
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let batches: Vec<&[&str]> = texts.chunks(self.opts.batch_size).collect();
        let next = AtomicUsize::new(0);
        let workers = self.opts.concurrency.min(batches.len());

        let mut done: Vec<(usize, Result<Vec<Vec<f32>>>)> = Vec::with_capacity(batches.len());
        std::thread::scope(|s| -> Result<()> {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    s.spawn(|| {
                        let mut mine = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(batch) = batches.get(i) else { break };
                            mine.push((i, self.request(batch)));
                        }
                        mine
                    })
                })
                .collect();
            for h in handles {
                done.extend(
                    h.join()
                        .map_err(|_| anyhow!("HTTP embedding worker panicked"))?,
                );
            }
            Ok(())
        })?;
        done.sort_by_key(|(i, _)| *i);

        let mut out = Vec::with_capacity(texts.len());
        for (_, result) in done {
            out.extend(result?);
        }
        for v in &mut out {
            normalise(v);
        }
        Ok(out)
    }

    /// One POST with all of `texts` as the `input` array.
    fn request(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        use std::net::{TcpStream, ToSocketAddrs};
        use std::time::Duration;

        let input: Vec<&str> = texts
            .iter()
            .map(|t| truncate_at_char_boundary(t, self.opts.max_input_bytes))
            .collect();
        let body = serde_json::json!({
            "model": self.opts.model,
            "input": input,
            "truncate": true,
        })
        .to_string();

        let connect_timeout = Duration::from_millis(self.opts.connect_timeout_ms);
        let io_timeout = Duration::from_millis(self.opts.read_timeout_ms);
        let addrs = self
            .authority
            .to_socket_addrs()
            .with_context(|| format!("resolving {}", self.authority))?;
        let mut last_err = None;
        let mut stream = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, connect_timeout) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let mut stream = match (stream, last_err) {
            (Some(s), _) => s,
            (None, Some(e)) => {
                return Err(e).with_context(|| format!("connecting to {}", self.describe()));
            }
            (None, None) => bail!("{} resolved to no address", self.authority),
        };
        stream
            .set_read_timeout(Some(io_timeout))
            .context("setting read timeout")?;
        stream
            .set_write_timeout(Some(io_timeout))
            .context("setting write timeout")?;

        let request = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            self.path,
            self.host_header,
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .and_then(|()| stream.write_all(body.as_bytes()))
            .with_context(|| format!("sending request to {}", self.describe()))?;

        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .with_context(|| format!("reading response from {}", self.describe()))?;
        let preview_chars = self.opts.error_preview_chars;
        let body = parse_http_response(&raw, preview_chars)
            .with_context(|| format!("response from {}", self.describe()))?;
        parse_embeddings(&body, texts.len(), preview_chars)
            .with_context(|| format!("response from {}", self.describe()))
    }
}

/// The longest prefix of `s` of at most `max` bytes that ends on a character boundary.
fn truncate_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let end = (0..=max)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    &s[..end]
}

/// Lossy UTF-8 preview of a response body for error messages: its first
/// `max_chars` characters (`[embed] http_error_preview_chars`).
fn preview(body: &[u8], max_chars: usize) -> String {
    String::from_utf8_lossy(body)
        .chars()
        .take(max_chars)
        .collect()
}

/// Parse a raw HTTP/1.x response: check the status, then return the body
/// (de-chunked, or cut to Content-Length). An error status quotes the first
/// `preview_chars` characters of the body.
fn parse_http_response(raw: &[u8], preview_chars: usize) -> Result<Vec<u8>> {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response (no end of headers)"))?;
    let head = String::from_utf8_lossy(&raw[..header_end]);
    let body = &raw[header_end + 4..];

    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status line: {status_line:?}"))?;

    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.to_ascii_lowercase().contains("chunked");
            } else if name.eq_ignore_ascii_case("content-length") {
                content_length = Some(
                    value
                        .parse::<usize>()
                        .with_context(|| format!("bad Content-Length {value:?}"))?,
                );
            }
        }
    }

    let body = if chunked {
        decode_chunked(body)?
    } else if let Some(len) = content_length {
        body.get(..len)
            .ok_or_else(|| {
                anyhow!(
                    "response body is {} bytes, Content-Length says {len}",
                    body.len()
                )
            })?
            .to_vec()
    } else {
        body.to_vec()
    };

    if !(200..300).contains(&status) {
        bail!("HTTP status {status}: {}", preview(&body, preview_chars));
    }
    Ok(body)
}

/// Decode a chunked transfer-encoded body: `<hex size>[;ext]\r\n<data>\r\n`
/// repeated, ended by a zero-size chunk. Works on bytes, so multi-byte
/// characters split across chunks are reassembled intact.
fn decode_chunked(mut raw: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| anyhow!("truncated chunked body (no chunk size line)"))?;
        let size_line = std::str::from_utf8(&raw[..line_end])
            .map_err(|_| anyhow!("chunk size line is not ASCII"))?;
        let size_hex = size_line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_hex, 16)
            .with_context(|| format!("bad chunk size {size_hex:?}"))?;
        raw = &raw[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        let data = raw
            .get(..size)
            .ok_or_else(|| anyhow!("truncated chunked body (chunk of {size} bytes)"))?;
        out.extend_from_slice(data);
        raw = raw[size..]
            .strip_prefix(b"\r\n")
            .ok_or_else(|| anyhow!("chunk not followed by CRLF"))?;
    }
}

/// Parse `{"embeddings": [[f32, ..], ..]}` and check there is one finite,
/// numeric vector per input.
fn parse_embeddings(body: &[u8], expected: usize, preview_chars: usize) -> Result<Vec<Vec<f32>>> {
    let parsed: serde_json::Value = serde_json::from_slice(body)
        .with_context(|| format!("response is not JSON: {}", preview(body, preview_chars)))?;
    let embeddings = parsed
        .get("embeddings")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            anyhow!(
                "response has no 'embeddings' array: {}",
                preview(body, preview_chars)
            )
        })?;
    if embeddings.len() != expected {
        bail!(
            "response has {} embeddings for {expected} inputs",
            embeddings.len()
        );
    }
    embeddings
        .iter()
        .enumerate()
        .map(|(i, emb)| {
            emb.as_array()
                .ok_or_else(|| anyhow!("embedding {i} is not an array"))?
                .iter()
                .enumerate()
                .map(|(j, v)| {
                    v.as_f64()
                        .map(|x| x as f32)
                        .filter(|x| x.is_finite())
                        .ok_or_else(|| {
                            anyhow!("embedding {i} value {j} is not a finite number: {v}")
                        })
                })
                .collect()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Stub embedding: deterministic unit vector from text
// ---------------------------------------------------------------------------

/// Generate a deterministic unit vector of `dim` f32s from `text`:
/// an FNV-1a hash of the text seeds an LCG (Knuth's MMIX constants) whose
/// outputs, mapped to [-1, 1], are L2-normalised.
fn stub_embed(text: &str, dim: usize) -> Vec<f32> {
    const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
    const FNV_PRIME: u64 = 1_099_511_628_211;
    const LCG_A: u64 = 6_364_136_223_846_793_005;
    const LCG_C: u64 = 1_442_695_040_888_963_407;

    let mut state = text.bytes().fold(FNV_OFFSET, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(FNV_PRIME)
    });
    let mut v: Vec<f32> = (0..dim)
        .map(|_| {
            state = state.wrapping_mul(LCG_A).wrapping_add(LCG_C);
            let bits = (state >> 32) as u32;
            (bits as f32 / 2_147_483_648.0) - 1.0
        })
        .collect();
    normalise(&mut v);
    v
}

/// In-place L2 normalisation. No-op if the norm is near zero.
fn normalise(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-9 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    const DIM: usize = 64;
    /// Test value of `[embed] http_error_preview_chars`.
    const PREVIEW_CHARS: usize = 20;

    #[test]
    fn stub_is_unit_length_and_deterministic() {
        let emb = Embedder::stub(DIM);
        let a = emb.embed_one("hello world").unwrap();
        assert_eq!(a.len(), DIM);
        assert!((dot(&a, &a) - 1.0).abs() < 1e-5);
        assert_eq!(a, emb.embed_one("hello world").unwrap());
        let b = emb.embed_one("completely different text").unwrap();
        assert!(dot(&a, &b) < 0.99);
    }

    #[test]
    fn stub_batch_matches_individual_and_empty_batch_is_empty() {
        let emb = Embedder::stub(DIM);
        let texts = ["alpha", "beta", "gamma"];
        let batch = emb.embed_batch(&texts).unwrap();
        for (text, v) in texts.iter().zip(&batch) {
            assert_eq!(v, &emb.embed_one(text).unwrap());
        }
        assert!(emb.embed_batch(&[]).unwrap().is_empty());
    }

    #[test]
    fn stub_identity() {
        let emb = Embedder::stub(DIM);
        assert_eq!(emb.model_file(), None);
        assert_eq!(emb.dim(), DIM);
        assert_eq!(emb.identity(), &ModelIdentity::new(STUB_MODEL_NAME, DIM));
        assert_eq!(emb.model_sha256().unwrap(), None);
        assert_eq!(emb.identity_with_sha().unwrap().sha256, None);
    }

    #[test]
    fn identity_compatibility_rules() {
        let a = ModelIdentity::new("m", 768);
        let mut a_sha = a.clone();
        a_sha.sha256 = Some("aa".into());
        let mut b_sha = a.clone();
        b_sha.sha256 = Some("bb".into());
        assert!(
            a.is_compatible_with(&a_sha),
            "unknown sha is not a mismatch"
        );
        assert!(
            !a_sha.is_compatible_with(&b_sha),
            "two known, different shas"
        );
        assert!(!a.is_compatible_with(&ModelIdentity::new("other", 768)));
        assert!(!a.is_compatible_with(&ModelIdentity::new("m", 384)));
        assert_eq!(a_sha.to_string(), "m (dim 768, sha256 aa)");
    }

    #[test]
    fn file_sha256_streams_known_digest() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"abc").unwrap();
        assert_eq!(
            Embedder::file_sha256(tmp.path()).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn onnx_opts() -> OnnxOptions {
        OnnxOptions {
            max_tokens: 16,
            batch_size: 1,
        }
    }

    #[test]
    fn load_missing_model_or_tokenizer_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("model.onnx");
        let err = Embedder::load(&missing, "m", &onnx_opts()).unwrap_err();
        assert!(format!("{err:#}").contains("tokenizer"), "{err:#}");
    }

    #[test]
    fn load_rejects_zero_options() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("model.onnx");
        let zero_batch = OnnxOptions {
            batch_size: 0,
            ..onnx_opts()
        };
        let err = Embedder::load(&path, "m", &zero_batch).unwrap_err();
        assert!(err.to_string().contains("onnx_batch_size"), "{err}");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_at_char_boundary("héllo", 2), "h");
        assert_eq!(truncate_at_char_boundary("héllo", 3), "hé");
        assert_eq!(truncate_at_char_boundary("abc", 10), "abc");
    }

    // --- HTTP response parsing ---

    #[test]
    fn chunked_body_is_decoded_on_bytes() {
        // "é" (C3 A9) is split across two chunks: decoding must rejoin it.
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                    3\r\n{\"\xC3\r\n2;ext=1\r\n\xA9\"\r\n1\r\n}\r\n0\r\n\r\n";
        let body = parse_http_response(raw, PREVIEW_CHARS).unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "{\"é\"}");
    }

    #[test]
    fn truncated_chunk_is_an_error() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nff\r\nshort";
        assert!(parse_http_response(raw, PREVIEW_CHARS).is_err());
    }

    #[test]
    fn content_length_cuts_the_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}garbage";
        assert_eq!(parse_http_response(raw, PREVIEW_CHARS).unwrap(), b"{}");
    }

    #[test]
    fn error_status_is_reported_with_multibyte_preview() {
        let body = "é".repeat(PREVIEW_CHARS * 2);
        let raw = format!("HTTP/1.1 500 Internal Server Error\r\n\r\n{body}");
        let err = parse_http_response(raw.as_bytes(), PREVIEW_CHARS)
            .unwrap_err()
            .to_string();
        assert!(err.contains("HTTP status 500"), "{err}");
        // The configured number of characters is quoted, no more.
        assert_eq!(err.matches('é').count(), PREVIEW_CHARS, "{err}");
    }

    #[test]
    fn embeddings_must_be_numeric_and_counted() {
        assert_eq!(
            parse_embeddings(br#"{"embeddings":[[1,2],[3,4.5]]}"#, 2, PREVIEW_CHARS).unwrap(),
            vec![vec![1.0, 2.0], vec![3.0, 4.5]]
        );
        assert!(parse_embeddings(br#"{"embeddings":[[1,"x"]]}"#, 1, PREVIEW_CHARS).is_err());
        assert!(parse_embeddings(br#"{"embeddings":[[1,2]]}"#, 2, PREVIEW_CHARS).is_err());
        assert!(parse_embeddings(br#"{"error":"model not found"}"#, 1, PREVIEW_CHARS).is_err());
    }

    fn http_opts(url: String) -> HttpOptions {
        HttpOptions {
            url,
            model: "test-model".into(),
            endpoint_path: "/api/embed".into(),
            connect_timeout_ms: 2_000,
            read_timeout_ms: 5_000,
            concurrency: 2,
            max_input_bytes: 64,
            error_preview_chars: PREVIEW_CHARS,
            batch_size: 2,
        }
    }

    #[test]
    fn url_path_is_kept_and_port_defaults() {
        let c = HttpClient::new(&http_opts("http://embed.example:9000/ollama/".into())).unwrap();
        assert_eq!(c.authority, "embed.example:9000");
        assert_eq!(c.path, "/ollama/api/embed");
        let c = HttpClient::new(&http_opts("http://embed.example".into())).unwrap();
        assert_eq!(c.authority, format!("embed.example:{HTTP_DEFAULT_PORT}"));
        assert!(HttpClient::new(&http_opts("https://embed.example".into())).is_err());
    }

    /// A one-thread fake server: answers each connection with a chunked JSON
    /// body holding one 3-d vector per input, and records the request lines.
    fn fake_server(connections: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..connections {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                // Read headers + body (Content-Length) without waiting for EOF.
                let body = loop {
                    let n = s.read(&mut tmp).unwrap();
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if buf.len() >= end + 4 + len {
                            seen.push(head.lines().next().unwrap().to_string());
                            break buf[end + 4..end + 4 + len].to_vec();
                        }
                    }
                };
                let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let n = req["input"].as_array().unwrap().len();
                let vecs: Vec<[f32; 3]> = (0..n).map(|i| [i as f32 + 1.0, 0.0, 0.0]).collect();
                let json = serde_json::json!({ "embeddings": vecs }).to_string();
                let (a, b) = json.split_at(json.len() / 2);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
                    a.len(),
                    b.len()
                );
                s.write_all(resp.as_bytes()).unwrap();
            }
            seen
        });
        (format!("http://{addr}/base"), handle)
    }

    #[test]
    fn http_backend_batches_requests_and_normalises() {
        // 1 probe + ceil(3 / batch_size=2) = 2 requests.
        let (url, server) = fake_server(3);
        let emb = Embedder::http(&http_opts(url)).unwrap();
        assert_eq!(emb.dim(), 3);
        assert_eq!(emb.identity().name, "test-model");
        let out = emb.embed_batch(&["a", "b", "c"]).unwrap();
        assert_eq!(out.len(), 3);
        for v in &out {
            assert!((dot(v, v) - 1.0).abs() < 1e-6);
        }
        let lines = server.join().unwrap();
        assert!(
            lines.iter().all(|l| l == "POST /base/api/embed HTTP/1.1"),
            "{lines:?}"
        );
    }

    #[test]
    fn http_connect_failure_is_an_error() {
        // Bind then drop: the port is (almost certainly) closed.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = Embedder::http(&http_opts(format!("http://127.0.0.1:{port}"))).unwrap_err();
        assert!(format!("{err:#}").contains("connecting"), "{err:#}");
    }

    // --- Real model (run with: VEC_TEST_MODEL=/path/to/model.onnx cargo test -- --ignored) ---

    fn real_model(batch_size: usize) -> Embedder {
        let path = std::env::var_os("VEC_TEST_MODEL")
            .expect("set VEC_TEST_MODEL to an ONNX model file with tokenizer.json beside it");
        Embedder::load(
            Path::new(&path),
            "test-model",
            &OnnxOptions {
                max_tokens: 128,
                batch_size,
            },
        )
        .unwrap()
    }

    fn sample_texts() -> Vec<String> {
        (0..12)
            .map(|i| {
                format!(
                    "fn handler_{i}() {{ let total = compute({i}); println!(\"{{}}\", total); }} {}",
                    "more words here ".repeat(i)
                )
            })
            .collect()
    }

    #[test]
    #[ignore = "needs a real ONNX model in VEC_TEST_MODEL"]
    fn real_model_unbatched_equals_single_text() {
        let emb = real_model(1);
        let texts = sample_texts();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let batch = emb.embed_batch(&refs).unwrap();
        for (t, v) in refs.iter().zip(&batch) {
            let single = emb.embed_one(t).unwrap();
            let max_diff = v
                .iter()
                .zip(&single)
                .map(|(a, b)| (a - b).abs())
                .fold(0f32, f32::max);
            assert!(max_diff < 1e-4, "max diff {max_diff}");
            assert!((dot(v, v) - 1.0).abs() < 1e-4);
        }
    }

    #[test]
    #[ignore = "needs a real ONNX model in VEC_TEST_MODEL"]
    fn real_model_batched_is_close_to_single_and_ranks_sensibly() {
        let single = real_model(1);
        let batched = real_model(4);
        assert_eq!(single.dim(), batched.dim());
        let texts = sample_texts();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let a = single.embed_batch(&refs).unwrap();
        let b = batched.embed_batch(&refs).unwrap();
        // Quantized models drift a little when padded (see module docs); a
        // float model stays within rounding.
        for (x, y) in a.iter().zip(&b) {
            assert!(dot(x, y) > 0.95, "cosine {}", dot(x, y));
        }
        let q = single
            .embed_one("function that prints a computed total")
            .unwrap();
        let near = single.embed_one(&texts[0]).unwrap();
        let far = single.embed_one("Die Katze schläft auf dem Sofa.").unwrap();
        assert!(dot(&q, &near) > dot(&q, &far));
    }
}

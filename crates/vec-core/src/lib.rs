//! vec-core — shared foundations: configuration, `vec init` templates,
//! snippet rendering, utilities, embedder loading, and model identities
//! with a cached model file hash ([`model_sha`]).

pub mod config;
pub mod init;
pub mod model_sha;
pub mod snippet;
pub mod util;

use anyhow::{Context, Result};
use config::{Config, EmbedBackend};
use vec_embed::Embedder;

/// Load the embedder selected by `[embed] backend`.
///
/// - `onnx`: resolves `[embed] model` in `model_search_path` and loads it.
/// - `ollama`: connects to `[embed] embed_url` (one probe request).
///
/// Any failure is an error that says what to fix. There is no fallback: a
/// stub embedder would fill the index with meaningless vectors.
pub fn load_embedder(cfg: &Config) -> Result<Embedder> {
    match cfg.embed.backend {
        EmbedBackend::Onnx => {
            let path = cfg.resolve_model_path()?;
            Embedder::load(&path, &cfg.embed.model, &cfg.onnx_options()).with_context(|| {
                format!(
                    "loading ONNX model {} ([embed] model = {:?}); reinstall the model or \
                     point [embed] model at a valid .onnx file",
                    path.display(),
                    cfg.embed.model
                )
            })
        }
        EmbedBackend::Ollama => {
            let opts = cfg.http_options()?;
            Embedder::http(&opts).with_context(|| {
                format!(
                    "using the embedding server at {} ([embed] embed_url) with model {:?}; \
                     check that the server runs and serves this model",
                    opts.url, opts.model
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_onnx_model_is_an_error_not_a_stub() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::defaults().unwrap();
        cfg.embed.model_search_path = vec![dir.path().to_path_buf()];
        let err = format!("{:#}", load_embedder(&cfg).unwrap_err());
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn broken_onnx_model_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::defaults().unwrap();
        let model = dir.path().join("broken.onnx");
        std::fs::write(&model, b"not a model").unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"{}").unwrap();
        cfg.embed.model = model.display().to_string();
        let err = format!("{:#}", load_embedder(&cfg).unwrap_err());
        assert!(err.contains("loading ONNX model"), "{err}");
    }

    #[test]
    fn unreachable_http_backend_is_an_error() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let cfg = Config::from_toml(&format!(
            "[embed]\nbackend = \"ollama\"\nmodel = \"m\"\nembed_url = \"http://127.0.0.1:{port}\"\n"
        ))
        .unwrap();
        let err = format!("{:#}", load_embedder(&cfg).unwrap_err());
        assert!(err.contains("embed_url"), "{err}");
    }
}

//! vec-core — shared foundations: configuration, utilities, and embedder loading.

pub mod config;
pub mod util;

use config::Config;
use vec_embed::Embedder;

/// Load the embedder based on the configured backend.
///
/// - `backend = "onnx"` (default): loads a local ONNX model via tract.
/// - `backend = "ollama"`: connects to an Ollama-compatible HTTP API.
///
/// Falls back to a stub embedder if loading fails.
pub fn load_embedder(cfg: &Config) -> Embedder {
    match cfg.embed.backend.as_str() {
        "ollama" => {
            if cfg.embed.embed_url.is_empty() {
                anstream::eprintln!(
                    "error: backend = \"ollama\" but embed_url is not set in config.\n\
                     Set [embed] embed_url to your Ollama endpoint."
                );
                return Embedder::stub(768);
            }
            match Embedder::http(&cfg.embed.embed_url, &cfg.embed.model) {
                Ok(e) => e,
                Err(err) => {
                    anstream::eprintln!(
                        "warn: could not connect to Ollama at {}: {:?}\n\
                         Falling back to stub embedder.",
                        cfg.embed.embed_url,
                        err,
                    );
                    Embedder::stub(768)
                }
            }
        }
        _ => {
            // Default: local ONNX via tract.
            match cfg.resolve_model_path() {
                Ok(model_path) => match Embedder::load(&model_path, cfg.embed.max_tokens) {
                    Ok(e) => return e,
                    Err(err) => {
                        anstream::eprintln!(
                            "warn: could not load model at {}: {:?}\n\
                                 Falling back to stub embedder (not semantically meaningful).",
                            model_path.display(),
                            err,
                        );
                    }
                },
                Err(_) => {
                    anstream::eprintln!(
                        "warn: model '{}' not found in search path.\n\
                         Run `vec model download` for installation instructions.\n\
                         Using stub embedder (not semantically meaningful).",
                        cfg.embed.model
                    );
                }
            }
            Embedder::stub(768)
        }
    }
}

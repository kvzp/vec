//! vec-core — shared foundations: configuration, utilities, and embedder loading.

pub mod config;
pub mod util;

use config::Config;
use vec_embed::Embedder;

/// Load the embedder from the configured model path.
///
/// If the model cannot be found or loaded, falls back to a deterministic stub
/// embedder (dim=768) and prints a warning to stderr.
pub fn load_embedder(cfg: &Config) -> Embedder {
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

//! `vec serve` (MCP) and `vec daemon` (embeddings).

use std::path::Path;

use anyhow::{Context, Result};

use super::load_config;

/// `vec serve`: the MCP server on stdio, with the configuration of
/// `--config` and the usual layers.
pub fn serve(cfg_path: Option<&Path>) -> Result<()> {
    let cfg = load_config(cfg_path)?;
    tokio::runtime::Runtime::new()
        .context("starting the async runtime")?
        .block_on(vec_mcp::run_server(cfg))
}

/// Daemon start-up: re-embed the index's canary and log how far the vectors
/// moved. The daemon serves query vectors, so a drift is reported (queries
/// would compare vectors of another runtime with the index's) but is not
/// fatal: queries are readers, and `vec doctor` reports it as a failure.
#[cfg(unix)]
fn report_canary(cfg: &vec_core::config::Config, embedder: &vec_embed::Embedder) {
    use vec_store::{CanaryLimits, CanaryVerdict};

    let store = match vec_query::Searcher::new(cfg.clone()).open_store() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("embedding canary: not checked, the index cannot be opened: {e:#}");
            return;
        }
    };
    if !matches!(store.model(), Ok(Some(m)) if m.is_compatible_with(embedder.identity())) {
        eprintln!("embedding canary: not checked (the index has no or another model)");
        return;
    }
    let limits = CanaryLimits::from_config(&cfg.embed);
    match store.check_canary(|t| embedder.embed_one(t), embedder.runtime(), limits) {
        Ok(None) => eprintln!("embedding canary: none recorded in the index yet"),
        Ok(Some(c)) => match c.verdict {
            CanaryVerdict::Ok => eprintln!("{c}"),
            CanaryVerdict::Warn => eprintln!("warning: {c}"),
            CanaryVerdict::Drift => eprintln!(
                "error: {c}: query vectors no longer match the index; run 'vec updatedb --full' \
                 or restore the previous runtime"
            ),
        },
        Err(e) => eprintln!("warning: could not check the embedding canary: {e:#}"),
    }
}

/// `vec daemon`: load the configured embedder and serve it.
pub fn daemon(cfg_path: Option<&Path>) -> Result<()> {
    let cfg = load_config(cfg_path)?;
    #[cfg(unix)]
    {
        eprintln!(
            "Loading the {} model {} …",
            cfg.embed.backend, cfg.embed.model
        );
        let embedder = vec_core::load_embedder(&cfg)?;
        report_canary(&cfg, &embedder);
        vec_daemon::run_daemon(embedder, &cfg.daemon)
    }
    #[cfg(not(unix))]
    {
        let _ = cfg;
        anyhow::bail!("vec daemon needs Unix sockets, which this platform lacks")
    }
}

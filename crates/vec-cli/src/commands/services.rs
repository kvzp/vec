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
        vec_daemon::run_daemon(embedder, &cfg.daemon)
    }
    #[cfg(not(unix))]
    {
        let _ = cfg;
        anyhow::bail!("vec daemon needs Unix sockets, which this platform lacks")
    }
}

//! vec-mcp — the MCP server of vec (`vec serve`), on stdio.
//!
//! Tools:
//!
//! | tool | does |
//! |---|---|
//! | `search` | semantic search: one or more queries, path filter, excludes, min score |
//! | `similar` | chunks similar to the one at `file_path:line` (stored vector, no model) |
//! | `explain` | the chunks covering `file_path:line` and the file's index entry |
//! | `context` | lines around `file_path:line` of an indexed file |
//! | `index_status` | index, model, embedding runtime, watcher and daemon state |
//!
//! Search and ranking are [`vec_query::Searcher`]'s, exactly as in the CLI.
//! The server keeps one searcher, so the in-process model is loaded at most
//! once, on the first query that needs it (the embedding daemon is used when it
//! serves the index's model). Every tool runs on the blocking thread pool
//! (SQLite, file reads and model inference block).
//!
//! `context` only reads files that are in the index, readable by the user
//! running the server, regular (symlinks are not followed) and at most
//! `[index] max_file_size` bytes: a prompt-injected client cannot use it to
//! read arbitrary files such as `~/.ssh/id_*` or `/dev/zero`.
//!
//! Relative paths in parameters are resolved against the server's working
//! directory. A tool failure is returned as an error result (`isError`) whose
//! text says what to fix.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock as Content, Implementation, ServerCapabilities, ServerConfig,
    },
    schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

use vec_core::config::Config;
use vec_query::{SearchRequest, Searcher};

// ---------------------------------------------------------------------------
// Tool parameter types
// ---------------------------------------------------------------------------

/// Parameters of `search`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// The semantic search query.
    pub query: Option<String>,
    /// More queries; the results of all queries are merged (best score per
    /// chunk). At least one of `query` and `queries` is required.
    pub queries: Option<Vec<String>>,
    /// Maximum number of results (default: from config).
    pub limit: Option<u32>,
    /// Only return files equal to or under this path.
    pub path_filter: Option<String>,
    /// Drop files equal to or under these paths.
    pub exclude: Option<Vec<String>>,
    /// Minimum cosine similarity (-1.0 to 1.0; default: from config).
    pub min_score: Option<f32>,
}

/// A location in a file.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LocationParams {
    /// Path of the file (absolute, or relative to the server's directory).
    pub file_path: String,
    /// Line number (1-based).
    pub line: u32,
}

/// Parameters of `context`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContextParams {
    /// Path of an indexed file (absolute, or relative to the server's directory).
    pub file_path: String,
    /// Target line number (1-based).
    pub line: u32,
    /// Lines of context on each side of `line` (default: from config).
    pub window: Option<u32>,
}

/// Parameters of `similar`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SimilarParams {
    /// Path of an indexed file (absolute, or relative to the server's directory).
    pub file_path: String,
    /// A line covered by the chunk to compare with (1-based).
    pub line: u32,
    /// Maximum number of results (default: from config).
    pub limit: Option<u32>,
}

/// `index_status` parameters.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IndexStatusParams {
    /// Also ask the embedding daemon for its model. Off by default:
    /// connecting starts a socket-activated daemon, which loads the model.
    #[serde(default)]
    pub probe_daemon: bool,
}

/// One result of `search` or `similar` (serialised as JSON).
#[derive(Debug, Serialize)]
pub struct ResultItem {
    /// Absolute path of the file.
    pub path: String,
    /// The line of the chunk that best matches the query (1-based); the
    /// chunk's first line for `similar`.
    pub line: usize,
    /// First line of the matching chunk (1-based).
    pub start_line: usize,
    /// Last line of the matching chunk (1-based).
    pub end_line: usize,
    /// Ranking score: cosine similarity, plus the path boost for `search`.
    pub score: f32,
    /// Up to `[search] snippet_lines` lines before and after `line` (a radius), read from the live file,
    /// as `> NNNNN: text` rows (`>` marks `line`); absent when the file
    /// cannot be read.
    pub snippet: Option<String>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// The MCP server: one shared [`Searcher`] (config + lazily loaded model).
#[derive(Clone)]
pub struct VecServer {
    searcher: Arc<Searcher>,
    #[allow(dead_code)] // read by the code #[tool_handler] generates
    tool_router: ToolRouter<Self>,
}

/// Run `f` with the searcher on the blocking pool; an error becomes an error
/// result the client sees.
async fn blocking(
    searcher: &Arc<Searcher>,
    f: impl FnOnce(&Searcher) -> Result<String> + Send + 'static,
) -> Result<CallToolResult, McpError> {
    let searcher = Arc::clone(searcher);
    let out = tokio::task::spawn_blocking(move || f(&searcher))
        .await
        .map_err(|e| McpError::internal_error(format!("tool task failed: {e}"), None))?;
    Ok(match out {
        Ok(text) => CallToolResult::success(vec![Content::text(text)]),
        Err(e) => CallToolResult::error(vec![Content::text(format!("{e:#}"))]),
    })
}

fn to_json(value: &impl Serialize) -> Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}

fn items(annotated: Vec<vec_query::Annotated>) -> Vec<ResultItem> {
    annotated
        .into_iter()
        .map(|a| ResultItem {
            path: a.path.to_string_lossy().into_owned(),
            line: a.line,
            start_line: a.start_line,
            end_line: a.end_line,
            score: a.score,
            snippet: a.snippet,
        })
        .collect()
}

fn location(file_path: &str) -> Result<PathBuf> {
    vec_core::util::absolute_lexical(Path::new(file_path))
}

#[tool_router]
impl VecServer {
    /// A server for `cfg`.
    #[must_use]
    pub fn new(cfg: Config) -> Self {
        Self::with_searcher(Searcher::new(cfg))
    }

    /// A server using `searcher` (tests, or an embedder already loaded).
    #[must_use]
    pub fn with_searcher(searcher: Searcher) -> Self {
        Self {
            searcher: Arc::new(searcher),
            tool_router: Self::tool_router(),
        }
    }

    /// Semantic search.
    #[tool(
        description = "Search indexed files by meaning. Give `query` and/or `queries` (results \
                       merged); optional `path_filter`, `exclude` paths, `limit`, `min_score`. \
                       Returns JSON: path, best-matching line, chunk lines, score, snippet."
    )]
    pub async fn search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        blocking(&self.searcher, move |s| {
            let req = SearchRequest {
                queries: p
                    .query
                    .into_iter()
                    .chain(p.queries.into_iter().flatten())
                    .collect(),
                limit: p.limit.map(|l| l as usize),
                min_score: p.min_score,
                path_filter: p.path_filter.map(PathBuf::from),
                exclude: p.exclude.into_iter().flatten().map(PathBuf::from).collect(),
            };
            let store = s.open_store()?;
            let hits = s.search(&store, &req)?;
            let out = s.annotate(&hits, &req.queries, Some(s.config().search.snippet_lines));
            to_json(&items(out))
        })
        .await
    }

    /// Chunks similar to a location.
    #[tool(
        description = "Find code or text similar to the indexed chunk covering file_path:line \
                       (uses its stored vector). Returns JSON like `search`."
    )]
    pub async fn similar(
        &self,
        Parameters(p): Parameters<SimilarParams>,
    ) -> Result<CallToolResult, McpError> {
        blocking(&self.searcher, move |s| {
            let path = location(&p.file_path)?;
            let store = s.open_store()?;
            let hits = s.similar(&store, &path, p.line as usize, p.limit.map(|l| l as usize))?;
            let out = s.annotate(&hits, &[], Some(s.config().search.snippet_lines));
            to_json(&items(out))
        })
        .await
    }

    /// The index entry of a location.
    #[tool(
        description = "Explain how file_path:line is indexed: the chunks covering the line \
                       (line and byte ranges), the file's indexed hash, and whether it changed \
                       since. JSON."
    )]
    pub async fn explain(
        &self,
        Parameters(p): Parameters<LocationParams>,
    ) -> Result<CallToolResult, McpError> {
        blocking(&self.searcher, move |s| {
            let path = location(&p.file_path)?;
            let store = s.open_store()?;
            to_json(&s.explain(&store, &path, p.line as usize)?)
        })
        .await
    }

    /// Lines around a location of an indexed file.
    #[tool(
        description = "Return the lines around file_path:line of an indexed file (the line is \
                       marked with '>'). Useful to read more of a search result."
    )]
    pub async fn context(
        &self,
        Parameters(p): Parameters<ContextParams>,
    ) -> Result<CallToolResult, McpError> {
        blocking(&self.searcher, move |s| {
            let path = location(&p.file_path)?;
            let store = s.open_store()?;
            let content = s.read_indexed(&store, &path)?;
            let window = p
                .window
                .map_or(s.config().search.context_window, |w| w as usize);
            vec_query::context(&content, &path, p.line as usize, window)
        })
        .await
    }

    /// Index statistics.
    #[tool(
        description = "Return the state of the vec index: files, chunks, schema, the model the \
                       index was built with vs the configured one, last updatedb run, the \
                       embedding runtime of the first and last write and the stored embedding \
                       canary, watcher state, and whether the embedding daemon socket exists (probe_daemon: \
                       ask the daemon for its model, which starts a socket-activated one). JSON."
    )]
    pub async fn index_status(
        &self,
        Parameters(p): Parameters<IndexStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        blocking(&self.searcher, move |s| {
            let store = s.open_store()?;
            to_json(&vec_query::status::status(
                s.config(),
                &store,
                p.probe_daemon,
            )?)
        })
        .await
    }
}

#[tool_handler]
impl ServerHandler for VecServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "vec — semantic file search. Use `search` to find files by meaning, \
                 `context` to read lines around a result, `similar` to find related code, \
                 `explain` to see how a location is indexed, `index_status` to check the index.",
            )
    }
}

// ---------------------------------------------------------------------------
// run_server — public entry point called from main.rs
// ---------------------------------------------------------------------------

/// Serve MCP on stdio with `cfg` until the client closes the connection.
///
/// # Errors
///
/// The transport fails to start or stops with an error.
pub async fn run_server(cfg: Config) -> Result<()> {
    let service = VecServer::new(cfg)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP server initialise error: {e}"))?;
    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server join error: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests;

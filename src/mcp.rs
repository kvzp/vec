// src/mcp.rs — MCP server for vec, exposing three tools:
//
//   search(query, limit?, path_filter?) -> JSON array of SearchResultItem
//   context(file_path, line, window?)   -> string of surrounding lines
//   index_status()                      -> JSON object with index stats
//
// Transport: stdio (read from stdin, write to stdout) — the standard MCP
// transport for CLI-launched servers like vec.
//
// rmcp 0.3 API used here:
//   - `#[tool_router]` on an impl block registers tools via the ToolRouter.
//   - `#[tool(description = "...")]` marks each method.
//   - Parameters are extracted using the `Parameters<T>` newtype wrapper.
//   - `ServerHandler` is implemented to provide server metadata.
//   - `.serve(stdio())` connects the handler to the stdio transport.
//   - `.waiting().await` blocks until the connection closes.

use std::sync::Arc;

use anyhow::Result;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::tool::{Parameters, ToolRouter},
    model::{
        CallToolResult, Content, ServerCapabilities, ServerInfo,
    },
    schemars, tool, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::embed::Embedder;
use crate::store::Store;

// ---------------------------------------------------------------------------
// Public return types (serialised to JSON for MCP clients)
// ---------------------------------------------------------------------------

/// One search result returned by the `search` tool.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchResultItem {
    /// Absolute path to the source file.
    pub path: String,
    /// First line of the matching chunk (1-based).
    pub start_line: usize,
    /// Last line of the matching chunk (1-based).
    pub end_line: usize,
    /// Cosine similarity score in [0, 1].
    pub score: f32,
    /// Text snippet (the raw chunk bytes read from the live file).
    pub snippet: Option<String>,
}

/// Index statistics returned by the `index_status` tool.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexStatusResult {
    /// Number of files indexed.
    pub file_count: usize,
    /// Number of chunks (embedding rows) in the index.
    pub chunk_count: usize,
    /// Path to the SQLite database.
    pub db_path: String,
    /// Configured embedding model name.
    pub model: String,
    /// Whether the model file was found on disk.
    pub model_found: bool,
}

// ---------------------------------------------------------------------------
// Tool parameter types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// The semantic search query.
    pub query: String,
    /// Maximum number of results to return (default: from config).
    pub limit: Option<u32>,
    /// Only return results whose file path starts with this prefix.
    pub path_filter: Option<String>,
    /// Minimum cosine similarity score (0.0–1.0); results below this are suppressed.
    pub min_score: Option<f32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContextParams {
    /// Absolute path to the source file.
    pub file_path: String,
    /// Target line number (1-based).
    pub line: u32,
    /// Number of lines of context on each side of `line` (default: 10).
    pub window: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IndexStatusParams {}

// ---------------------------------------------------------------------------
// Server struct
// ---------------------------------------------------------------------------

/// The MCP server — holds the loaded config (cheaply Arc'd so it is Clone).
#[derive(Clone)]
pub struct VecServer {
    config: Arc<Config>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl VecServer {
    /// Search indexed files by semantic meaning.
    #[tool(description = "Search files by semantic meaning using vector embeddings. Returns file paths, line numbers, and optional snippets ranked by relevance.")]
    fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = &self.config;

        // Open the store (WAL mode allows concurrent readers).
        let store = Store::open(&cfg.database.db_path, cfg.database.wal)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Load embedder (stub fallback if model not ready).
        let mut embedder = load_embedder_for_mcp(cfg);

        let limit = params.limit.map(|l| l as usize).unwrap_or(cfg.search.default_limit);
        let min_score = params.min_score.unwrap_or(0.0);
        let path_filter = params.path_filter.as_deref().map(std::path::Path::new);

        let embedding = embedder
            .embed_one(&params.query)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let raw_results = store
            .search(&embedding, limit, min_score, path_filter)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Convert to SearchResultItem, applying access() check and reading
        // snippets from the live file.
        let mut items: Vec<SearchResultItem> = Vec::new();
        for result in raw_results {
            // Security: check read permission before returning the result.
            if !crate::util::can_read(&result.path) {
                continue;
            }

            let snippet = std::fs::read(&result.path).ok().map(|bytes| {
                let end = result.byte_end.min(bytes.len());
                let start = result.byte_offset.min(end);
                String::from_utf8_lossy(&bytes[start..end]).into_owned()
            });

            items.push(SearchResultItem {
                path: result.path.to_string_lossy().into_owned(),
                start_line: result.start_line,
                end_line: result.end_line,
                score: result.score,
                snippet,
            });
        }

        let json = serde_json::to_string_pretty(&items)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get source lines around a specific file location.
    #[tool(description = "Return a window of source lines centred on a given line in a file. Useful for reading context around a search result.")]
    fn context(
        &self,
        Parameters(params): Parameters<ContextParams>,
    ) -> Result<CallToolResult, McpError> {
        let window = params.window.unwrap_or(10) as usize;
        let target_line = params.line as usize; // 1-based

        // Security: check read permission before returning file content.
        let path = std::path::Path::new(&params.file_path);
        if !crate::util::can_read(path) {
            return Err(McpError::invalid_params(
                format!("Permission denied: {}", params.file_path),
                None,
            ));
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| {
                McpError::internal_error(
                    format!("Could not read {}: {e}", params.file_path),
                    None,
                )
            })?;

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        if total == 0 || target_line == 0 {
            return Ok(CallToolResult::success(vec![Content::text(String::new())]));
        }

        // Convert 1-based target to 0-based index, clamp to file bounds.
        let idx = (target_line - 1).min(total - 1);
        let start = idx.saturating_sub(window);
        let end = (idx + window + 1).min(total);

        let mut out = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            let lineno = start + i + 1; // back to 1-based for display
            let marker = if lineno == target_line { ">" } else { " " };
            out.push_str(&format!("{} {:>5}: {}\n", marker, lineno, line));
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Return index statistics (file count, chunk count, model info).
    #[tool(description = "Return statistics about the vec index: number of files, chunks, database path, and configured model.")]
    fn index_status(
        &self,
        Parameters(_params): Parameters<IndexStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = &self.config;

        let store = Store::open(&cfg.database.db_path, cfg.database.wal)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let (file_count, chunk_count, _last_mtime) = store
            .stats()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let model_found = cfg.resolve_model_path().is_ok();

        let result = IndexStatusResult {
            file_count,
            chunk_count,
            db_path: cfg.database.db_path.to_string_lossy().into_owned(),
            model: cfg.embed.model.clone(),
            model_found,
        };

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

impl VecServer {
    fn new() -> Result<Self> {
        let config = Config::load(None)?;
        Ok(Self {
            tool_router: Self::tool_router(),
            config: Arc::new(config),
        })
    }
}

impl ServerHandler for VecServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "vec — semantic file search. Use `search` to find files by meaning, \
                 `context` to read lines around a result, `index_status` to check \
                 the index."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// run_server — public entry point called from main.rs
// ---------------------------------------------------------------------------

/// Start the MCP server using stdio transport.
///
/// Reads JSON-RPC messages from stdin, writes responses to stdout.
/// Blocks until the client closes the connection.
pub async fn run_server() -> Result<()> {
    use rmcp::service::RunningService;
    use rmcp::RoleServer;

    let server = VecServer::new()?;

    let service: RunningService<RoleServer, VecServer> = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP server initialise error: {e}"))?;

    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server join error: {e}"))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helper: load embedder with stub fallback (no stderr from within MCP tools
// since stderr may interfere with the JSON-RPC framing on some hosts)
// ---------------------------------------------------------------------------

fn load_embedder_for_mcp(cfg: &Config) -> Embedder {
    if let Ok(model_path) = cfg.resolve_model_path() {
        if let Ok(e) = Embedder::load(&model_path, cfg.embed.max_tokens) {
            return e;
        }
    }
    Embedder::stub(768)
}

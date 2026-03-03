# MCP Server

`vec serve` starts an MCP server over stdio, exposing semantic file search to any
MCP-compatible AI assistant (Claude Code, Cursor, Continue, etc.).

## Tools

### `search(query, limit?, path_filter?)`
Semantic search over the indexed corpus.
- `query` — natural language or code fragment
- `limit` — max results (default: 10)
- `path_filter` — restrict to paths matching this prefix or glob
- Returns: `[{path, start_line, end_line, score}]`

### `context(file_path, line, window?)`
Raw file content around a line.
- `file_path` — absolute path
- `line` — 1-based line number
- `window` — lines above/below to include (default: 10)
- Returns: `{path, start_line, end_line, content}`

### `index_status()`
Index health snapshot.
- Returns: `{db_path, model_name, file_count, chunk_count, last_indexed}`

## Prerequisites

The index must be populated before starting the server:

```bash
vec updatedb
```

## Claude Code

Add to `~/.claude.json` under `"mcpServers"`:

```json
{
  "mcpServers": {
    "vec": {
      "command": "vec",
      "args": ["serve"]
    }
  }
}
```

Restart Claude Code. The `vec` tools are then available in every session.

## Other MCP Clients

Any client that supports stdio transport works. Generic config:

```json
{
  "command": "vec",
  "args": ["serve"],
  "transport": "stdio"
}
```

Cursor: add under `mcp.servers` in `~/.cursor/mcp.json`.
Continue: add under `mcpServers` in `~/.continue/config.json`.

## Usage

After registration, prompt your assistant naturally:

> "Find where authentication errors are handled."

The assistant calls `vec.search("authentication error handling")`, receives ranked
results with file paths and line numbers, then calls `vec.context()` to read the
relevant snippets — without any file browsing or grep.

## Transport

`vec serve` speaks the MCP protocol over stdin/stdout. No network port is opened.
The client process manages the subprocess lifetime.

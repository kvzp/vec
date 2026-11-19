# AI Assistant Integration

vec integrates with AI assistants via two methods: an **MCP server** (tool-based, always-available) and a **Claude Code skill** (lightweight, on-demand). Both can be configured for autonomous use — where the assistant reaches for vec without being asked.

This guide covers full installation for each method and how to combine them.

---

## Prerequisites

Before setting up any integration, vec must be installed and indexed:

```bash
# Verify vec is installed
vec --version

# Verify the index is populated
vec status

# If not indexed yet:
sudo vec updatedb       # system-wide install
vec updatedb            # userland (~/.config/vec/config.toml is read automatically)
```

If vec is not installed at all, see [README.md](README.md) for build and install instructions (system-wide or userland).

---

## Method 1: MCP Server (`vec serve`)

`vec serve` starts an MCP server over stdio, exposing semantic file search as programmatic tools to any MCP-compatible assistant.

### MCP Tools

Search and ranking are the CLI's (same path boost, same filters). The server
loads the model at most once, on the first query that needs it, and uses the
embedding daemon when it serves the index's model. A failure comes back as an
error result (`isError: true`) whose text says what to fix (e.g. a model
mismatch asks for `vec updatedb --full`; every case: [FAILURE-ZONE.md](FAILURE-ZONE.md#mcp-server)). Relative paths are resolved against
the server's working directory.

#### `search(query?, queries?, limit?, path_filter?, exclude?, min_score?)`
Semantic search over the indexed corpus.
- `query` / `queries` — one or more natural-language or code queries (at least one; at most `[search] max_queries`); results are merged, best score per chunk
- `limit` — max results (default: `[search] default_limit`)
- `path_filter` — only files equal to or under this path
- `exclude` — drop files equal to or under these paths
- `min_score` — minimum cosine similarity, -1.0 to 1.0 (default: `[search] min_score`)
- Returns: `[{path, line, start_line, end_line, score, snippet}]` — `line` is the chunk's line that best matches the query words, `start_line`/`end_line` the chunk, `snippet` up to `[search] snippet_lines` lines before and after `line` read from the live file (`>` marks `line`)

#### `similar(file_path, line, limit?)`
Chunks similar to the indexed chunk covering `file_path:line` (uses its stored vector; no model load). `file_path` must be readable by the user running the server.
- Returns: the same items as `search`

#### `explain(file_path, line)`
How a location is indexed. `file_path` must exist and be readable by the user running the server.
- Returns: `{path, line, chunks: [{start_line, end_line, byte_offset, byte_end}], indexed_hash, changed_since_indexed}`

#### `context(file_path, line, window?)`
File content around a line, for **indexed files only** (also: readable by the user running the server, a regular file, at most `[index] max_file_size` bytes) — a prompt-injected client cannot read arbitrary files.
- `window` — lines above/below (default: `[search] context_window`)
- Returns: annotated source lines with `>` marking the target

#### `index_status(probe_daemon?)`
Index health snapshot. The embedding daemon is not contacted unless `probe_daemon` is `true` (connecting starts a socket-activated daemon, which loads the model); by default `daemon` only says whether its socket exists.
- Returns: `{db_path, db_size_bytes, schema_version, supported_schema_version, files, chunks, index_model: {name, dim, sha256} | null, backend, configured_model, model_path, model_error, model_mismatch, last_updatedb, watcher: {last_flush, last_success, last_error, last_paths, watched_dirs, watch_failures, pending, overflows, rescans, reloads} | null, daemon}`. Times are Unix seconds. `daemon` is one of `{state: "not_probed", socket_exists}`, `{state: "not_running"}`, `{state: "running", identity, usable}`, `{state: "error", message}`, `{state: "unsupported"}`.

### MCP Setup

#### Claude Code

Add to `~/.claude.json`:

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

Restart Claude Code. The `vec` tools are available in every session.

#### Other MCP Clients

Any client that supports stdio transport works:

```json
{
  "command": "vec",
  "args": ["serve"],
  "transport": "stdio"
}
```

- **Cursor:** add under `mcp.servers` in `~/.cursor/mcp.json`
- **Continue:** add under `mcpServers` in `~/.continue/config.json`

### MCP Transport

`vec serve` speaks the MCP protocol over stdin/stdout. No network port is opened. The client process manages the subprocess lifetime.

---

## Method 2: Claude Code Skill (`/vec`)

A `/vec` skill invokes `vec` directly via the CLI — no MCP server process needed. Lower token overhead because the skill only loads into context when invoked, rather than keeping tool schemas in context permanently.

### Skill Installation

The skill lives in `.claude/skills/vec/SKILL.md`. Install it at the scope you need:

```bash
# Global — available in all projects (recommended)
mkdir -p ~/.claude/skills/vec
cp .claude/skills/vec/SKILL.md ~/.claude/skills/vec/

# Per-project — available only in that repo
mkdir -p /path/to/project/.claude/skills/vec
cp .claude/skills/vec/SKILL.md /path/to/project/.claude/skills/vec/
```

After copying, restart Claude Code (or start a new session). The skill appears in the `/` menu.

### Skill Usage

Type `/vec` followed by your query:

```
/vec authentication middleware
/vec database connection pooling
/vec where does error handling happen
```

The skill instructs Claude to run `vec` via Bash, interpret the results, and optionally read context around matches using the Read tool.

### Userland Note

The skill runs `vec` from `$PATH`; a userland install reads `~/.config/vec/config.toml` automatically. If your binary is elsewhere (e.g. `~/.local/bin` not in `$PATH`), edit the skill's `SKILL.md`.

---

## MCP vs Skill: When to Use Which

| | MCP (`vec serve`) | Skill (`/vec`) |
|--|--|--|
| Token cost | Higher (schema always in context) | Lower (loaded on demand) |
| Multi-step agent loops | Better (programmatic tool calls) | Manual |
| Autonomous use | Native (tool always visible) | Requires hook or instruction |
| Setup | Config in `~/.claude.json` | Copy `SKILL.md` to skills dir |
| Requires running process | Yes (`vec serve` subprocess) | No |
| Works with non-Claude clients | Yes (any MCP client) | No (Claude Code only) |

**Recommendation:** Use the skill for interactive sessions where you search occasionally. Use MCP when building agent workflows that need to search programmatically in loops.

---

## Making Claude Use vec Autonomously

By default, Claude won't proactively use `/vec` unless prompted. There are four methods to change this, listed from most to least reliable.

### Method 1: PreToolUse Hook (most reliable)

A hook fires programmatically every time Claude is about to use a matching tool. It injects a reminder directly into the conversation context — impossible to forget or drift from.

Add to `~/.claude/settings.json` (global) or `.claude/settings.local.json` (per-project):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Grep|Agent",
        "hooks": [
          {
            "type": "command",
            "command": "echo 'Before using grep or spawning an agent for code search: consider using /vec for semantic search. It finds code by meaning and is faster than grep when searching by concept rather than exact string. Use /vec for intent-based search, Grep for exact pattern matches.'"
          }
        ]
      }
    ]
  }
}
```

**How it works:** Every time Claude is about to call `Grep` or spawn an `Agent` for search, the hook intercepts and prints a reminder. Claude sees this message in-context and decides whether vec is more appropriate.

**Scope options:**
- `~/.claude/settings.json` — all projects on this machine
- `.claude/settings.json` — this project, committed to git (team-wide)
- `.claude/settings.local.json` — this project only, not committed to git

**Pros:** Fires at exactly the right moment. Cannot be forgotten. No token overhead until triggered.
**Cons:** Adds a small message to context on every Grep/Agent call, even when Grep is the right tool.

### Method 2: SessionStart Hook

A hook that fires once at the start of every Claude Code session, reminding Claude that vec exists.

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo 'This project has vec installed for semantic code search. Use /vec to find code by meaning (e.g., /vec authentication middleware). Prefer /vec over Grep when searching by concept or intent.'"
          }
        ]
      }
    ]
  }
}
```

**Pros:** One-time cost — only fires once per session. Sets expectations early.
**Cons:** Can drift over long conversations as the reminder scrolls out of context.

### Method 3: CLAUDE.md Instruction

Add to your project's `CLAUDE.md` (or `~/.claude/CLAUDE.md` for global):

```markdown
## Semantic Search

This project has `vec` installed. Use `/vec <query>` for semantic code search
when looking for code by concept or intent (e.g., "authentication middleware",
"error handling", "where does X happen"). Use Grep only for exact string/regex matches.
```

**Pros:** No JSON config needed — just a markdown file. Documents the convention for human readers too.
**Cons:** Claude can drift from CLAUDE.md instructions over very long conversations. Less reliable than hooks.

### Method 4: Skill Description (least reliable)

The skill's `description` field in `SKILL.md` tells Claude when to auto-invoke it:

```yaml
description: Semantic file search — find files by meaning, not just name. Use when
  looking for code by concept ("authentication middleware", "error handling") or when
  grep/glob aren't finding what you need.
```

With `disable-model-invocation` not set (the default), Claude is *allowed* to auto-invoke the skill when a task matches the description.

**Pros:** Zero configuration beyond the skill itself.
**Cons:** Least reliable — Claude doesn't proactively scan available skills mid-task. Works better as a supplement to other methods.

---

## Recommended Full Setup

Combine methods for maximum reliability. Complete steps from scratch:

### 1. Install the skill (global)

```bash
mkdir -p ~/.claude/skills/vec
cp .claude/skills/vec/SKILL.md ~/.claude/skills/vec/
```

### 2. Add the PreToolUse hook (global)

Edit `~/.claude/settings.json` and add to the `hooks` object:

```json
"PreToolUse": [
  {
    "matcher": "Grep|Agent",
    "hooks": [
      {
        "type": "command",
        "command": "echo 'Before using grep or spawning an agent for code search: consider using /vec for semantic search. It finds code by meaning and is faster than grep when searching by concept rather than exact string. Use /vec for intent-based search, Grep for exact pattern matches.'"
      }
    ]
  }
]
```

If you already have `PreToolUse` hooks, add this as another entry in the array.

### 3. Add a CLAUDE.md instruction (optional, per-project)

Add to your project's `CLAUDE.md`:

```markdown
## Semantic Search

This project has `vec` installed. Use `/vec <query>` for semantic code search
when looking for code by concept or intent. Use Grep only for exact string/regex matches.
```

### 4. Verify

Start a new Claude Code session and type:

```
/vec authentication middleware
```

Claude should run `vec`, return ranked results, and offer to read context around matches.

---

All three methods are idempotent and complement each other. The hook is the safety net; the CLAUDE.md is the instruction; the skill is the implementation.

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
vec updatedb            # system-wide install
# or for userland:
vec --config ~/.config/vec/config.toml updatedb
```

If vec is not installed at all, see [README.md](README.md) for build and install instructions (system-wide or userland).

---

## Method 1: MCP Server (`vec serve`)

`vec serve` starts an MCP server over stdio, exposing semantic file search as programmatic tools to any MCP-compatible assistant.

### MCP Tools

#### `search(query, limit?, path_filter?, min_score?)`
Semantic search over the indexed corpus.
- `query` — natural language or code fragment
- `limit` — max results (default: from config)
- `path_filter` — restrict to paths matching this prefix
- `min_score` — minimum cosine similarity (0.0-1.0)
- Returns: `[{path, start_line, end_line, score, snippet}]`

#### `context(file_path, line, window?)`
Raw file content around a line.
- `file_path` — absolute path
- `line` — 1-based line number
- `window` — lines above/below to include (default: 10)
- Returns: annotated source lines with `>` marking the target

#### `index_status()`
Index health snapshot.
- Returns: `{file_count, chunk_count, db_path, model, model_found}`

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

If vec is installed as userland (not in `$PATH`), the skill calls `~/.local/bin/vec --config ~/.config/vec/config.toml`. If your binary or config is elsewhere, edit the skill's `SKILL.md` to match your paths.

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

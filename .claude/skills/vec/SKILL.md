---
name: vec
description: Semantic file search — find files by meaning, not just name. Use when looking for code by concept ("authentication middleware", "error handling", "database pooling") or when grep/glob aren't finding what you need.
argument-hint: <query> [--path dir] [--limit N] [--min-score 0.8]
allowed-tools: Bash(*), Read
---

# vec — Semantic File Search

You have access to `vec`, a semantic search tool that finds files by meaning using vector embeddings. It works like `locate` but for concepts instead of filenames.

## How to search

Run `vec` directly via Bash. The output is `file:line` paths, one per line — pipe-friendly.

```bash
# Basic search (default: 10 results, paths only — lightweight)
vec "authentication middleware"

# Limit results
vec "error handling" --limit 5

# Scope to a directory
vec "database connection" --path ~/projects/backend

# Filter by minimum relevance score
vec "auth logic" --min-score 0.82
```

vec auto-detects `~/.config/vec/config.toml` for userland installs. No `--config` flag needed.

## Token-conscious workflow

**DO NOT use --snippet by default.** It adds ~7 lines per result — with 10 results that's 70+ extra lines in context.

Instead, follow this two-step approach:

1. **Search without --snippet** — get ranked `file:line` paths (minimal tokens)
2. **Read only the interesting results** — use the Read tool with offset/limit to view just the relevant lines

This way you only load code you actually need into context.

Only use `--snippet` if the user explicitly asks for inline snippets, or if you need a quick overview of a single result (`--limit 1 --snippet`).

## Checking index health

```bash
vec status
```

## When to use vec vs grep/glob

| Need | Tool |
|------|------|
| Exact string or regex | Grep |
| File by name pattern | Glob |
| Code by **concept** or **intent** | **vec** |
| "Where does X happen?" | **vec** |
| "How is X implemented?" | **vec** |

## Important

- vec searches the **pre-built index** — if files were just created, they may not appear until the next `vec updatedb` or the watcher picks them up
- The query is natural language — write what you mean, not keywords
- vec never sends data over the network — all embedding runs locally

## Responding to the user

After running the search:
1. Present the most relevant results (top 3–5 unless more were requested)
2. Use the Read tool to show context around the best matches — DO NOT use --snippet for this
3. If no results: suggest rephrasing the query or checking `vec status`

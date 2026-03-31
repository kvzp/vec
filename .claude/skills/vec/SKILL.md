---
name: vec
description: Semantic file search — find files by meaning, not just name. Use when looking for code by concept ("authentication middleware", "error handling", "database pooling") or when grep/glob aren't finding what you need.
argument-hint: <query> [--path dir] [--limit N] [--min-score 0.8] [--snippet]
allowed-tools: Bash(*), Read
---

# vec — Semantic File Search

You have access to `vec`, a semantic search tool that finds files by meaning using vector embeddings. It works like `locate` but for concepts instead of filenames.

## CRITICAL: Always use --config

vec requires the `--config` flag to find the userland database. **Every** vec command must include it:

```
vec --config ~/.config/vec/config.toml <args>
```

Without `--config`, vec tries `/var/lib/vec` (system path) and fails with "Permission denied".

## How to search

Run `vec` directly via Bash. The output is `file:line` paths, one per line — pipe-friendly.

```bash
# Basic search
vec --config ~/.config/vec/config.toml "authentication middleware"

# Limit results
vec --config ~/.config/vec/config.toml "error handling" --limit 5

# Scope to a directory
vec --config ~/.config/vec/config.toml "database connection" --path ~/projects/backend

# Show code snippets inline
vec --config ~/.config/vec/config.toml "cache invalidation" --snippet

# Filter by minimum relevance score
vec --config ~/.config/vec/config.toml "auth logic" --min-score 0.82

# Combine flags
vec --config ~/.config/vec/config.toml "payment processing" --path ~/projects --limit 10 --snippet --min-score 0.75
```

## Reading context around a result

When a result looks promising, read the surrounding code to understand it:

```bash
# vec returned /home/user/src/auth.rs:42 — read around that line
```

Use the Read tool with offset/limit to view context around the matched line.

## Checking index health

```bash
vec --config ~/.config/vec/config.toml status
```

Reports: file count, chunk count, DB path, model info, last update.

## When to use vec vs grep/glob

| Need | Tool |
|------|------|
| Exact string or regex | Grep |
| File by name pattern | Glob |
| Code by **concept** or **intent** | **vec** |
| "Where does X happen?" | **vec** |
| "How is X implemented?" | **vec** |

## Interpreting results

- Results are ranked by cosine similarity (0.0–1.0)
- Higher score = more semantically relevant
- Results are filtered by filesystem permissions — only files readable by the current user appear
- If `--snippet` is used, the matching chunk text is shown inline

## Important

- vec searches the **pre-built index** — if files were just created, they may not appear until the next `vec updatedb` or the watcher picks them up
- The query is natural language — write what you mean, not keywords
- vec never sends data over the network — all embedding runs locally

## Responding to the user

After running the search:
1. Present the most relevant results (top 3–5 unless more were requested)
2. If using `--snippet`, quote the relevant code
3. If the user needs more context, use Read on the specific file and line
4. If no results: suggest rephrasing the query or checking `vec status`

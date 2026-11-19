# vec — Semantic locate

> `locate` finds files by name. `vec` finds files by meaning.

## The problem

`locate auth` finds files *named* `auth*`. `grep -r authentication` finds the
literal string. Neither finds the code that *does* authentication when it is
called something else. Semantic search does, but most tools for it are developer
add-ons (Python runtimes, cloud APIs, per-project indexes) that are not built
for multi-user machines or system packaging.

## Positioning

vec is infrastructure, in the spirit of `locate`:

| locate | vec | role |
|--------|-----|------|
| `updatedb` | `vec updatedb` | build / update the index |
| `mlocate.db` | `/var/lib/vec/vec.db` | central index (paths + vectors, no text) |
| `/etc/updatedb.conf` | `/etc/vec.conf` | what to index |
| cron | systemd timer + `vec watch` | keep it fresh |

`locate` can share one database because it stores paths only. vec keeps that
property: it stores vectors and byte offsets, never text, and checks each result
against the caller's file permissions. Embedding runs locally on the CPU.

## Mental model

```
locate "filename pattern"    →  matching paths
vec    "concept or question" →  relevant paths + lines, ranked by similarity
```

The heavy work (embedding) happens at index time; a query embeds one string and
scans the stored vectors.

## Scope

In: semantic search over local text files for shells, editors and AI agents
(MCP); system-wide and per-user installs; incremental and real-time indexing;
distro packaging.

Out (for now): language-aware parsing (tree-sitter), hosted/remote indexes,
non-Linux platforms as tested targets. See [TODO.md](TODO.md).

Where to go next: [README.md](README.md) (usage), [ARCHITECTURE.md](ARCHITECTURE.md),
[SECURITY.md](SECURITY.md).

# Security Model

## The Core Property

**The vec database contains no source code.**

It stores: file paths, byte offsets, and embedding vectors.

Embedding vectors encode semantic meaning as floats. You cannot reconstruct source text from them. If someone obtains `vec.db`, they learn which files exist and how many chunks each has — nothing else.

Snippets are read from the live source file at display time, subject to normal filesystem permissions.

This property is verifiable without trusting the application: inspect the schema, confirm there is no `content` column in `chunks`.

---

## Implementation Language

vec is written in Rust. Memory safety is enforced at compile time — no buffer overflows, no use-after-free, no data races.

---

## Database Security

Central system-wide DB at `/var/lib/vec/vec.db`, mode `644`, owned by root.

Readable by all users. Contains no source text — only paths, byte offsets, and embedding vectors. Per-user access enforcement happens at query time via `access(path, R_OK)`.

---

## Access Control Flow

No daemon. The CLI runs as the user and calls `access(path, R_OK)` on every result before displaying it:

```
user runs: vec "query"
    → embed query
    → cosine similarity → top-k hits from DB
    → for each hit:
          access(path, R_OK)    # checked as the running user
          if denied: drop silently
    → read snippet from live file
    → stdout
```

The filesystem is the single source of truth. No separate ACL. No user list in the DB. No daemon.

### Revoking Access

Revoke filesystem read on the relevant paths. The user's next `vec` query will fail the `access()` check for those files and silently drop them — no index changes needed.

---

## What to Exclude from Indexing

`vec` respects `.gitignore` automatically. Additionally, configure exclusions in `/etc/vec.conf`:

```toml
[index]
exclude_files = [".env", ".env.*", "*.key", "*.pem", "*.p12", "*.pfx",
                 "secrets.*", "credentials.*"]
exclude_dirs  = [".secrets", "private"]
```

Files matching these patterns are never indexed — no embedding, no entry in the DB.

**Recommended exclusions for teams:**
- Environment files: `.env`, `.env.*`
- Key material: `*.key`, `*.pem`, `*.p12`, `*.pfx`
- Secret stores: `*.vault`, `secrets/`, `credentials/`

---

## Embedding Model Security

### Embedding backend

Embeddings run in-process via `tract-onnx` — pure Rust, no network calls, no external service. Text never leaves the process. The ONNX model is loaded from a local file at startup; no runtime download.

---

## Threat Model

| Threat | Mitigated? | How |
|--------|------------|-----|
| DB file stolen | Yes | No source text in DB; vectors are non-invertible |
| Unauthorized result shown | Yes | `access(path, R_OK)` checked on every result before display |
| Stale access after permission change | Yes | `access()` checked live on every query — no caching |
| Sensitive file indexed accidentally | Partial | `.gitignore` + exclude config; defense is configuration |
| Local embedding leaks data | N/A | tract-onnx runs in-process; no network calls, no external service |

## Out of Scope

- Encryption at rest (the DB is non-sensitive by design; if required, use full-disk encryption)
- Authentication beyond Unix credentials (use VPN/firewall on multi-tenant hosts)
- Audit logging (not implemented in MVP; add as needed)

# Results — 2026-11-20

Corpora: vec at the 0.1.0 tree (89 files, 742 chunks) and ripgrep `48242a7`
(216 files, 2,498 chunks), chunked like vec (40 lines, 10 overlap). 20 queries
each. **hit@k** = queries whose expected file is among the first k files;
**MRR** = mean reciprocal rank of the first expected file (top 10).

| Method | vec hit@1/3/10 | ripgrep hit@1/3/10 | hit@10 total (/40) | MRR (mean) |
|---|---|---|---|---|
| Ollama `nomic-embed-text-v2-moe` (vec today) | 9 / 11 / 17 | 7 / 14 / 18 | 35 | 0.547 |
| same, with `search_query:`/`search_document:` prefixes | 8 / 11 / 17 | 7 / 13 / 18 | 35 | 0.530 |
| keyword (FTS5 BM25, vec tokens, stop words) | 8 / 11 / 19 | 8 / 11 / 16 | 35 | 0.516 |
| **Ollama + keyword** (RRF) | 7 / 13 / 19 | 8 / 13 / 18 | **37** | 0.546 |
| static, distilled from `nomic-embed-text-v2-moe` (128 MB) | 2 / 4 / 8 | 2 / 5 / 9 | 17 | 0.198 |
| static nomic + keyword | 4 / 10 / 16 | 7 / 9 / 16 | 32 | 0.424 |
| static, distilled from `mxbai-embed-large-v1` (15 MB) | 6 / 8 / 12 | 6 / 11 / 14 | 26 | 0.415 |
| **static mxbai + keyword** | 7 / 12 / 16 | 8 / 13 / 17 | **33** | 0.512 |

Embedding time for all chunks of ripgrep: Ollama 177 s; static models 0.7–1.3 s
(Python, same machine): a static model indexes about 150–250× faster.

## Reading

- **Static from nomic v2 is unusable** (17/40). It is a multilingual
  mixture-of-experts model with a 250k-token vocabulary; one vector per token,
  without context, loses most of what it does. Also 128 MB.
- **Static from mxbai is the usable one**: 15 MB (30k-token English
  vocabulary, 256 dimensions, f16). Alone it is clearly behind (26/40);
  **with keyword search it reaches 33/40**, against 35 for vec today.
- **Keyword search alone matches today's vec** (35/40) on these sets, and
  fusing it with Ollama is the best result (37/40). About 6 queries per set
  name identifiers on purpose, which favours keywords; even so, keyword search
  is the cheapest gain for every backend.
- **nomic task prefixes do not help** (35/40, lower MRR): drop that TODO.
- 20 queries per corpus: differences of 1–2 are noise; 17 vs 35 is not.

## Decision

1. Add keyword search (SQLite FTS5) fused with the vector ranking: helps every
   backend, no new dependency.
2. Add a built-in static backend (mxbai-distilled, 15 MB, compiled into the
   binary): no Ollama, no ONNX runtime, indexing in seconds, vectors that can
   never drift. Default for new installs; Ollama stays the high-quality option.
3. Later: improve the static model (vocabulary from code, training it on
   vec's own chunks with the teacher's vectors, e.g. Tokenlearn) and re-run
   this benchmark.

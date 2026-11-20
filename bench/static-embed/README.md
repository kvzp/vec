# Retrieval benchmark: static embeddings, keyword search, Ollama

Measures which retrieval method puts the right file in the top results for
natural-language questions about a codebase. Used to decide whether vec can
ship a model inside the binary (no Ollama, no ONNX runtime).

- `bench.py` — the harness (Python; `numpy`, `model2vec`, SQLite FTS5).
- `bench.toml` — every setting (chunking, Ollama model, stop words, fusion).
- `queries-vec.toml`, `queries-ripgrep.toml` — 20 questions each with the
  file(s) that answer them, written by a separate agent that read the code
  (about 6 per set name an exact identifier or message on purpose).
- [RESULTS.md](RESULTS.md) — results and the decision.

Run:

```bash
python -m venv venv && . venv/bin/activate
pip install torch --index-url https://download.pytorch.org/whl/cpu
pip install "model2vec[distill]" sentence-transformers einops
mkdir corpus && git clone https://github.com/kvzp/vec corpus/vec \
  && git clone https://github.com/BurntSushi/ripgrep corpus/ripgrep
python -c 'from model2vec.distill import distill; distill("mixedbread-ai/mxbai-embed-large-v1", pca_dims=256).save_pretrained("mxbai-static")'
VEC_BENCH_OLLAMA_URL=http://localhost:11434 python bench.py bench.toml corpus \
  queries-vec.toml queries-ripgrep.toml --static mxbai-static --cache cache
```

`nomic-embed-text-v2-moe` cannot be distilled as is: its remote code lacks
`get_input_embeddings`; load it with `AutoModel`, point that method at
`model.embeddings.word_embeddings`, then call
`model2vec.distill.distillation.distill_from_model`.

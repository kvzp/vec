#!/usr/bin/env python3
"""Retrieval benchmark for vec: which embedding approach finds the right file?

Usage: bench.py CONFIG CORPUS_DIR QUERIES.toml [QUERIES.toml...] --static DIR [--methods a,b] [--cache DIR]

Each QUERIES file is [[query]] text/expect (paths relative to CORPUS_DIR/<stem>,
where <stem> is the part of the file name after "queries-"). Methods:
  ollama           vec today: Ollama embeddings, no task prefixes
  ollama_prefixed  the same model with nomic task prefixes
  keyword          SQLite FTS5 BM25 over vec-style word tokens
  static           Model2Vec static model (--static)
  static+keyword   reciprocal rank fusion of static and keyword
  ollama+keyword   reciprocal rank fusion of ollama and keyword
"""
import argparse, concurrent.futures as cf, json, os, re, sqlite3, sys, time, tomllib, urllib.request
from pathlib import Path
import numpy as np

WORD = re.compile(r"[A-Z]+(?=[A-Z][a-z])|[A-Z]?[^\W\d_A-Z]+|[A-Z]+|\d+")

def tokens(text):
    """vec's tokenizer: split at non-alphanumerics and camelCase, lowercase."""
    out = []
    for part in re.split(r"[\W_]+", text):
        if part:
            out += [t.lower() for t in WORD.findall(part)] or [part.lower()]
    return out

def walk(root, cfg):
    files = []
    for dirpath, dirs, names in os.walk(root):
        dirs[:] = sorted(d for d in dirs if d not in cfg["skip_dirs"])
        for n in sorted(names):
            p = Path(dirpath) / n
            if n in cfg["skip_files"] or p.is_symlink() or p.stat().st_size > cfg["max_file_bytes"]:
                continue
            data = p.read_bytes()
            if b"\0" in data:
                continue
            files.append((p.relative_to(root).as_posix(), data.decode("utf-8", "replace")))
    return files

def chunk(files, cfg):
    chunks = []
    step = cfg["size"] - cfg["overlap"]
    for path, text in files:
        lines = text.split("\n")
        for start in range(0, max(len(lines), 1), step):
            part = lines[start:start + cfg["size"]]
            if sum(1 for l in part if l.strip()) >= cfg["min_lines"] or (start == 0 and any(l.strip() for l in part)):
                chunks.append((path, start + 1, "\n".join(part)))
            if start + cfg["size"] >= len(lines):
                break
    return chunks

def normalize(m):
    m = np.asarray(m, dtype=np.float32)
    return m / np.maximum(np.linalg.norm(m, axis=1, keepdims=True), 1e-12)

def ollama_embed(texts, cfg, url):
    def one(batch):
        body = json.dumps({"model": cfg["model"], "input": batch}).encode()
        req = urllib.request.Request(url + cfg["embed_path"], body, {"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=cfg["timeout_s"]) as r:
            return json.load(r)["embeddings"]
    batches = [texts[i:i + cfg["batch"]] for i in range(0, len(texts), cfg["batch"])]
    with cf.ThreadPoolExecutor(cfg["concurrency"]) as ex:
        out = [v for b in ex.map(one, batches) for v in b]
    return normalize(out)

class Keyword:
    def __init__(self, chunks, stop):
        self.db = sqlite3.connect(":memory:")
        self.db.execute("create virtual table c using fts5(body, tokenize='unicode61')")
        self.db.executemany("insert into c(rowid, body) values (?, ?)",
                            ((i, " ".join(tokens(t))) for i, (_, _, t) in enumerate(chunks)))
        self.stop = set(stop)
    def search(self, q, depth):
        words = [w for w in dict.fromkeys(tokens(q)) if w not in self.stop]
        if not words:
            return []
        expr = " OR ".join('"' + w.replace('"', '') + '"' for w in words)
        return [r[0] for r in self.db.execute(
            "select rowid from c where c match ? order by bm25(c) limit ?", (expr, depth))]

def dense_search(qv, m, depth):
    return list(np.argsort(-(m @ qv))[:depth])

def rrf(lists, k, depth):
    score = {}
    for l in lists:
        for r, i in enumerate(l[:depth]):
            score[i] = score.get(i, 0.0) + 1.0 / (k + r + 1)
    return sorted(score, key=lambda i: -score[i])

def files_of(ranked, chunks, n):
    seen = []
    for i in ranked:
        p = chunks[i][0]
        if p not in seen:
            seen.append(p)
            if len(seen) == n:
                break
    return seen

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("config"); ap.add_argument("corpus"); ap.add_argument("queries", nargs="+")
    ap.add_argument("--static", default=None); ap.add_argument("--cache", default=None)
    ap.add_argument("--methods", default="ollama,ollama_prefixed,keyword,static,static+keyword,ollama+keyword")
    a = ap.parse_args()
    cfg = tomllib.loads(Path(a.config).read_text())
    methods = a.methods.split(",")
    url = os.environ.get("VEC_BENCH_OLLAMA_URL")
    if any(m.startswith("ollama") for m in methods) and not url:
        sys.exit("VEC_BENCH_OLLAMA_URL is not set (needed by the ollama methods)")
    if any(m.startswith("static") for m in methods):
        if not a.static:
            sys.exit("--static DIR is needed by the static methods")
        from model2vec import StaticModel
        static = StaticModel.from_pretrained(a.static)
    report = {}
    for qfile in a.queries:
        stem = Path(qfile).stem.removeprefix("queries-")
        root = Path(a.corpus) / stem
        qs = tomllib.loads(Path(qfile).read_text())["query"]
        for q in qs:
            for e in q["expect"]:
                if not (root / e).is_file():
                    sys.exit(f"{qfile}: expected file {e} does not exist under {root}")
        chunks = chunk(walk(root, cfg["corpus"]), cfg["chunk"])
        texts = [t for _, _, t in chunks]
        qtexts = [q["text"] for q in qs]
        timing, vecs = {}, {}
        def cached(name, fn):
            f = Path(a.cache) / f"{stem}-{name}.npy" if a.cache else None
            if f and f.exists():
                return np.load(f)
            t = time.time(); m = fn(); timing[name] = round(time.time() - t, 2)
            if f:
                f.parent.mkdir(parents=True, exist_ok=True); np.save(f, m)
            return m
        o = cfg["ollama"]
        if "ollama" in methods or "ollama+keyword" in methods:
            vecs["ollama"] = (cached("ollama-docs", lambda: ollama_embed(texts, o, url)), ollama_embed(qtexts, o, url))
        if "ollama_prefixed" in methods:
            vecs["ollama_prefixed"] = (cached("ollama-prefixed-docs", lambda: ollama_embed([o["document_prefix"] + t for t in texts], o, url)),
                                       ollama_embed([o["query_prefix"] + t for t in qtexts], o, url))
        if any(m.startswith("static") for m in methods):
            t = time.time(); d = normalize(static.encode(texts)); timing["static-docs"] = round(time.time() - t, 2)
            vecs["static"] = (d, normalize(static.encode(qtexts)))
        kw = Keyword(chunks, cfg["keyword"]["stop_words"])
        depth, n = cfg["fusion"]["depth"], cfg["eval"]["top_files"]
        res = {m: {"hits": {k: 0 for k in cfg["eval"]["ks"]}, "rr": 0.0, "misses": []} for m in methods}
        for qi, q in enumerate(qs):
            lists = {name: dense_search(qv[qi], d, depth) for name, (d, qv) in vecs.items()}
            lists["keyword"] = kw.search(q["text"], depth)
            for m in methods:
                ranked = rrf([lists[p] for p in m.split("+")], cfg["fusion"]["rrf_k"], depth) if "+" in m else lists[m]
                top = files_of(ranked, chunks, n)
                ranks = [top.index(e) + 1 for e in q["expect"] if e in top]
                best = min(ranks) if ranks else None
                for k in cfg["eval"]["ks"]:
                    res[m]["hits"][k] += int(best is not None and best <= k)
                res[m]["rr"] += 1.0 / best if best else 0.0
                if not best:
                    res[m]["misses"].append(q["text"])
        for m in methods:
            res[m]["mrr"] = round(res[m].pop("rr") / len(qs), 3)
        report[stem] = {"queries": len(qs), "files": len({c[0] for c in chunks}), "chunks": len(chunks), "timing_s": timing, "results": res}
    print(json.dumps(report, indent=1))

if __name__ == "__main__":
    main()

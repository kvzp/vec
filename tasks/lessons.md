# Lessons

## 2026-11-19 — editing user files from a script
- A bash heredoc whose body is interpolated JS text can lose its terminator (body without trailing newline) and append the following commands into the target file. Write files via base64 (`echo B64 | base64 -d > f.new`), validate `f.new`, then `mv` — never heredoc straight onto a live config.

## 2026-11-19 — "falls back to stub" is a data-loss bug, not a warning
- The live index had 18% stub vectors from Ollama outages. Detect them by cosine to the index mean (real ≥ 0.3, random ≈ 0, clean gap), not by journal history (journal was truncated).

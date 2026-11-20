# Lessons

## 2026-11-19 — editing user files from a script
- A bash heredoc whose body is interpolated JS text can lose its terminator (body without trailing newline) and append the following commands into the target file. Write files via base64 (`echo B64 | base64 -d > f.new`), validate `f.new`, then `mv` — never heredoc straight onto a live config.

## 2026-11-19 — "falls back to stub" is a data-loss bug, not a warning
- The live index had 18% stub vectors from Ollama outages. Detect them by cosine to the index mean (real ≥ 0.3, random ≈ 0, clean gap), not by journal history (journal was truncated).

## 2026-11-20 — the owner's machine is private, the repo is public
- github.com/kvzp/vec is PUBLIC. Never write anything from the owner's machine into the repo, a PR or a commit message: no home paths, no other project names, no file names or counts from their index, no hardware/OS details, no session scratch paths. Test fixtures and doc examples use invented names (`/work/TheApp`). Before every push: `git diff origin/main | grep -iE '/home/|~/|<names seen in their index>'`.
- The live index spans the owner's whole home: a demo query returns their other projects. Don't name or quote those as examples; demo on the vec repo itself, or say "files from your other projects" without naming them.

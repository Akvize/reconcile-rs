---
description: Searching or orienting in this repo before reading files
globs: ["**/*"]
---

Orient before you read. Measured on this repo: `rg -n Fingerprint` costs ~7186
tokens; `rg -c Fingerprint` or `rg -l Fingerprint` costs ~180 — a factor of 40 for
the same question ("where, and how much").

- Start with `rg -l <pattern>` (which files) or `rg -c <pattern>` (how many hits
  per file), not `rg -n` or a full `Read`, unless you already know you need every
  matching line.
- `.rgignore` already drops `Cargo.lock`/`SOTA.md`/`target/` from the
  default search surface; use `rg -u` to include them when a task is actually
  about one of them.
- `rg -t toml` includes `Cargo.lock` (a 53 KB file) — prefer `-g '*.toml'` or an
  explicit path when you mean the manifests, not the lockfile.
- For structural questions ("every `pub fn` returning `Result`", safe rewrites),
  prefer `ast-grep` or `rust-analyzer ssr` over a regex once a plain `rg` pattern
  would need to approximate syntax it doesn't understand — a regex silently misses
  generics/async variants rather than erroring.

# CLAUDE.md

@AGENTS.md

Everything above is imported verbatim from [`AGENTS.md`](./AGENTS.md), the source of truth for this
repo — read it, don't skim it. This file adds only what's specific to Claude Code sessions:

- AGENTS.md §3 runs via hooks on commit/push — don't replay it by hand; run CI-only checks only when relevant.
- Touching `rsos/`, `rbsr/`, or `lww-register/`? Re-read AGENTS.md §9 — `./scripts/check-domain-purity.sh`
  gates all three against infrastructure imports (manifest **and** source), and the gate fails the
  build, not just a warning.
- `gossip` deliberately does **not** depend on `lww-register`. If a change seems to need that edge,
  the code has probably landed in the wrong crate.
- Adding or touching a wire/domain value type? Re-read AGENTS.md §4 — strong-typed newtypes with
  type-owned validation, not bare primitives.
- Don't add prose-only guidelines here. A rule worth stating either belongs in `AGENTS.md` (source
  of truth) or, per AGENTS.md §10, in a script wired into CI and whichever hook tier it fits (§3).

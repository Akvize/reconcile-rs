# CLAUDE.md

@AGENTS.md

Everything above is imported verbatim from [`AGENTS.md`](./AGENTS.md), the source of truth for this
repo — read it, don't skim it. This file adds only what's specific to Claude Code sessions:

- Before reporting work done, run every command in AGENTS.md §3, not a subset — `cargo fmt --check`,
  `./scripts/check-domain-purity.sh`, both `cargo clippy` invocations, and the relevant `cargo test`
  invocations.
- Touching `rsos/src/hrtree.rs`, `rsos/src/hrtree_iter.rs`, `rsos/src/fingerprint.rs`,
  `src/reconcilable.rs`, `src/bounds.rs`, or `src/proto.rs`? Re-read AGENTS.md §9.2 — those files
  are gated on staying infrastructure-free, and the gate fails the build, not just a warning.
- Adding or touching a wire/domain value type? Re-read AGENTS.md §4 — strong-typed newtypes with
  type-owned validation, not bare primitives, is the established convention.
- Don't add prose-only guidelines here. A rule worth stating either belongs in `AGENTS.md` (source
  of truth) or, per AGENTS.md §10, in a script wired into `./pre-commit` and CI.

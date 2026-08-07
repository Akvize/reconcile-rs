# CLAUDE.md

@AGENTS.md

Everything above is imported verbatim from [`AGENTS.md`](./AGENTS.md), the source of truth for this
repo — read it, don't skim it. This file adds only what's specific to Claude Code sessions:

- Before reporting work done, run every command in AGENTS.md §3, not a subset — `cargo fmt --check`,
  `./scripts/check-domain-purity.sh`, both `cargo clippy` invocations, and the relevant `cargo test`
  invocations.
- Touching anything under `lww-register/src/`? Re-read AGENTS.md §9.2 — every file in that crate is
  gated on staying infrastructure-free by `./scripts/check-domain-purity.sh`, and the gate fails the
  build, not just a warning. Its `Cargo.toml` already blocks the crate-level edge; the script exists
  for what a manifest cannot see (an infrastructure type reached through a re-export). The `rsos` and
  `rbsr` crates (`rsos/src/fingerprint_tree_map.rs`, `rsos/src/fingerprint_tree_map_iter.rs`,
  `rsos/src/fingerprint.rs`, `rsos/src/aggregate.rs`, `rbsr/src/diff.rs`, `rbsr/src/rsos_view.rs`)
  hold the same invariant, enforced by their own `Cargo.toml` dependency lists rather than by grep —
  don't add an infrastructure dependency there either.
- `gossip` deliberately does **not** depend on `lww-register`: nothing in the transport/auth/replay/
  discovery layer knows what an `Entry`, a `Timestamp` or a `Key` is. If a change seems to need that
  edge, the code has probably landed in the wrong crate.
- Adding or touching a wire/domain value type? Re-read AGENTS.md §4 — strong-typed newtypes with
  type-owned validation, not bare primitives, is the established convention.
- Don't add prose-only guidelines here. A rule worth stating either belongs in `AGENTS.md` (source
  of truth) or, per AGENTS.md §10, in a script wired into `./pre-commit` and CI.

# AGENTS.md

Source of truth for any human or AI agent working in this repository, across tools (Claude Code,
Codex, Cursor, ...). Tool-specific files (`CLAUDE.md`) import this file and add nothing that
contradicts it — if they ever disagree, this document wins.

## 1. Project overview

`reconcile-rs` is a Rust library crate: an embedded, in-memory, eventually-consistent key-value
store whose replicas reconcile over UDP (HRTree + range fingerprint + anti-entropy gossip, LWW over
a Hybrid Logical Clock). Edition 2021, no MSRV pin currently declared.

For anything beyond a small, local change, read the relevant design doc first — don't duplicate
their content here:

- [`README.md`](./README.md) — usage, API, security model, deployment.
- [`ARCHITECTURE.md`](./ARCHITECTURE.md) — target hexagonal (ports & adapters) architecture and the
  migration plan (tracked in issue #138).
- [`PROGRESS.md`](./PROGRESS.md) — real-time correctness/security/maturity status.
- [`SOTA.md`](./SOTA.md) — durable state-of-the-art positioning, glossary, bibliography.

## 2. Environment setup

Plain `cargo` is all you need if you already have a Rust toolchain on `PATH`. Two containerized
setups are documented in [`CONTRIBUTING.md`](./CONTRIBUTING.md) for a reproducible environment: Dev
Container (`make dc-up`) or raw Docker (`make dev`).

Link the local pre-commit gate once (see §3):

```bash
ln -sf ../../pre-commit .git/hooks/pre-commit
```

## 3. Build, lint, and test commands

Exact commands, in the order CI runs them (`.github/workflows/main.yml`). Run all of them locally
before declaring work done — not a subset:

```bash
cargo fmt --check                                         # formatting
./scripts/check-domain-purity.sh                          # hexagonal boundary, see §9.2
cargo clippy --all --features internal-testing             # lint, default-relevant feature set
cargo clippy --all --all-features                          # lint, full feature set
cargo build --all
cargo test --all --features internal-testing               # unit + integration tests
cargo test --all --all-features
cargo test --doc --all --features internal-testing         # doctests
cargo bench --no-run --features internal-testing            # benches must still compile
cargo doc --all --all-features                              # docs must build warning-free
```

CI sets `RUSTFLAGS=-Dwarnings` and `RUSTDOCFLAGS=-Dwarnings`: a compiler or rustdoc warning fails
the build, not just a lint warning. Export the same env vars locally to reproduce that exactly, or
rely on `cargo clippy -- --deny warnings` (what `./pre-commit` runs) to catch most of it.

`./pre-commit` runs `cargo fmt --check`, `cargo clippy --all -- --deny warnings`, and
`./scripts/check-domain-purity.sh` against the staged tree on every commit — see §10 for why these,
and only these, are wired in as hard gates today.

## 4. Type safety and domain modeling

Conventions for how code in this crate should be shaped, beyond what rustfmt/clippy check.

**Strong-type every wire/domain entity — no bare primitives for meaningful values.** A value that
means something specific in the protocol or the domain (a sequence number, a timestamp, a key, a MAC
tag, a cluster secret) must be its own newtype, never passed around as a bare
`u64`/`[u8; N]`/`String`. Two bare `u64` parameters of the same type are a bug waiting to happen
(nothing stops a caller from swapping `seq` and `stamp`); two distinct newtypes make that swap a
compile error.

Precedent already in the crate: [`Timestamp`](./src/clock.rs) (HLC timestamp: `wall_ms`, `counter`,
`node_id`), [`ClusterKey`](./src/auth.rs) and [`Tag`](./src/auth.rs) (MAC key/output), [`Seq` and
`Stamp`](./src/replay.rs) (replay-header sequence number and sender wall-clock stamp). None of these
are exposed as raw integers or byte arrays past the boundary where they are parsed off the wire.

**Every entity owns its own validation.** The type that represents a value is the *only* place that
decides whether a given instance of that value is well-formed, in-range, or acceptable — never the
caller:

- Parsing/encoding lives on the type (`Seq::from_le_bytes`/`to_le_bytes`, not free functions that
  take/return `u64`).
- A check that is conceptually "is this value acceptable" is a method on the type, not arithmetic
  redone at each call site. Example: [`Stamp::is_fresh`](./src/replay.rs) is the *only* place the
  freshness-window comparison is written; [`ReplayFilter`](./src/replay.rs) calls it rather than
  reimplementing `now.saturating_sub(stamp) > window_ms` itself. If you find the same validation
  arithmetic duplicated at two call sites, that arithmetic belongs on the type instead.
- Constructing an invalid instance should be structurally impossible, or at least funneled through
  one obviously-fallible constructor — not something every caller has to remember to check.

This is the same "parse, don't validate" principle already called out for [`Payload`](./src/auth.rs)
(a `Payload` can only be obtained from `Authenticator::open`, so unauthenticated bytes can never
reach message handling) and for [`Entry`](./ARCHITECTURE.md#36-domain-types-and-conflict-policy)
(merge semantics live on the type, not scattered across call sites). Apply it to every entity, not
just the ones that happen to be security-critical.

**When adding a new wire field or protocol value:**

1. Give it a newtype in the module that owns its semantics (usually the module that already parses
   or generates it), not in the module that happens to consume it first.
2. Put its encode/decode and any acceptance/validation logic on that type.
3. Only reach for a bare primitive at the actual wire boundary (the byte array a `to_le_bytes`
   writes into) — everywhere else in the call chain, pass the typed value.

## 5. Code style guidelines

- `#![forbid(unsafe_code)]` (`src/lib.rs`) — no `unsafe` anywhere in this crate; this is a compile
  error, not a lint, so there is nothing to remember here beyond not fighting it.
- `cargo fmt` formatting is non-negotiable and gated (§3, §10); don't hand-format around it.
- `cargo clippy --deny warnings` is gated on both the default and `--all-features` feature sets
  (§3, §6) — a clippy warning under either is a build failure, not a style suggestion.
- Strong typing and type-owned validation (§4) are conventions, not yet mechanically gated — hold
  the line on them in review.
- Only style rules beyond rustfmt/clippy defaults: keep the domain mechanism free of infrastructure
  imports (§9.2), and treat `internal-testing`-gated APIs (`crate::testing`) as test-only, never
  reachable from non-test code paths (§6).

## 6. Feature flags

Declared in `Cargo.toml`. `mac-blake3` is the default MAC backend; `mac-hmac` is the alternative
(exactly one wins — `mac-blake3` if both are set; the build fails if neither is enabled). `zeroize`,
`encryption`, `metrics`, `metrics-prometheus` are opt-in. `internal-testing` exposes
`crate::testing` (the `pub(crate)` reconciliation seam) for the external test oracles and benches —
not part of the supported public API, never used outside `tests/` and `benches/`.

When you touch feature-gated code, run **both** the default-feature and `--all-features` variants of
`clippy`/`test` (§3) — CI runs both as separate, independently-required jobs specifically because
feature interactions are where bugs hide.

## 7. Testing instructions

- New behavior needs a test. Prefer `tests/*.rs` integration tests for anything crossing the public
  API; `#[cfg(test)]` unit tests for internal invariants.
- `tests/proptest_hrtree.rs` and `tests/fuzz_packets.rs` are property/fuzz-style oracles guarding the
  data structure and the wire format against malformed input — extend these rather than adding
  narrow example-based tests when a change touches parsing or HRTree invariants.
- Coverage is tracked via `cargo llvm-cov` and uploaded to Codecov (`CONTRIBUTING.md` has the local
  invocation), but `codecov.yml` currently sets both the project and patch checks
  `informational: true` — **coverage regressions do not block merge today.** Known gap relative to
  the "blocking gate" bar the rest of this document holds to (§10); don't treat a green Codecov
  comment as a guarantee, and don't rely on it to catch an untested change.

## 8. Security considerations

- By default the UDP protocol is **unauthenticated**: any host that can reach the port can forge an
  update and poison the cluster via last-write-wins. Production configs must set a shared 32-byte
  cluster key (`Config::with_cluster_key`) on every node — see README "Security model" for the full
  threat model. Never land a change that weakens this default-unauthenticated-but-loud-warning
  posture without updating that section.
- The cluster key is a **single shared secret** — no per-peer identity, no forward secrecy. Don't
  design features that assume otherwise (e.g. per-peer revocation) without flagging the gap.
- Never commit a real cluster key, credential, or secret-manager value — the README's key examples
  are placeholders (`/* same secret on all nodes, e.g. loaded from your secret manager */`); keep it
  that way in any doc or example you touch.
- `mirror.rs`/read-only mirrors and the `zeroize` feature exist specifically to reduce key exposure
  in memory; prefer extending them over adding new places that hold the raw key.
- Per-peer replay protection ([`src/replay.rs`](./src/replay.rs)) guards authenticated mode against
  captured-datagram replay; its state deliberately outlives peer membership (see the module docs) —
  don't "clean up" a decommissioned peer's replay-filter entry, that reopens the replay window.

## 9. Project structure and boundaries

### 9.1 Modules (`src/`)

See `ARCHITECTURE.md` §2.1 for the full table. The two structural groupings that matter for where
to put new code:

- **Domain** (data structure + protocol algorithm, infrastructure-free): `hrtree.rs`,
  `hrtree_iter.rs`, `fingerprint.rs`, `reconcilable.rs`, `bounds.rs`, `proto.rs`.
- **Infrastructure** (transport, codec, clock, discovery, auth/replay): `reconcile_engine.rs`,
  `reconcile_store.rs`, `clock.rs`, `discovery.rs`, `persistence.rs`, `auth.rs`, `replay.rs`,
  `observability.rs`, `prometheus.rs`.

### 9.2 Enforced boundary: the domain stays infrastructure-free

`ARCHITECTURE.md` §2.2/§3.3 documents that the domain mechanism carries no infrastructure
dependency: no async runtime, no socket, no wire codec, no wall clock. This is true today, and it is
load-bearing for the in-progress ports/adapters migration (#138) — not just a description.

`./scripts/check-domain-purity.sh` greps the §9.1 domain file list for `tokio`, `bincode`, `chrono`,
`ipnet`, `mio`, `reqwest`, `hyper`, `std::net` imports and fails if any is found. It runs in
`./pre-commit` and as a required CI step (§3). Widening or narrowing the domain set (a module newly
freed of infra deps, or reclassified as an adapter) means updating `DOMAIN_FILES` in the script
**and** `ARCHITECTURE.md` §2.1/§2.2 in the same change — the two must never drift apart.

### 9.3 Docs that change with the code vs. docs that don't

- `README.md`, `ARCHITECTURE.md` §1–§4, this file: update in the same PR as the code change they
  describe.
- `PROGRESS.md`: living status — update when a finding's status changes or a phase lands.
- `SOTA.md`: durable reference material (literature survey, glossary); not updated for routine code
  changes.

## 10. Commit, PR, and gating conventions

No enforced commit-message or PR-title format exists in this repo today (no template, no
conventional-commits lint) — match the style of recent history (`git log --oneline`) rather than
inventing a new one. Reference the tracking issue (`#NNN`) for anything that resolves or partially
addresses one, the way `PROGRESS.md` does.

The one convention that **is** enforced: if you find yourself writing a rule in a doc, a PR
description, or a code comment that a human is expected to remember and enforce by eye ("don't
import X here", "always update Y when you change Z"), prefer wiring it into `./pre-commit` and
`.github/workflows/main.yml` instead, the way §9.2 does. A guideline that only lives in prose
decays; a failing command doesn't. Not everything is tractable this way (architectural taste,
naming, API ergonomics) — use judgment — but import-boundary rules, required-file-pairs, and
structural invariants almost always are.

## 11. Publishing

`cargo publish --allow-dirty --dry-run` runs in CI to catch packaging breakage early (see the
`exclude` list in `Cargo.toml` — repository-only docs and `examples/k8s/` are deliberately excluded
from the published crate). Actual publishing happens from `.github/workflows/tags.yml` on `v*` tags;
never hand-run `cargo publish` outside that flow.

# AGENTS.md

Source of truth for any human or AI agent working here, across tools (Claude Code, Codex, Cursor,
...). Tool-specific files (`CLAUDE.md`) import this file and add nothing that contradicts it.

## 1. Project overview

`reconcile-rs` is a Cargo workspace: the `rsos` crate (`FingerprintTreeMap` + range fingerprint) is
a standalone leaf, `rbsr` holds the Range-Based Set Reconciliation algorithm generic over any `rsos`
backend, and the `reconcile` package is an embedded, in-memory, eventually-consistent key-value
store whose replicas reconcile over UDP (anti-entropy gossip over `rbsr`/`rsos`, LWW over a Hybrid
Logical Clock). Edition 2021, no MSRV pin.

For non-trivial changes, read first rather than duplicating here: [`README.md`](./README.md)
(usage/API/security/deployment), [`ARCHITECTURE.md`](./ARCHITECTURE.md) (target hexagonal
architecture + migration plan, issue #138), [`PROGRESS.md`](./PROGRESS.md) (live status),
[`SOTA.md`](./SOTA.md) (durable positioning/glossary/bibliography).

## 2. Environment setup

Plain `cargo` is enough with a Rust toolchain on `PATH`. [`CONTRIBUTING.md`](./CONTRIBUTING.md) also
documents a Dev Container (`make dc-up`) and raw Docker (`make dev`) for a reproducible environment.
Link the pre-commit gate once: `ln -sf ../../pre-commit .git/hooks/pre-commit`.

## 3. Build, lint, and test commands

Exact commands, in CI's order (`.github/workflows/main.yml`) — run all before declaring work done:

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

CI sets `RUSTFLAGS=-Dwarnings`/`RUSTDOCFLAGS=-Dwarnings`: warnings fail the build. `./pre-commit`
runs `cargo fmt --check`, `cargo clippy --all -- --deny warnings`, and
`./scripts/check-domain-purity.sh` on the staged tree before every commit (§10).

## 4. Type safety and domain modeling

- **Strong-type every wire/domain entity.** A value meaningful to the protocol or domain (sequence
  number, timestamp, key, MAC tag, cluster secret) is its own newtype, never a bare
  `u64`/`[u8; N]`/`String` — two same-typed primitives let a caller swap them silently; two newtypes
  make that a compile error. Precedent: [`Timestamp`](./src/clock.rs), [`ClusterKey`]/[`Tag`]
  (`src/auth.rs`), [`Seq`]/[`Stamp`] (`src/replay.rs`).
- **Every entity owns its own validation** — never the caller. Parsing/encoding lives on the type
  (`Seq::from_le_bytes`, not a free function); an "is this acceptable" check is a method on the type
  (e.g. [`Stamp::is_fresh`](./src/replay.rs)) — if the same validation arithmetic shows up at two
  call sites, it belongs on the type instead. Construction of an invalid instance should be
  structurally impossible or funneled through one fallible constructor. Same "parse, don't validate"
  principle as [`Payload`](./src/auth.rs) (only obtainable via `Authenticator::open`) and
  [`Entry`](./ARCHITECTURE.md#36-domain-types-and-conflict-policy) — apply it to every entity, not
  just the security-critical ones.
- **New wire field or protocol value:** newtype it in the module that owns its semantics; put
  encode/decode and validation on that type; only touch a bare primitive at the actual wire boundary.

## 5. Code style guidelines

- `#![forbid(unsafe_code)]` (`src/lib.rs`) — no `unsafe`, ever; this is a compile error, not a lint.
- `cargo fmt` and `cargo clippy --deny warnings` (both feature sets, §3/§6) are gated — not style
  suggestions.
- Strong typing and type-owned validation (§4) are conventions, not yet mechanically gated — hold
  the line in review.
- Keep the domain mechanism free of infrastructure imports (§9.2); treat `internal-testing`-gated
  `crate::testing` as test-only, never reachable from non-test code.

## 6. Feature flags

`mac-blake3` (default MAC backend) vs `mac-hmac` — exactly one wins (`mac-blake3` if both set; build
fails if neither). `zeroize`, `encryption`, `metrics`, `metrics-prometheus` are opt-in.
`internal-testing` exposes `crate::testing` (the `pub(crate)` reconciliation seam) for external test
oracles/benches only — not public API. Touching feature-gated code: run **both** the default and
`--all-features` variants of `clippy`/`test` (§3) — CI gates them as separate required jobs because
feature interactions hide bugs.

## 7. Testing instructions

- New behavior needs a test: `tests/*.rs` integration tests for public-API-crossing changes,
  `#[cfg(test)]` unit tests for internal invariants.
- `tests/proptest_fingerprint_tree_map.rs` / `tests/fuzz_packets.rs` are property/fuzz oracles for
  the data structure and wire format (the former drives `rbsr`/`rsos` directly, not via
  `reconcile::testing`) — extend these over narrow example tests when touching parsing or
  `FingerprintTreeMap` invariants.
- `cargo llvm-cov` coverage uploads to Codecov, but `codecov.yml` marks both checks
  `informational: true` — **coverage regressions do not block merge today** (a known gap vs. the
  gating bar the rest of this doc holds to, §10). Don't rely on a green Codecov comment as proof.

## 8. Security considerations

- UDP is **unauthenticated by default** — any host reaching the port can forge an update via
  last-write-wins. Production sets a shared 32-byte cluster key (`Config::with_cluster_key`) on
  every node (README "Security model"). Never weaken this default-unauthenticated-but-loud-warning
  posture without updating that section.
- The cluster key is a **single shared secret** — no per-peer identity, no forward secrecy; don't
  design features assuming otherwise without flagging the gap.
- Never commit a real key/credential — README's examples are placeholders, keep them that way.
- `mirror.rs` and the `zeroize` feature reduce key exposure in memory; extend them rather than adding
  new places that hold the raw key.
- Per-peer replay protection ([`src/replay.rs`](./src/replay.rs)) deliberately outlives peer
  membership (see module docs) — don't "clean up" a decommissioned peer's entry, that reopens replay.

## 9. Project structure and boundaries

**9.1 Modules** (full table: `ARCHITECTURE.md` §2.1). `rsos/src/fingerprint_tree_map.rs`,
`rsos/src/fingerprint_tree_map_iter.rs`, `rsos/src/fingerprint.rs`, `rsos/src/aggregate.rs` moved
to (or were added in) the standalone `rsos` crate (migration step 6 Step A), and `src/proto.rs` to
the standalone `rbsr` crate (Step B, now `rbsr/src/diff.rs` + `rbsr/src/rsos_view.rs`) — both
leaves with zero workspace/infrastructure dependency, enforced by their own minimal `Cargo.toml`s.
Within the `reconcile` package, **domain**, infrastructure-free: `entry.rs`, `bounds.rs`.
**Infrastructure**: `reconcile_engine.rs`, `reconcile_store.rs`, `clock.rs`, `discovery.rs`,
`persistence.rs`, `auth.rs`, `replay.rs`, `observability.rs`, `prometheus.rs`.

**9.2 Enforced boundary.** `ARCHITECTURE.md` §2.2/§3.3: the domain mechanism carries no
infrastructure dependency (no async runtime, socket, wire codec, wall clock) — true today, and
load-bearing for the in-progress ports/adapters migration (#138). `./scripts/check-domain-purity.sh`
greps the §9.1 domain files for `tokio`/`bincode`/`chrono`/`ipnet`/`mio`/`reqwest`/`hyper`/
`std::net` imports and fails if found; runs in `./pre-commit` and CI (§3). Widening/narrowing the
domain set means updating `DOMAIN_FILES` in the script **and** `ARCHITECTURE.md` §2.1/§2.2 together.

**9.3 Docs that change with code vs. not.** `README.md`, `ARCHITECTURE.md` §1–§4, this file: same PR
as the change they describe. `PROGRESS.md`: living status, update as findings/phases change.
`SOTA.md`: durable reference, not updated for routine changes.

## 10. Commit, PR, and gating conventions

No enforced commit-message/PR-title format exists — match recent history (`git log --oneline`)
rather than inventing one. Reference the tracking issue (`#NNN`) where relevant, like `PROGRESS.md`
does.

The one enforced convention: a rule a human must remember and enforce by eye ("don't import X
here", "always update Y with Z") belongs in `./pre-commit` and `.github/workflows/main.yml` instead,
the way §9.2 does. Prose-only guidelines decay; failing commands don't. Not everything is tractable
this way (architectural taste, naming, API ergonomics) — but import-boundary rules,
required-file-pairs, and structural invariants almost always are.

## 11. Publishing

`cargo package -p rsos --allow-dirty` runs in CI to catch packaging breakage (see `Cargo.toml`'s
`exclude` list — repo-only docs and `examples/k8s/` are excluded from the published crate). Actual
publishing happens from `.github/workflows/tags.yml` on `v*` tags; never hand-run `cargo publish`
outside that flow.

The check covers **`rsos` only** — not `rbsr`, and not the top-level `reconcile` package. Cargo
refuses to package any crate holding a path dependency that carries no version requirement, until
that dependency is itself on crates.io. `rsos` is the workspace's one true leaf (zero intra-workspace
dependencies), so it is the only crate that can be packaged today; `rbsr` depends on `rsos` by path
and `reconcile` on both, so both hit that wall. Whether and how to publish a multi-crate workspace
is an open policy question (#204) — until it is settled, do **not** "fix" this by inventing version
requirements for the internal crates or by publishing them ad hoc. The internal crates carry
`publish = false` deliberately; `cargo package` is used rather than `cargo publish --dry-run`
precisely so that guard can stay in place while still compiling the packaged crate. A new crate
becomes checkable here only once it has no unpublished path dependency of its own.

**Releases are blocked until #204 is settled.** `tags.yml`'s `cargo publish` would fail on a `v*`
tag today, for the same path-dependency reason — loudly, at release time, not silently. That is
deliberate: resolving it means deciding what the workspace publishes (one facade crate with the
internals vendored? every crate, versioned in lockstep? only some?), which is #204's question, not
something to patch around here.

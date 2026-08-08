# AGENTS.md

Source of truth for any human or AI agent working here, across tools (Claude Code, Codex, Cursor,
...). Tool-specific files (`CLAUDE.md`) import this file and add nothing that contradicts it.

## 1. Project overview

`reconcile-rs` is a six-crate Cargo workspace. `rsos` (`FingerprintTreeMap` + range fingerprint) is
a standalone leaf; `rbsr` holds the Range-Based Set Reconciliation algorithm generic over any `rsos`
backend; `lww-register` is the infrastructure-free LWW-Register CRDT domain; `gossip` is the network
adapter layer; `snapshot` is the file-backed persistence adapter; and the published `reconcile`
package is the facade over all five — an embedded, in-memory, eventually-consistent key-value store
whose replicas reconcile over UDP (anti-entropy gossip over `rbsr`/`rsos`, LWW over a Hybrid Logical
Clock). See §9.1 for the full map. Edition 2021, no MSRV pin.

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
cargo fmt --check                                          # formatting
./scripts/check-domain-purity.sh                           # hexagonal boundary, see §9.2
cargo clippy --workspace --features internal-testing        # lint, default-relevant feature set
cargo clippy --workspace --all-features                     # lint, full feature set
cargo build --workspace
cargo test --workspace --features internal-testing          # unit + integration tests
cargo test --workspace --all-features
cargo test --doc --workspace --features internal-testing    # doctests
cargo bench --no-run --features internal-testing            # benches must still compile
cargo doc --workspace --all-features                        # docs must build warning-free
```

`--workspace` throughout (not the deprecated `--all` alias): with six members, the distinction is
worth being unambiguous about. `.github/workflows/main.yml`, `./pre-commit` and this listing are
kept in sync — change one, change all three.

CI sets `RUSTFLAGS=-Dwarnings`/`RUSTDOCFLAGS=-Dwarnings`: warnings fail the build. `./pre-commit`
runs `cargo fmt --check`, `cargo clippy --workspace -- --deny warnings`, and
`./scripts/check-domain-purity.sh` on the staged tree before every commit (§10).

## 4. Type safety and domain modeling

- **Strong-type every wire/domain entity.** A value meaningful to the protocol or domain (sequence
  number, timestamp, key, MAC tag, cluster secret) is its own newtype, never a bare
  `u64`/`[u8; N]`/`String` — two same-typed primitives let a caller swap them silently; two newtypes
  make that a compile error. Precedent: [`Timestamp`](./lww-register/src/clock.rs), [`ClusterKey`]/[`Tag`]
  (`gossip/src/auth.rs`), [`Seq`]/[`Stamp`] (`gossip/src/replay.rs`).
- **Every entity owns its own validation** — never the caller. Parsing/encoding lives on the type
  (`Seq::from_le_bytes`, not a free function); an "is this acceptable" check is a method on the type
  (e.g. [`Stamp::is_fresh`](./gossip/src/replay.rs)) — if the same validation arithmetic shows up at two
  call sites, it belongs on the type instead. Construction of an invalid instance should be
  structurally impossible or funneled through one fallible constructor. Same "parse, don't validate"
  principle as [`Payload`](./gossip/src/auth.rs) (only obtainable via `Authenticator::open`) and
  [`Entry`](./ARCHITECTURE.md#36-domain-types-and-conflict-policy) — apply it to every entity, not
  just the security-critical ones.
- **New wire field or protocol value:** newtype it in the module that owns its semantics; put
  encode/decode and validation on that type; only touch a bare primitive at the actual wire boundary.

## 5. Code style guidelines

- `#![forbid(unsafe_code)]` on **every** crate root (`src/lib.rs` and all five workspace crates) —
  no `unsafe`, ever; this is a compile error, not a lint. Carry it onto any new crate root: the
  workspace split briefly lost it on `rsos`, because moving code out of the monolithic `src/lib.rs`
  silently drops the attribute that used to cover it.
- `cargo fmt` and `cargo clippy --deny warnings` (both feature sets, §3/§6) are gated — not style
  suggestions.
- Strong typing and type-owned validation (§4) are conventions, not yet mechanically gated — hold
  the line in review.
- Keep the domain mechanism free of infrastructure imports (§9.2); treat `internal-testing`-gated
  `crate::testing` as test-only, never reachable from non-test code.

## 6. Feature flags

`mac-blake3` (default MAC backend) vs `mac-hmac` — exactly one wins (`mac-blake3` if both set; build
fails with a `compile_error!` if neither). `zeroize`, `encryption`, `metrics`, `metrics-prometheus`
are opt-in. `internal-testing` exposes `crate::testing` (the `pub(crate)` reconciliation seam) for
external test oracles/benches only — not public API.

Since the workspace split, the flags live where their code lives: `mac-blake3`, `mac-hmac`,
`zeroize`, `encryption` and `dns-hickory` are **declared on `gossip`** (which owns `auth.rs` and
`discovery.rs`) and re-exposed from `reconcile` as unification entries
(`mac-blake3 = ["gossip/mac-blake3"]`). `metrics`/`metrics-prometheus` (for `observability.rs` /
`prometheus.rs`) and `internal-testing` stay on `reconcile`. `reconcile` depends on `gossip` with
`default-features = false`, so its own `default = ["mac-blake3"]` is the single place the MAC backend
gets chosen. `encryption` is both forwarded *and* read locally — `replica.rs`/`replicated_map.rs`
carry their own `#[cfg(feature = "encryption")]` arms.

Touching feature-gated code: run **both** the default and `--all-features` variants of
`clippy`/`test` (§3) — CI gates them as separate required jobs because feature interactions hide
bugs.

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
- `read_replica_map.rs` and the `zeroize` feature reduce key exposure in memory; extend them rather
  than adding new places that hold the raw key.
- Per-peer replay protection ([`gossip/src/replay.rs`](./gossip/src/replay.rs)) deliberately outlives peer
  membership (see module docs) — don't "clean up" a decommissioned peer's entry, that reopens replay.

## 9. Project structure and boundaries

**9.1 Crates and modules** (full table: `ARCHITECTURE.md` §2.1). Six workspace members, in
dependency order:

| Crate | Holds | Kind |
|---|---|---|
| `rsos` | `FingerprintTreeMap`, `Fingerprint`, `Aggregate`, the `Rsos<K>` trait (`fingerprint_tree_map.rs`, `fingerprint_tree_map_iter.rs`, `fingerprint.rs`, `aggregate.rs`) | leaf, zero workspace deps |
| `rbsr` | the RBSR diff walk + `RsosView<K>` (`diff.rs`, `rsos_view.rs`) | depends on `rsos` only |
| `lww-register` | `entry.rs`, `bounds.rs`, `clock.rs` (`Timestamp`/`Clock`/HLC ordering), `persistence.rs` (`Persistence`/`PersistedState`/`InMemoryPersistence`) | **domain**, infrastructure-free |
| `gossip` | `transport.rs`, `bincode.rs`, `auth.rs`, `replay.rs`, `discovery.rs`, `gen_ip.rs` | infrastructure; **no `lww-register` dep** — nothing there knows what an `Entry` is |
| `snapshot` | `FileSnapshot` + the versioned on-disk header | infrastructure; depends on `lww-register` |
| `reconcile` | `replica.rs`, `replicated_map.rs`, `read_replica_map.rs`, `clock.rs` (the chrono-reading `HlcClock` adapter), `observability.rs`, `prometheus.rs`, `timeout_wheel.rs` | the facade; depends on all five and re-exports their public types |

The `reconcile` package keeps re-export shims (`src/persistence.rs`, `src/clock.rs`, `pub use` in
`src/lib.rs`) so `reconcile::entry::Entry`, `reconcile::transport::UdpTransport`,
`reconcile::FileSnapshot` and friends resolve exactly as before the split.

**9.2 Enforced boundary.** `ARCHITECTURE.md` §2.2/§3.3: the domain mechanism carries no
infrastructure dependency (no async runtime, socket, wire codec, wall clock) — true today, and
load-bearing for the completed ports/adapters migration (#138).
`./scripts/check-domain-purity.sh` gates it in `./pre-commit` and CI (§3), in **two parts that
answer two different questions**:

1. **Manifest gate — "can the forbidden thing be reached at all?"** The
   `[dependencies]`/`[dev-dependencies]`/`[build-dependencies]` tables of `rsos/Cargo.toml`,
   `rbsr/Cargo.toml` and `lww-register/Cargo.toml` must name none of `tokio`, `bincode`, `chrono`,
   `ipnet`, `mio`, `reqwest`, `hyper`, `socket2`, `async-trait` (renamed `package = "…"` targets
   included). This is where the invariant is actually breakable: with the dependency undeclared,
   `use tokio::…` in those crates does not compile — so *adding the dependency* is the step that
   has to be caught. `gossip`, `snapshot` and the root `reconcile` package are deliberately out of
   scope; they are adapters and carry infrastructure deps by design.
2. **Source grep — "is an *allowed* dependency being used to reach something forbidden?"** Every
   `lww-register/src/*.rs` file is grepped for `tokio`/`bincode`/`chrono`/`ipnet`/`mio`/`reqwest`/
   `hyper`/`std::net` imports. A manifest cannot see this: an infrastructure type re-exported by an
   allowed dependency compiles fine and silently breaches the boundary. One documented carve-out:
   `std::net`'s plain address *value* types (`IpAddr` & co.) are allowed, since `PersistedState`'s
   membership set is a set of peer identities; socket types are not.

Neither part subsumes the other — part 1 is about the dependency edge, part 2 about how a permitted
edge is used. Widening/narrowing either set means updating `DOMAIN_FILES` / `STANDALONE_MANIFESTS`
in the script **and** `ARCHITECTURE.md` §2.1/§2.2 together.

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

`cargo package` runs in CI for `rsos`, `lww-register` and `gossip` to catch packaging breakage (see
`Cargo.toml`'s `exclude` list — repo-only docs and `examples/k8s/` are excluded from the published
crate). Actual publishing happens from `.github/workflows/tags.yml` on `v*` tags; never hand-run
`cargo publish` outside that flow.

The check covers **only the crates with no intra-workspace dependency**. Cargo refuses to package
any crate holding a path dependency that carries no version requirement, until that dependency is
itself on crates.io. `rsos`, `lww-register` and `gossip` each depend on nothing else in the
workspace, so all three can be packaged; `rbsr` (→ `rsos`), `snapshot` (→ `lww-register`) and the
top-level `reconcile` package (→ all five) hit that wall. Whether and how to publish a multi-crate
workspace is an open policy question (#204) — until it is settled, do **not** "fix" this by
inventing version requirements for the internal crates or by publishing them ad hoc. The internal
crates carry `publish = false` deliberately; `cargo package` is used rather than
`cargo publish --dry-run` precisely so that guard can stay in place while still compiling the
packaged crate. A new crate becomes checkable here only once it has no unpublished path dependency
of its own.

**Releases are blocked until #204 is settled.** `tags.yml`'s `cargo publish` would fail on a `v*`
tag today, for the same path-dependency reason — loudly, at release time, not silently. That is
deliberate: resolving it means deciding what the workspace publishes (one facade crate with the
internals vendored? every crate, versioned in lockstep? only some?), which is #204's question, not
something to patch around here.

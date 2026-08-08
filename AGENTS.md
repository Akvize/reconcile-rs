# AGENTS.md

Source of truth for any human or AI agent working here, across tools (Claude Code, Codex, Cursor,
...). Tool-specific files (`CLAUDE.md`) import this file and add nothing that contradicts it.

## 1. Project overview

`reconcile-rs` is a five-crate Cargo workspace. `rsos` (`FingerprintTreeMap` + range fingerprint) is
a standalone leaf; `rbsr` holds the Range-Based Set Reconciliation algorithm generic over any `rsos`
backend; `lww-register` is the infrastructure-free LWW-Register CRDT domain; `gossip` is the network
adapter layer; and the published `reconcile`
package is the facade over all four — an embedded, in-memory, eventually-consistent key-value store
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
cargo doc --workspace                                       # docs must build warning-free…
cargo doc --workspace --all-features                        # …in both feature sets
```

Both `cargo doc` lines matter, and the bare one is easy to forget: an intra-doc link to a
feature-gated item resolves under `--all-features` and dangles without it, so only the first
invocation catches it — as `-Dwarnings` promotes a dangling link to an error.

`--workspace` throughout (not the deprecated `--all` alias): with five members, the distinction is
worth being unambiguous about. `.github/workflows/main.yml`, `./pre-commit` and this listing are
kept in sync — change one, change all three.

CI sets `RUSTFLAGS=-Dwarnings`/`RUSTDOCFLAGS=-Dwarnings`: warnings fail the build. `./pre-commit`
runs `cargo fmt --check`, `cargo clippy --workspace -- --deny warnings`, and
`./scripts/check-domain-purity.sh` on the staged tree before every commit (§10).

## 4. Type safety and domain modeling

- **Strong-type every wire/domain entity.** A value meaningful to the protocol or domain (sequence
  number, timestamp, key, MAC tag, cluster secret) is its own newtype, never a bare
  `u64`/`[u8; N]`/`String` — two same-typed primitives let a caller swap them silently; two newtypes
  make that a compile error. Precedent:
  [`PhysicalTime`]/[`LogicalCounter`]/[`NodeId`]/[`ClockDrift`](./lww-register/src/clock.rs) — the
  components of a [`Timestamp`] (named for the HLC paper's own vocabulary), which used to be two
  bare `u64`s and a `u32`, so transposing `physical` and `node_id` compiled and silently changed
  LWW ordering — plus [`ClusterKey`]/[`Tag`] (`gossip/src/auth.rs`) and [`Seq`]/[`Stamp`]
  (`gossip/src/replay.rs`). Note `ClockDrift`: a
  *duration* is not the same type as the *instant* it is added to, even when both are "a number of
  milliseconds".
- **Group the components the way the domain groups them, and let the grouping carry the
  arithmetic.** Worked example: `Hlc`/`Timestamp`. A Hybrid Logical Clock (Kulkarni et al.) *is* the
  pair `(physical, logical)`; the `node_id` is not a clock component but the tie-break that makes
  LWW a total order. While `Timestamp` was one flat triple, both arithmetic entry points took a
  `NodeId` they never read — a pure passenger, present only in the value they constructed. Splitting
  out [`Hlc`](./lww-register/src/clock.rs) let the arithmetic (`Hlc::next_tick`,
  `Hlc::advance_past_remote`) drop the parameter entirely, and let the `Clock` adapter store the
  node identity exactly once instead of both as a field and inside its clock state. The tell to
  watch for: **a parameter that appears only in the constructed result, never in a comparison or a
  computation, belongs to the caller's step, not this one.** Nesting is free on the wire — bincode
  and `rsos`'s canonical encoding both write a struct as its fields in declaration order with no
  framing — but prove it with a golden vector (`tests/timestamp_wire_format.rs`) rather than
  asserting it.
- **Every entity owns its own validation** — never the caller. Parsing/encoding lives on the type
  (`Seq::from_le_bytes`, not a free function); an "is this acceptable" check is a method on the type
  (e.g. [`Stamp::is_fresh`](./gossip/src/replay.rs)) — if the same validation arithmetic shows up at two
  call sites, it belongs on the type instead. Construction of an invalid instance should be
  structurally impossible or funneled through one fallible constructor. Same "parse, don't validate"
  principle as [`Payload`](./gossip/src/auth.rs) (only obtainable via `Authenticator::open`),
  [`AdmittedTime`](./lww-register/src/clock.rs) and
  [`Entry`](./ARCHITECTURE.md#36-domain-types-and-conflict-policy) — apply it to every entity, not
  just the security-critical ones.
- **A safety property carried by a parameter name is not carried at all** — give it a type. Worked
  example: `AdmittedTime`. The HLC's far-future clamp is what stops a forged remote stamp from
  pinning every node's clock into the future; it used to be an `effective_remote_wall: u64`
  parameter documented as "the clamped value on the adapter path, the raw value on the trusted
  path", which no compiler checks. Note the past participle: `AdmittedTime` says the check *has
  run*, which is the claim the type actually carries. `Hlc::advance_past_remote` now takes an
  `AdmittedTime`, obtainable only through `AdmittedTime::clamped_to_drift` (which performs the
  clamp) or the explicitly-named `AdmittedTime::trusted` escape hatch — so the trusted path has to *say* it is
  trusting the value, and a raw datagram reading has no route in. Same shape as `Payload`: the type
  exists precisely to be un-forgeable evidence that a check ran.
- **New wire field or protocol value:** newtype it in the module that owns its semantics; put
  encode/decode and validation on that type; only touch a bare primitive at the actual wire boundary.

## 5. Code style guidelines

- `#![forbid(unsafe_code)]` on **every** crate root (`src/lib.rs` and all four sibling crates) —
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
are opt-in. `internal-testing` exposes test-only seams (`reconcile::testing`, and `rbsr`'s
`RangeAggregate::for_testing`) for external test oracles/benches only — not public API.

Since the workspace split, the flags live where their code lives: `mac-blake3`, `mac-hmac`,
`zeroize`, `encryption` and `dns-hickory` are **declared on `gossip`** (which owns `auth.rs` and
`discovery.rs`) and re-exposed from `reconcile` as unification entries
(`mac-blake3 = ["gossip/mac-blake3"]`). `metrics`/`metrics-prometheus` (for `observability.rs` /
`prometheus.rs`) stay on `reconcile`. `internal-testing` is declared on **both** `reconcile` and
`rbsr`, the latter forwarded as another unification entry
(`internal-testing = ["rbsr/internal-testing"]`): on `reconcile` it opens `crate::testing`, on
`rbsr` it opens `RangeAggregate::for_testing`, the seam `tests/wire_format.rs`'s golden vector is
built through. That split is deliberate — it is what lets the byte-level test sit where the codec
is chosen, so `rbsr` needs no codec dependency of its own, not even a dev one.

`reconcile` depends on `gossip` with `default-features = false`, so its own
`default = ["mac-blake3"]` is the single place the MAC backend gets chosen. `encryption` is both
forwarded *and* read locally — `replica.rs`/`replicated_map.rs` carry their own
`#[cfg(feature = "encryption")]` arms.

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

**9.1 Crates and modules** (full table: `ARCHITECTURE.md` §2.1). Five workspace members, in
dependency order:

| Crate | Holds | Kind |
|---|---|---|
| `rsos` | `FingerprintTreeMap`, `Fingerprint`, `Aggregate`, the `Rsos<K>` trait (`fingerprint_tree_map.rs`, `fingerprint_tree_map_iter.rs`, `fingerprint.rs`, `encoding.rs`, `aggregate.rs`) | leaf, zero workspace deps |
| `rbsr` | the RBSR protocol driver + `RsosView<K>` (`protocol.rs`, `rsos_view.rs`) | depends on `rsos` only |
| `lww-register` | `entry.rs`, `bounds.rs`, `clock.rs` (`Hlc`/`Timestamp`/`Clock`/HLC ordering), `persistence.rs` (`Persistence`/`PersistedState`/`InMemoryPersistence`) | **domain**, infrastructure-free |
| `gossip` | `transport.rs`, `bincode.rs`, `auth.rs`, `replay.rs`, `discovery.rs`, `gen_ip.rs` | infrastructure; **no `lww-register` dep** — nothing there knows what an `Entry` is |
| `reconcile` | `replica.rs`, `replicated_map.rs`, `read_replica_map.rs`, `clock.rs` (the chrono-reading `HlcClock` adapter), `snapshot.rs` (`FileSnapshot` + the versioned on-disk header), `observability.rs`, `prometheus.rs`, `timeout_wheel.rs` | the facade; depends on all four and re-exports their public types |

The `reconcile` package keeps re-export shims (`src/persistence.rs`, `src/clock.rs`, `pub use` in
`src/lib.rs`) so `reconcile::entry::Entry`, `reconcile::transport::UdpTransport`,
`reconcile::FileSnapshot` and friends resolve exactly as before the split.

`snapshot` was a sixth member for one step of the split and was folded back in on purpose
(`src/snapshot.rs`): one type, no reuse away from this workspace, no published identity to earn —
ARCHITECTURE.md §3.9 and migration step 9. The persistence *architecture* is unchanged: the
`Persistence` port and `PersistedState` stay in `lww-register`, so the domain still touches no
filesystem. Don't re-split it without a reason the other four boundaries can point to.

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
   has to be caught. `gossip` and the root `reconcile` package (which holds the file-persistence
   adapter, `src/snapshot.rs`) are deliberately out of scope; they are adapters and carry
   infrastructure deps by design.
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

**Policy (#204, settled).** All five members publish. `rsos` and `rbsr` are the genuinely reusable
pieces — a range-summarizable ordered store and the RBSR algorithm over any such store, both useful
away from this project. `lww-register` and `reconcile-gossip` publish only because **cargo has no
vendoring**: `reconcile` cannot be published unless every crate it depends on is on crates.io. They
are **implementation detail with no stability guarantee**, and both say so in their `description`,
their `README.md` and their crate-root docs — the `serde_derive` / `pin-project-internal` /
`tracing-attributes` pattern. Nobody should depend on them directly; keep that warning prominent if
you touch those files.

**`gossip` publishes as `reconcile-gossip`** — the plain name is taken on crates.io. Every dependent
renames it straight back:

```toml
gossip = { package = "reconcile-gossip", version = "0.1.0", path = "gossip" }
```

so **no Rust source anywhere says anything but `use gossip::…`**, and the directory stays `gossip/`.
The published name is a registry detail, not an API one. Don't "fix" the mismatch by renaming the
directory or the imports.

**Every intra-workspace dependency carries a `version` alongside its `path`** (`0.1.0` for the four
siblings). Cargo strips `path` at publish time and resolves the requirement from crates.io, so
without it packaging is refused outright. Add both whenever you add such an edge.

**Publish order is a hard constraint**, encoded in `.github/workflows/tags.yml` with a comment
saying so: `rsos` → (`rbsr`, `lww-register`) → `reconcile-gossip` → `reconcile`. A crate cannot be
published before everything it depends on is on crates.io *and indexed*, so the workflow polls the
sparse index after each upload before moving on. `--no-verify` is not the answer to that lag and
must not be used — it would skip compiling the packaged crate, the one breakage worth catching at
release time. The workflow is idempotent: a version already on crates.io is skipped, which is the
normal path for the four siblings, whose versions change far less often than `reconcile`'s.
Publishing happens **only** from that workflow, on a `v*` tag; never hand-run `cargo publish`.

**`reconcile`'s version is not this repo's to bump casually.** It is 0.2.1 on crates.io; a release
picks the next number, and the renames in the recent stack (`Mirror` → `ReadReplicaMap`, the crate
reshuffle) are breaking, so it is likely 0.3.0. That is a maintainer release decision, made when the
tag is cut — not something a feature PR decides. The tag and the manifest version must match;
`tags.yml` checks that first and fails the release if they don't.

**CI packaging check.** `main.yml` runs `cargo package --workspace --allow-dirty` on every PR,
covering all five members (see the root `Cargo.toml`'s `exclude` list — repo-only docs and
`examples/k8s/` stay out of the published package). `--workspace` is load-bearing: a *per-crate*
`cargo package -p rbsr` still fails with "no matching package named `rsos`" until `rsos` is really
on crates.io, so one-at-a-time only ever covers `rsos`, `lww-register` and `reconcile-gossip`.
Packaged together, cargo resolves the `path` + `version` pairs against a local temporary registry
and `rbsr` and `reconcile` are covered too. Keep it as one invocation.

**Local gotcha with that check.** `cargo package --workspace` verifies each member by resolving its
siblings out of a temporary registry, and cargo caches an *extracted* dependency source under
`~/.cargo/registry/src/<hash>/<name>-<version>/`, keyed by name and version alone. The four siblings
sit at `0.1.0` while their contents change every commit, so a second local run can verify a
**stale** extraction and fail with a nonsensical error — typically `unresolved import` for an item
you just added. CI never sees this (fresh `CARGO_HOME`). If it happens, delete the offending
`~/.cargo/registry/src/<hash>/` directory, or re-run with a throwaway `CARGO_HOME=$(mktemp -d)` to
confirm the tree is actually fine before chasing a phantom bug.

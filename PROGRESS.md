# Project status — `reconcile-rs`

> **Living document.** This is the real-time view of the project's correctness, security, and
> maturity. It is kept current as work lands. It complements, and does not duplicate:
>
> - [`SOTA.md`](./SOTA.md) — state-of-the-art positioning, competitor audit, glossary and
>   bibliography. Durable reference material; not updated as the code evolves.
> - [`ARCHITECTURE.md`](./ARCHITECTURE.md) — the target architecture (hexagonal ports & adapters)
>   and the migration plan.
> - **Issue [#138](https://github.com/Akvize/reconcile-rs/issues/138)** — execution tracking of the
>   architecture migration (one sub-issue per phase).

- **Last updated:** 2026-08-08
- **Baseline:** `claude/workspace-split-step-c` + Step D + the `ReadReplicaMap` `Transport` pass and
  the `Mirror`→`ReadReplicaMap` rename (migration steps 6 A–D, 7 and 8 landed; pending merge to
  main). Correctness baseline unchanged since the
  2026-06 sprint (`claude/determined-franklin-s3tvt1` @ `f1423ce`) — the split is
  behaviour-preserving.
- **Manifest:** `reconcile` `0.2.1` (published on crates.io); the four siblings `0.1.0`, not yet
  published. Publish policy settled under
  [#204](https://github.com/Akvize/reconcile-rs/issues/204) — all five publish, `gossip` as
  `reconcile-gossip`, `lww-register`/`reconcile-gossip` as non-public implementation detail; see
  AGENTS.md §11. The next `reconcile` version is a maintainer release decision (the recent renames
  are breaking, so likely `0.3.0`) and is deliberately **not** bumped in the preparation PR.

---

## 1. Headline

The **algorithmic core** (FingerprintTreeMap + range fingerprint + RBSR diff) is correct and SOTA-aligned. The
**critical engineering and distributed-design defects** found by the original review have since been
fixed: the crate now has a collision-resistant 256-bit fingerprint, HLC-keyed conflict resolution,
causal-stability tombstone GC, malformed-packet hardening, optional per-datagram authentication and
payload encryption, pluggable persistence, runtime observability, and a lightweight dateless
read replica. A 2026-06 adversarial audit filed implementation-level correctness bugs as
[#195](https://github.com/Akvize/reconcile-rs/issues/195)–[#205](https://github.com/Akvize/reconcile-rs/issues/205)
(tracking [#206](https://github.com/Akvize/reconcile-rs/issues/206)); the Phase-0 correctness
findings ([#195](https://github.com/Akvize/reconcile-rs/issues/195),
[#196](https://github.com/Akvize/reconcile-rs/issues/196),
[#197](https://github.com/Akvize/reconcile-rs/issues/197),
[#198](https://github.com/Akvize/reconcile-rs/issues/198)) plus
[#148](https://github.com/Akvize/reconcile-rs/issues/148) are **fixed on this branch**; remaining
audit findings ([#199](https://github.com/Akvize/reconcile-rs/issues/199)–[#205](https://github.com/Akvize/reconcile-rs/issues/205))
are **security/robustness-grade**, scheduled per [#206](https://github.com/Akvize/reconcile-rs/issues/206).
Remaining work is **maturity, scaling, and the confidentiality roadmap**.

---

## 2. Correctness & security findings

Status of every finding (`Fxx`) from the original code audit (commit `64f1ebf`).
✅ resolved · ◐ partial · ◯ open.

| # | Severity | Finding | Status | Resolution / note |
|---|----------|---------|--------|-------------------|
| F1 | Critical | `hash==0` sentinel → silent divergence | ✅ | #106 — emptiness/equality decided on `size`, not `hash` |
| F2 | Critical | panic on malformed UDP → remote DoS | ✅ | #107 — malformed datagrams dropped (`warn!`+`return`) |
| F3 | Critical | unauthenticated + attacker-controlled timestamp | ✅ | #108 — per-datagram keyed MAC, verified before deserialize (opt-in key) |
| F4 | Critical | tombstone resurrection (60 s wall-clock GC) | ✅ | #109 — GC gated on causal stability |
| F5 | High | physical-clock LWW (lossy + non-commutative) | ✅ | #110 — Hybrid Logical Clock + total order |
| F6 | High | 64-bit XOR fingerprint (weak, craftable) | ✅ | #111 — 256-bit additive BLAKE3 |
| F7 | High | crafted `RangeAggregate` → panic/underflow | ✅ | #112 — bound validation + `checked_sub` |
| F8 | High | `DefaultHasher` unstable on the wire | ✅ | #111 — wire fingerprint is BLAKE3; closed the other half by owning the *input* encoding too (`rsos::canonical`), and `version_hash` now derives from it instead of `DefaultHasher` — see §4.x below |
| F9 | High | UDP amplification / reflection | ◐ | mitigated by #108 (auth) + #106; rate-limiting / path validation still open |
| F10 | High | IP-scan discovery, O(N²) membership | ◐ | [#147](https://github.com/Akvize/reconcile-rs/issues/147) — `Discovery` port + `DnsDiscovery` (k8s headless-Service DNS, no IP-scan) lands a cloud-native discovery path; bounded-fan-out membership (SWIM/HyParView) still open |
| F11 | High | no property-testing / fuzzing | ✅ | #113 — `tests/proptest_fingerprint_tree_map.rs`, `tests/fuzz_packets.rs` |
| F12 | Medium | debug `println!` in the hot path | ✅ | #113 — removed |
| F13 | Medium | panic-only API (no `Result`) | ✅ | [#148](https://github.com/Akvize/reconcile-rs/issues/148) — fallible `new` constructors (`io::Result`, `AddrInUse` surfaces); no network send can panic the run loops (`f1423ce`, this branch) |
| F14 | Medium | `pre_insert` hook under the write-lock (net path) | ✅ | [#149](https://github.com/Akvize/reconcile-rs/issues/149) — `pre_insert` runs outside the write lock on both paths, with a regression test (`f4b5028`) |
| F15 | Medium | no persistence | ✅ | #122 — pluggable `Persistence` (`InMemory`, `FileSnapshot`) |
| F16 | Medium | loopback benches + README inconsistency | ◐ | README updated; benches still loopback-only |
| F17 | Medium/Low | maturity signals | ◐ | clippy fixed; see §3 checklist |
| F18 | Medium | resource exhaustion (`peers` map, bincode bomb) | ◐ | bincode decode limit landed ([#151](https://github.com/Akvize/reconcile-rs/issues/151)); [#150](https://github.com/Akvize/reconcile-rs/issues/150) rescoped to the `peers` cap (unauthenticated mode) + per-datagram message/segment caps; related growth vectors in [#200](https://github.com/Akvize/reconcile-rs/issues/200) |
| F19 | Low | dependency hygiene | ◐ | bincode `with_limit` landed ([#151](https://github.com/Akvize/reconcile-rs/issues/151)); `overflow-checks` and `cargo audit`/`cargo deny` in CI still open ([#203](https://github.com/Akvize/reconcile-rs/issues/203), [#205](https://github.com/Akvize/reconcile-rs/issues/205)) |

**Score:** 13 resolved · 6 partial (F9, F10, F16, F17, F18, F19) · 0 open. All Critical resolved;
all but one High resolved or mitigated.

---

## 3. Maturity checklist

- [x] CI green under `-D warnings` (clippy `mismatched_lifetime_syntaxes` fixed)
- [x] Property tests for convergence (`proptest_fingerprint_tree_map.rs`)
- [x] Malformed-packet fuzz harness (`fuzz_packets.rs`)
- [x] Security model documented (README "Security model")
- [x] Pluggable persistence documented (README "Persistence")
- [x] Publish policy settled and the release pipeline prepared — all five crates publish, in
      dependency order, `gossip` as `reconcile-gossip` ([#204](https://github.com/Akvize/reconcile-rs/issues/204), AGENTS.md §11). Semver
      cadence (and the next `reconcile` version) still a per-release maintainer call.
- [ ] `CHANGELOG.md`
- [x] CI code coverage + doc-tests ([#97](https://github.com/Akvize/reconcile-rs/issues/97)) — Codecov (`cargo llvm-cov`) + `cargo test --doc` in CI
- [ ] `cargo audit` / `cargo deny` in CI ([#151](https://github.com/Akvize/reconcile-rs/issues/151))
- [ ] Declare + CI-pin MSRV (`rust-version`) — iterators are safe Rust (crate is `#![forbid(unsafe_code)]`); miri item dropped ([#189](https://github.com/Akvize/reconcile-rs/issues/189))
- [ ] `overflow-checks = true` in the release profile ([#151](https://github.com/Akvize/reconcile-rs/issues/151))
- [ ] bincode decode limit (`with_limit`) against allocation bombs ([#150](https://github.com/Akvize/reconcile-rs/issues/150), [#151](https://github.com/Akvize/reconcile-rs/issues/151))

---

## 4. Open items & roadmap

### Security / confidentiality (umbrella [#96](https://github.com/Akvize/reconcile-rs/issues/96))
- ✅ Confidentiality + integrity — XChaCha20-Poly1305 AEAD on payloads (PR #131).
- ◐ Per-node authentication / anti-MITM — [#136](https://github.com/Akvize/reconcile-rs/issues/136).
- ◯ Forward secrecy (ephemeral-key handshake) — [#135](https://github.com/Akvize/reconcile-rs/issues/135).
- ◯ Key rotation / management — [#137](https://github.com/Akvize/reconcile-rs/issues/137).

### Scaling & robustness
- ✅ Multi-location reconciliation: per-network discovery probes + geography-aware gossip with
  bounded cross-network fan-out (decentralized, no gateway nodes) — [#53](https://github.com/Akvize/reconcile-rs/issues/53).
- ✅ Runtime reconfiguration: live `set_nets`/`add_net`/`remove_net`, `set_remote_interval`/
  `set_remote_fanout`, `set_reconcile_interval`, `set_tombstone_timeout` (auto-derived local net;
  anti-entropy repair decoupled from net membership, so topology changes cannot cause divergence).
- ◐ Membership / discovery: `Discovery` port with a `DnsDiscovery` adapter gives a Kubernetes-native
  path (headless-Service DNS + grace-period decommission of vanished pods, with a wall-time floor
  that holds decommissioning of a member with pending unacknowledged tombstones — closing the
  DNS-driven resurrection hazard, [#201](https://github.com/Akvize/reconcile-rs/issues/201)) that
  sidesteps the random IP-scan; bounded-fan-out membership (SWIM/HyParView) still open
  (F10 — [#147](https://github.com/Akvize/reconcile-rs/issues/147)).
- ◯ Bound the `peers` map; cap messages/segments per datagram; bincode limit
  (F18 — [#150](https://github.com/Akvize/reconcile-rs/issues/150), F19 — [#151](https://github.com/Akvize/reconcile-rs/issues/151)).
- ◯ Larger-than-datagram payloads — [#2](https://github.com/Akvize/reconcile-rs/issues/2).
- ✅ Lightweight dateless read replica (`ReadReplicaMap`), #109-safe — PR #133 ([#128](https://github.com/Akvize/reconcile-rs/issues/128)).
- ✅ Observability: `tracing` spans + `metrics` facade + optional Prometheus endpoint — PR #130 ([#94](https://github.com/Akvize/reconcile-rs/issues/94)).

### Remaining gaps to SOTA (see [`SOTA.md`](./SOTA.md) §2.4)
- Reconciliation latency: RBSR uses O(log n) sequential RTTs; a Rateless-IBLT pass to drain
  divergent leaves in one shot would cut WAN latency. Design choice, not a defect.
- API ergonomics: `Result`-returning constructors ✅ (F13 — [#148](https://github.com/Akvize/reconcile-rs/issues/148), `f1423ce`);
  `pre_insert` outside the write-lock ✅ (F14 — [#149](https://github.com/Akvize/reconcile-rs/issues/149), `f4b5028`);
  post-insert hooks ([#79](https://github.com/Akvize/reconcile-rs/issues/79)).

### 2026-06 adversarial audit

Five independent adversarial audits challenged the codebase and all open issues; findings were
filed as [#195](https://github.com/Akvize/reconcile-rs/issues/195)–[#205](https://github.com/Akvize/reconcile-rs/issues/205)
with roadmap and tracking in
[#206](https://github.com/Akvize/reconcile-rs/issues/206). The Phase-0 correctness items —
HLC restart monotonicity ([#195](https://github.com/Akvize/reconcile-rs/issues/195)), `TimeoutWheel`
same-millisecond expiry collision ([#196](https://github.com/Akvize/reconcile-rs/issues/196)),
fingerprint-desyncing mutable iterators ([#197](https://github.com/Akvize/reconcile-rs/issues/197)),
and HLC far-future stamp / counter wrap ([#198](https://github.com/Akvize/reconcile-rs/issues/198))
— are fixed on this branch; the remainder
([#199](https://github.com/Akvize/reconcile-rs/issues/199) replay/membership poisoning,
[#200](https://github.com/Akvize/reconcile-rs/issues/200) unbounded acks/bulk dumps,
[#201](https://github.com/Akvize/reconcile-rs/issues/201) DNS decommission vs GC gate,
[#202](https://github.com/Akvize/reconcile-rs/issues/202) persistence robustness,
[#203](https://github.com/Akvize/reconcile-rs/issues/203) CI gaps,
[#204](https://github.com/Akvize/reconcile-rs/issues/204) release pipeline/version drift — **now
addressed**: the publish policy is settled and `tags.yml` rewritten to publish all five crates in
dependency order (AGENTS.md §11); cutting the actual release, and choosing `reconcile`'s next
version, remain maintainer decisions —
[#205](https://github.com/Akvize/reconcile-rs/issues/205) hygiene batch) are security/robustness-grade
and scheduled per [#206](https://github.com/Akvize/reconcile-rs/issues/206).

### Architecture refactor
Tracked in [`ARCHITECTURE.md`](./ARCHITECTURE.md) and issue
[#138](https://github.com/Akvize/reconcile-rs/issues/138) (hexagonal ports & adapters). Steps:
bound bundles & encapsulation → dissolve diff traits → `Clock` port → `Entry`/`State` type →
`Transport`/`Codec` ports → workspace split → `ReadReplicaMap` onto the `Transport` port →
`Mirror`→`ReadReplicaMap` rename (all done). None of these change runtime behaviour except the
`Entry`/`State` step (wire/on-disk format), and all preserve the invariants below.

Refinements adopted after a `file:line` review of the sequence (see issue
[#138](https://github.com/Akvize/reconcile-rs/issues/138)): the `Clock` port returns the concrete
`Timestamp` (no generic associated type); the `Entry`/`State` step also dissolves `Projectable`/`ValueOnly`
into `State<V>` and is guarded by invariant 8 below; the `Codec` port carries a decode cap and the
`BincodeCodec` adapter sets `with_limit` (partially closing
[#151](https://github.com/Akvize/reconcile-rs/issues/151)); the `Transport`/`Codec` ports live in
the network crate (the `Clock`/`Persistence` ports in the domain crate) — which is what the split
delivered, as `gossip` and `lww-register` respectively.

Progress:
- ✅ Step 1 — bound bundles & encapsulation ([#140](https://github.com/Akvize/reconcile-rs/issues/140), PR #155).
- ✅ Step 2 — dissolve the diff traits ([#141](https://github.com/Akvize/reconcile-rs/issues/141), PR #156):
  `HashRangeQueryable` / `Diffable` removed; range querying is now inherent on `FingerprintTreeMap`
  (today `aggregate` / `rank` / `select`, all public since the `rsos` extraction; `len` / `is_empty`
  public) and `start_diff` / `diff_round` became free functions over the local set in the
  then-`pub(crate)` `proto` module, with `RangeAggregate` / `DiffRange` no longer on the `reconcile`
  public surface (they now live in the standalone `rbsr` crate, generic over the `RsosView<K>`
  backend trait — migration step 6 Step B). Iso-functional; invariants 3–4
  preserved byte-for-byte.
- ✅ Step 3 — `Clock` port ([#142](https://github.com/Akvize/reconcile-rs/issues/142), PR #158): `pub trait Clock`
  (`now`/`observe`, returning the concrete `Timestamp`) is the domain's time seam; `HlcClock` is the
  default adapter owning the single `chrono` read; the engine holds it as `Arc<dyn Clock>` (object-safe,
  no clock type parameter on the engine/store/`Config`) and mints/observes only through it. A
  deterministic `ManualClock` test adapter makes HLC behaviour reproducible without wall-clock time.
  Iso-functional; invariant 2 (HLC total order) preserved.
- ✅ Naming cleanup alongside the port: type `Hlc`→`Timestamp` (#159) and module `hlc`→`clock` (#163).
- ✅ Step 4 — `Entry`/`State` domain type: the untyped `(Timestamp, Option<V>)` tuple is replaced by
  `Entry<Timestamp, V>` / `State<V>` (`entry.rs`); `MaybeTombstone`, `Reconcilable`, `Timestamped`,
  `Projectable` and `ValueOnly<V>` are dissolved into `Entry`'s inherent methods
  (`is_tombstone`/`value`/`merge`/`project`) and direct `.stamp` field access. `replica.rs`
  and `replicated_map.rs` parameterize the engine over the plain `V` and construct
  `Entry<Timestamp, V>` (map) / `State<V>` (projection) internally; `add_pre_insert`,
  `ReadReplicaMap::add_on_update` and `PersistedState` now carry `Entry`/`State` directly instead of
  the tuple. Invariant 8 (value-only projection summary is timestamp-independent) holds by
  construction — `Entry` has a `stamp` field, `State` has no `Timestamp` field at all, so no
  field-by-field content summary of the projection can include one — and is guarded by tests in
  `entry.rs` and `read_replica_map.rs`. **Changes the wire and on-disk
  formats** (expected; pre-1.0).
- ✅ Step 5 — `Transport` port + `bincode.rs` wire-encoding functions (the `Codec` trait itself was
  dissolved, ARCHITECTURE.md §2.4).
- ✅ Step 6 — workspace split, done in four sub-steps (ARCHITECTURE.md §6):
  **A** extracted the `rsos` crate (`FingerprintTreeMap` — renamed from `HRTree` via
  `FingerprintTree` — `Fingerprint`, the `Rsos` trait, and the Def. 3.5 bundled `Aggregate`
  returned by `aggregate`);
  **B** extracted the `rbsr` crate (`start_diff`/`diff_round`/`RangeAggregate`, generic over
  `rbsr::RsosView<K>`, blanket-implemented for every `rsos::Rsos` implementor, with `Rsos<K, V>`
  becoming `Rsos<K>` + associated `Value`);
  **C** split out `lww-register` (domain), `gossip` (network adapters, deliberately with no
  `lww-register` dependency) and `snapshot` (file persistence adapter — later folded back into
  `reconcile` as `src/snapshot.rs`, Step 9 below), plus the
  `ReconcileEngine`→`Replica`, `ReconcileStore`→`ReplicatedMap`, `ReconcileMirror`→`Mirror` renames;
  **D** reassembled `reconcile` as a thin re-exporting facade and swept CI/scripts/docs — `--all` →
  `--workspace` across `main.yml`/`pre-commit`/`AGENTS.md` §3, and
  `scripts/check-domain-purity.sh` gained a **manifest gate** forbidding
  `tokio`/`bincode`/`chrono`/`ipnet`/`mio`/`reqwest`/`hyper`/`socket2`/`async-trait` in
  `rsos`/`rbsr`/`lww-register`'s `Cargo.toml`s — the level at which that invariant is actually
  breakable, since with the dependency undeclared the import would not compile (ARCHITECTURE.md
  §2.2).

- ✅ Step 7 — the dateless read replica routed onto the `Transport` port, closing the item carried
  over from Step 5. It holds an `Arc<dyn Transport<Addr = SocketAddr>>` instead of an
  `Arc<UdpSocket>`, receives via `Transport::recv_from` and decodes via
  `gossip::bincode::decode_stream` under the engine's `MAX_MESSAGES_PER_DATAGRAM` bound, so
  `src/read_replica_map.rs` imports neither `bincode` nor `tokio::net`. `ReadReplicaMap::new` is
  unchanged for real-socket users (it binds a `UdpTransport` internally) and the new public
  `ReadReplicaMap::new_with_transport` matches `ReplicatedMap::new_with_transport`. Authentication
  still runs on raw datagram bytes ahead of any decode (invariant #5 below), and the read-only /
  non-member semantics are untouched. Evidence:
  `tests/read_replica_map.rs::read_replica_converges_with_dated_store_over_in_memory_transport`
  converges a read replica with a dated `ReplicatedMap` over `InMemoryNetwork` with no sockets bound
  anywhere.

- ✅ Step 8 — `Mirror` renamed to `ReadReplicaMap` (`src/mirror.rs` → `src/read_replica_map.rs`,
  `tests/mirror.rs` → `tests/read_replica_map.rs`), finishing the naming pass begun in Step 6C.
  `Mirror` was the last public type named after a generic metaphor rather than domain or literature
  vocabulary; the type is a passive, dateless, read-only replica of a dated `ReplicatedMap` — a
  *read replica* in the standard database sense — so the name now says so and parallels
  `ReplicatedMap`. No deprecated alias (pre-1.0, as with `ReconcileEngine`→`Replica` and
  `ReconcileStore`→`ReplicatedMap`); pure rename, no behaviour, wire-format or on-disk change.

- ✅ Step 9 — the `snapshot` crate folded back into `reconcile` as `src/snapshot.rs`, taking the
  workspace from six crates to five. A deliberate follow-up decision on Step 6C's granularity, not a
  revert: the domain/adapter *separation* stands (the `Persistence` port and `PersistedState` stay in
  `lww-register`, so nothing there touches a filesystem), but `FileSnapshot` — one type, useful to
  nobody outside this workspace, and with its name already taken on crates.io — did not need a crate
  of its own. Its six unit tests moved with it, including the two needing `bincode`, which was the
  reason they could not live in `lww-register`. `reconcile::FileSnapshot` and
  `reconcile::persistence::FileSnapshot` resolve exactly as before; no public-surface, wire-format or
  on-disk change. See ARCHITECTURE.md §3.9.

- ✅ **Canonical fingerprint encoding** — element fingerprints no longer take their bytes from
  `std::hash::Hash`. `rsos` gained `rsos/src/canonical.rs`: an injective, length-prefixed
  `serde::Serializer` that writes canonical bytes straight into BLAKE3 (fixed-width little-endian
  integers, `usize`/`isize` as 64-bit, `u64` length prefixes on strings/bytes/sequences, `u32`
  variant indices, struct fields in declaration order, **map entries sorted by encoded key**). No new
  dependency — `serde` was already there — and no codec crate, so the crate stays the
  zero-infrastructure leaf `check-domain-purity.sh` gates.

  *Why.* The module claimed a fingerprint was stable "across Rust versions, platforms and
  endianness", but only the *hasher* was pinned. The bytes fed into it came from std's `Hash`
  impls, whose exact sequences Rust explicitly does not stabilize — a future `Hash for str` or
  `Hash for Option<T>` would have moved every fingerprint in every cluster and left a mixed-version
  cluster re-exchanging forever. `Hash` is also unimplemented for `HashMap`/`HashSet`, so those were
  unusable as keys or values. The claim is true only now that `rsos` owns both halves.

  *Fallout.* `lift`'s bound became `Serialize`; the `impl Hasher for Blake3Hasher` is gone;
  `Key`/`Value` (`lww-register/src/bounds.rs`) **dropped `Hash`** — a pure loosening, since both
  already required `Serialize + DeserializeOwned`. The `Hash` bounds that remain are genuine
  `HashMap`-key requirements and are spelled out locally (`TimeoutWheel`, `src/snapshot.rs`,
  `ReplicatedMap`/`Replica`'s peer and tombstone indexes). `version_hash` moved off `DefaultHasher`
  — whose algorithm std does not stabilize either — onto `rsos::digest`, closing the remaining half
  of finding F8. Golden vectors in `rsos/src/fingerprint.rs` were **replaced**, not adjusted; the
  old constants are gone on purpose.

  > **⚠️ Deliberate wire break, pre-release.** Every element fingerprint changes. A node on this
  > code and a node on an earlier build never agree on a range fingerprint and re-exchange
  > indefinitely without converging. **Not a rolling upgrade**: stop the cluster, upgrade every
  > node, restart. This had to land before any release tag, and did.

**#138 is closed.** The structural migration was complete after Step 6 — a multi-crate workspace
(six then, five now: Step 9), ports on the
correct side of each boundary, domain purity enforced by the compiler and by
`check-domain-purity.sh` — and Step 7 removed the last residual infrastructure coupling. A grep of
the `reconcile` crate for `tokio::net`/`UdpSocket`/`bincode` now returns only `#[cfg(test)]` code and
the `UdpTransport` default-adapter constructions. The infrastructure that remains unported by design
(`tokio::time`, `rand::StdRng`, `ipnet`) is not a swappable boundary; see ARCHITECTURE.md §2.3/§6.

---

## 5. Load-bearing invariants

Any change must preserve these (they encode the fixes above):

1. Fingerprint format & arithmetic (`[u64;4]`, per-element BLAKE3 over `rsos::canonical`'s injective
   encoding, add/sub mod 2²⁵⁶) + golden vectors. Both halves are load-bearing: changing the encoding
   is as much a wire break as changing the hash.
2. HLC total order `(wall_ms, counter, node_id)`; merge uses strict `>`.
3. Range emptiness/equality decided on `size`, never on `hash`.
4. `diff_round` validates incoming bounds (`checked_sub`, no `unimplemented!`).
5. Authenticate-before-deserialize (MAC on raw bytes before decoding).
6. Causal-stability tombstone gate before GC.
7. `version_hash` determinism — via `rsos::digest` (the canonical encoding), not `DefaultHasher`.
8. Value-only projection summary is timestamp-less — the dated cell keeps a timestamp-**inclusive**
   encoding (which `version_hash` reads), while its value-only projection (the dateless read-replica
   channel) has no timestamp field at all, so a dated store and a dateless read replica agree on
   per-element fingerprints.

---

## 6. Maintaining this file

Update on any change that moves a finding's status, ticks a maturity box, or closes a roadmap item:
bump **Last updated** and **Baseline**, flip the status cell (and add the PR/issue), and keep the
headline honest. SOTA positioning and rationale stay in [`SOTA.md`](./SOTA.md); target design stays
in [`ARCHITECTURE.md`](./ARCHITECTURE.md).

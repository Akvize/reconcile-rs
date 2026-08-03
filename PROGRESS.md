# Project status — `reconcile-rs`

> **Living document** — the real-time view of correctness, security and maturity, kept current as
> work lands. Durable material lives elsewhere: field positioning in [`SOTA.md`](./SOTA.md), target
> design, invariants and the decision ledger in [`ARCHITECTURE.md`](./ARCHITECTURE.md), vocabulary in
> [`GLOSSARY.md`](./GLOSSARY.md). Migration execution is tracked in
> [#138](https://github.com/Akvize/reconcile-rs/issues/138).

| | |
|---|---|
| Last updated | 2026-08-03 |
| Baseline | `claude/priority-issues-review-2lu5e9`, stacked on the Phase-1 stack (`claude/p1-7-hygiene`, pending merge). Carries the [#216](https://github.com/Akvize/reconcile-rs/issues/216) tombstone-GC convergence fix, [#143](https://github.com/Akvize/reconcile-rs/issues/143) (`Entry`/`State`, wire + on-disk break) and [#144](https://github.com/Akvize/reconcile-rs/issues/144) (`Transport`/`Codec` ports) |
| Manifest | `0.2.1` (unpublished; breaking changes ride 0.3.0 — [D10](./ARCHITECTURE.md#d10--030-is-one-coordinated-break), [#204](https://github.com/Akvize/reconcile-rs/issues/204)) |

---

## 1. Headline

The **algorithmic core** (HRTree + range fingerprint + RBSR diff) is correct and SOTA-aligned, and
every **critical engineering and distributed-design defect** from the original review is fixed:
collision-resistant 256-bit fingerprint, HLC-keyed conflict resolution, causal-stability tombstone
GC, malformed-packet hardening, optional per-datagram authentication and payload encryption,
pluggable persistence, runtime observability, and a dateless read-only mirror.

Remaining work is **maturity, scaling, and the confidentiality roadmap**.

---

## 2. Correctness & security findings

Every finding (`Fxx`) from the original code audit (commit `64f1ebf`).
✅ resolved · ◐ partial · ◯ open.

| # | Severity | Finding | Status | Resolution / note |
|---|----------|---------|--------|-------------------|
| F1 | Critical | `hash==0` sentinel → silent divergence | ✅ | #106 — emptiness/equality decided on `size`, not `hash` |
| F2 | Critical | panic on malformed UDP → remote DoS | ✅ | #107 — malformed datagrams dropped (`warn!`+`return`) |
| F3 | Critical | unauthenticated + attacker-controlled timestamp | ✅ | #108 — per-datagram keyed MAC, verified before deserialize (opt-in key) |
| F4 | Critical | tombstone resurrection (60 s wall-clock GC) | ✅ | #109 — GC gated on causal stability |
| F5 | High | physical-clock LWW (lossy + non-commutative) | ✅ | #110 — Hybrid Logical Clock + total order |
| F6 | High | 64-bit XOR fingerprint (weak, craftable) | ✅ | #111 — 256-bit additive BLAKE3 |
| F7 | High | crafted `HashSegment` → panic/underflow | ✅ | #112 — bound validation + `checked_sub` |
| F8 | High | `DefaultHasher` unstable on the wire | ✅ | #111 — wire fingerprint is BLAKE3 (`version_hash` still fixed-key `DefaultHasher`) |
| F9 | High | UDP amplification / reflection | ◐ | mitigated by #108 (auth) + #106; rate-limiting / path validation still open |
| F10 | High | IP-scan discovery, O(N²) membership | ◐ | [#147](https://github.com/Akvize/reconcile-rs/issues/147) — `Discovery` port + `DnsDiscovery` lands a cloud-native path; bounded-fan-out membership (SWIM/HyParView) still open |
| F11 | High | no property-testing / fuzzing | ✅ | #113 — `tests/proptest_hrtree.rs`, `tests/fuzz_packets.rs` |
| F12 | Medium | debug `println!` in the hot path | ✅ | #113 — removed |
| F13 | Medium | panic-only API (no `Result`) | ✅ | [#148](https://github.com/Akvize/reconcile-rs/issues/148) — fallible `new` constructors; no network send can panic the run loops (`f1423ce`) |
| F14 | Medium | `pre_insert` hook under the write-lock (net path) | ✅ | [#149](https://github.com/Akvize/reconcile-rs/issues/149) — runs outside the write lock on both paths, with a regression test (`f4b5028`) |
| F15 | Medium | no persistence | ✅ | #122 — pluggable `Persistence` (`InMemory`, `FileSnapshot`) |
| F16 | Medium | loopback benches + README inconsistency | ◐ | README updated; benches still loopback-only |
| F17 | Medium/Low | maturity signals | ◐ | clippy fixed; see §3 |
| F18 | Medium | resource exhaustion (`peers` map, bincode bomb) | ◐ | bincode decode limit landed ([#151](https://github.com/Akvize/reconcile-rs/issues/151)); [#150](https://github.com/Akvize/reconcile-rs/issues/150) rescoped to the `peers` cap + per-datagram message/segment caps; related growth vectors in [#200](https://github.com/Akvize/reconcile-rs/issues/200) |
| F19 | Low | dependency hygiene | ◐ | bincode `with_limit` landed; `overflow-checks` and `cargo audit`/`cargo deny` in CI still open ([#203](https://github.com/Akvize/reconcile-rs/issues/203), [#205](https://github.com/Akvize/reconcile-rs/issues/205)) |

**Score:** 13 resolved · 6 partial (F9, F10, F16, F17, F18, F19) · 0 open. All Critical resolved;
all but one High resolved or mitigated.

---

## 3. Maturity checklist

- [x] CI green under `-D warnings`
- [x] Property tests for convergence (`proptest_hrtree.rs`)
- [x] Malformed-packet fuzz harness (`fuzz_packets.rs`)
- [x] Security model documented (README)
- [x] Pluggable persistence documented (README)
- [x] Semver and publish policy — tag-version verification restored; 0.3.0 break policy in `CHANGELOG.md` ([#204](https://github.com/Akvize/reconcile-rs/issues/204))
- [x] `CHANGELOG.md` seeded with the 0.2.1 baseline and an Unreleased (0.3.0) section
- [x] CI code coverage + doc-tests — Codecov (`cargo llvm-cov`) + `cargo test --doc` ([#97](https://github.com/Akvize/reconcile-rs/issues/97))
- [x] Feature-matrix CI — `mac-hmac`, `encryption` and `macos` jobs; scalable poll budget via `RECONCILE_TEST_TIME_MULTIPLIER` ([#203](https://github.com/Akvize/reconcile-rs/issues/203))
- [x] MSRV declared and CI-pinned (`rust-version = "1.85"`); crate is `#![forbid(unsafe_code)]`, so the miri item was dropped ([#189](https://github.com/Akvize/reconcile-rs/issues/189))
- [ ] `cargo audit` / `cargo deny` in CI ([#151](https://github.com/Akvize/reconcile-rs/issues/151))
- [ ] `overflow-checks = true` in the release profile ([#151](https://github.com/Akvize/reconcile-rs/issues/151))
- [ ] `peers` map cap and per-datagram message/segment caps ([#150](https://github.com/Akvize/reconcile-rs/issues/150))

---

## 4. Open items & roadmap

### Security / confidentiality — umbrella [#96](https://github.com/Akvize/reconcile-rs/issues/96)

| | Item | Issue |
|---|---|---|
| ✅ | Confidentiality + integrity — XChaCha20-Poly1305 AEAD on payloads | PR #131 |
| ◐ | Per-node authentication / anti-MITM | [#136](https://github.com/Akvize/reconcile-rs/issues/136) |
| ◯ | Forward secrecy (ephemeral-key handshake) | [#135](https://github.com/Akvize/reconcile-rs/issues/135) |
| ◯ | Key rotation / management | [#137](https://github.com/Akvize/reconcile-rs/issues/137) |

### Scaling & robustness

| | Item | Issue |
|---|---|---|
| ✅ | Multi-location reconciliation: per-network probes + geography-aware gossip with bounded cross-network fan-out, no gateway nodes | [#53](https://github.com/Akvize/reconcile-rs/issues/53) |
| ✅ | Runtime reconfiguration: live `set_nets`/`add_net`/`remove_net`, cadence and fan-out knobs; repair decoupled from net membership, so topology changes cannot cause divergence | — |
| ✅ | Dateless read-only mirror (`ReconcileMirror`), #109-safe | PR #133, [#128](https://github.com/Akvize/reconcile-rs/issues/128) |
| ✅ | Observability: `tracing` spans + `metrics` facade + optional Prometheus endpoint | PR #130, [#94](https://github.com/Akvize/reconcile-rs/issues/94) |
| ◐ | Membership / discovery: `DnsDiscovery` gives a Kubernetes-native path; bounded-fan-out membership (SWIM/HyParView) still open | F10, [#147](https://github.com/Akvize/reconcile-rs/issues/147) |
| ◯ | Bound the `peers` map; cap messages/segments per datagram | F18/F19, [#150](https://github.com/Akvize/reconcile-rs/issues/150), [#151](https://github.com/Akvize/reconcile-rs/issues/151) |
| ◯ | Larger-than-datagram payloads — a single oversize `Update` never converges (invariant 9) | [#2](https://github.com/Akvize/reconcile-rs/issues/2), [#230](https://github.com/Akvize/reconcile-rs/issues/230) |

### 2026-06 adversarial audit

Five independent audits challenged the codebase and all open issues; findings filed as
[#195](https://github.com/Akvize/reconcile-rs/issues/195)–[#205](https://github.com/Akvize/reconcile-rs/issues/205),
tracked in [#206](https://github.com/Akvize/reconcile-rs/issues/206).

| | Finding | Issue |
|---|---|---|
| ✅ | HLC restart monotonicity | [#195](https://github.com/Akvize/reconcile-rs/issues/195) |
| ✅ | `TimeoutWheel` same-millisecond expiry collision | [#196](https://github.com/Akvize/reconcile-rs/issues/196) |
| ✅ | Fingerprint-desyncing mutable iterators | [#197](https://github.com/Akvize/reconcile-rs/issues/197) |
| ✅ | HLC far-future stamp / counter wrap | [#198](https://github.com/Akvize/reconcile-rs/issues/198) |
| ✅ | Tombstone-GC convergence: acks were pairwise and non-transitive, so causal stability was unreachable at n ≥ 3. Every node now re-acknowledges the tombstones it holds each round | [#216](https://github.com/Akvize/reconcile-rs/issues/216) |
| ◯ | Replay / membership poisoning | [#199](https://github.com/Akvize/reconcile-rs/issues/199) |
| ◯ | Unbounded acks / bulk dumps | [#200](https://github.com/Akvize/reconcile-rs/issues/200) |
| ◯ | DNS decommission vs GC gate | [#201](https://github.com/Akvize/reconcile-rs/issues/201) |
| ◯ | Persistence robustness | [#202](https://github.com/Akvize/reconcile-rs/issues/202) |
| ◯ | CI gaps | [#203](https://github.com/Akvize/reconcile-rs/issues/203) |
| ◯ | Release pipeline / version drift | [#204](https://github.com/Akvize/reconcile-rs/issues/204) |
| ◯ | Hygiene batch | [#205](https://github.com/Akvize/reconcile-rs/issues/205) |

### Architecture refactor — [#138](https://github.com/Akvize/reconcile-rs/issues/138)

Sequence and rationale in [`ARCHITECTURE.md` §6](./ARCHITECTURE.md#6-migration-sequence). No step
changes runtime behaviour except step 4 (wire/on-disk format), and all preserve the invariants.

| Step | Status | Notes |
|---|---|---|
| 1 — bound bundles & encapsulation | ✅ | [#140](https://github.com/Akvize/reconcile-rs/issues/140), PR #155 |
| 2 — dissolve the diff traits | ✅ | [#141](https://github.com/Akvize/reconcile-rs/issues/141), PR #156. `HashRangeQueryable` / `Diffable` removed; range-hash querying inherent on `HRTree`; `start_diff` / `diff_round` free functions in `pub(crate) proto`. Iso-functional; invariants 3–4 byte-for-byte |
| 3 — `Clock` port | ✅ | [#142](https://github.com/Akvize/reconcile-rs/issues/142), PR #158. `Arc<dyn Clock>` — object-safe, no clock type parameter on the engine/store/`Config`. `ManualClock` makes HLC behaviour reproducible without wall-clock time. Invariant 2 preserved. Naming cleanup alongside: `Hlc`→`Timestamp` (#159), `hlc`→`clock` (#163) |
| 4 — `Entry` / `State` | ◐ | [#143](https://github.com/Akvize/reconcile-rs/issues/143), PR #229. Also dissolves `Projectable`/`ValueOnly` into `State<V>`; guarded by invariant 8. **Breaks the wire and on-disk formats** |
| 5 — `Transport` / `Codec` ports | ◐ | [#144](https://github.com/Akvize/reconcile-rs/issues/144), PR #229. Ports live in `reconcile-net`; the `Codec` port carries a decode cap and `BincodeCodec` sets `with_limit`, partly closing [#151](https://github.com/Akvize/reconcile-rs/issues/151) |
| 6 — workspace split | ◯ | Also lands the tree as a peer crate ([D1](./ARCHITECTURE.md#d1--hrtree-becomes-its-own-product-correctly-named)) |

### Remaining gaps to SOTA — [`SOTA.md` §2.4](./SOTA.md)

- **Reconciliation latency.** RBSR costs ⌈log₁₆ n⌉ sequential RTTs; a Rateless-IBLT pass to drain
  divergent leaves in one shot would cut WAN latency. A design choice, not a defect — and if built,
  it self-selects rather than becoming a knob ([D8](./ARCHITECTURE.md#d8--reconciliation-strategy-is-automatic-never-a-user-facing-choice)).
- **API ergonomics.** `Result`-returning constructors ✅ (F13), `pre_insert` outside the write lock
  ✅ (F14); post-insert hooks still open ([#79](https://github.com/Akvize/reconcile-rs/issues/79)).
- **Benchmarking.** Cold-sync throughput and loss recovery (#168, #169), per-entry memory (#170),
  point-read indexing (#171), snapshot cadence (#172), bulk-build throughput (#173), comparative
  suite (#174). Closing these is what moves the crate from a narrow niche to a credible Rust IMDG.

---

## 5. Maintaining this file

Update on any change that moves a finding's status, ticks a maturity box, or closes a roadmap item:
bump **Last updated** and **Baseline**, flip the status cell, add the PR/issue, and keep §1 honest.
Positioning stays in [`SOTA.md`](./SOTA.md); design, invariants and decisions stay in
[`ARCHITECTURE.md`](./ARCHITECTURE.md) — do not restate them here, or the two will drift.

# Project status — `reconcile-rs`

> **Living document.** Real-time view of correctness, security, and maturity. Complements, does not
> duplicate: [`SOTA.md`](./SOTA.md) (durable positioning/glossary/bibliography) and
> [`ARCHITECTURE.md`](./ARCHITECTURE.md) (the architecture and its invariants, §5 there).
> **[Issue #138](https://github.com/Akvize/reconcile-rs/issues/138)** tracks the architecture
> migration to closure.

- **Last updated:** 2026-08-09.
- **Structural migration:** complete — five-crate workspace, ports on the correct side of each
  boundary, domain purity compiler-enforced (`ARCHITECTURE.md`). Tracking issues
  [#138](https://github.com/Akvize/reconcile-rs/issues/138) and
  [#145](https://github.com/Akvize/reconcile-rs/issues/145) stay open pending the first
  split-aware release (below), not because code work remains.
- **Publish status:** only `reconcile` `0.2.1` is on crates.io, and it **predates this workspace
  split** — it vendors what are now `rsos`/`rbsr`/`lww-register`/`gossip` directly. The four sibling
  crates have never been published; the release pipeline to publish all five
  (`.github/workflows/tags.yml`, AGENTS.md §11) is implemented but has not been run. Tracked in
  [#204](https://github.com/Akvize/reconcile-rs/issues/204) (open): the next tag is a breaking
  release (recent renames, the wire-format changes below), so it is deliberately a maintainer call,
  not bumped by a feature PR.

---

## 1. Headline

The **algorithmic core** (FingerprintTreeMap + range fingerprint + RBSR diff) is correct and
SOTA-aligned (`SOTA.md`). The critical engineering and distributed-design defects the original
review found are fixed: collision-resistant 256-bit fingerprint, HLC-keyed conflict resolution,
causal-stability tombstone GC, malformed-packet hardening, optional per-datagram authentication and
payload encryption, pluggable persistence, runtime observability, a lightweight dateless read
replica. A 2026-06 adversarial audit filed further findings as
[#195](https://github.com/Akvize/reconcile-rs/issues/195)–[#205](https://github.com/Akvize/reconcile-rs/issues/205)
(tracking [#206](https://github.com/Akvize/reconcile-rs/issues/206)); all nine correctness/security
findings (#195–#201) are **fixed**. Remaining open items
([#202](https://github.com/Akvize/reconcile-rs/issues/202) persistence robustness,
[#203](https://github.com/Akvize/reconcile-rs/issues/203) CI gaps,
[#204](https://github.com/Akvize/reconcile-rs/issues/204) release pipeline,
[#205](https://github.com/Akvize/reconcile-rs/issues/205) hygiene batch) are maturity-grade, not
correctness-grade. Remaining work beyond those is scaling and the confidentiality roadmap.

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
| F6 | High | 64-bit XOR fingerprint (weak, craftable) | ✅ | #111 — 256-bit additive BLAKE3 (`rsos/src/fingerprint.rs`, add/sub mod 2²⁵⁶) |
| F7 | High | crafted `RangeAggregate` → panic/underflow | ✅ | #112 — bound validation + `checked_sub` |
| F8 | High | `DefaultHasher` unstable on the wire | ✅ | #111 + `rsos::encoding` (ARCHITECTURE.md §6) — wire fingerprint is BLAKE3 over an owned canonical byte encoding; `version_hash` derives from it, not `DefaultHasher` |
| F9 | High | UDP amplification / reflection | ◐ | mitigated by #108 (auth) + #106; rate-limiting / path validation still open |
| F10 | High | IP-scan discovery, O(N²) membership | ◐ | `Discovery` port + `DnsDiscovery` (k8s headless-Service DNS, no IP-scan) lands a cloud-native path; bounded-fan-out membership (SWIM/HyParView) still open — [#147](https://github.com/Akvize/reconcile-rs/issues/147)/[#190](https://github.com/Akvize/reconcile-rs/issues/190) |
| F11 | High | no property-testing / fuzzing | ✅ | #113 — `tests/proptest_fingerprint_tree_map.rs`, `tests/fuzz_packets.rs` |
| F12 | Medium | debug `println!` in the hot path | ✅ | #113 — removed |
| F13 | Medium | panic-only API (no `Result`) | ✅ | #148 — fallible `new` constructors; no network send can panic the run loops |
| F14 | Medium | `pre_insert` hook under the write-lock (net path) | ✅ | #149 — hook runs outside the write lock on both paths, regression-tested |
| F15 | Medium | no persistence | ✅ | #122 — pluggable `Persistence` (`InMemory`, `FileSnapshot`) |
| F16 | Medium | loopback benches + README inconsistency | ◐ | README updated; benches still loopback-only |
| F17 | Medium/Low | maturity signals | ◐ | clippy clean; MSRV still undeclared — [#189](https://github.com/Akvize/reconcile-rs/issues/189) |
| F18 | Medium | resource exhaustion (`peers` map, bincode bomb) | ◐ | per-datagram message/segment caps landed (#151); unbounded `peers` map (unauthenticated mode) still open — [#150](https://github.com/Akvize/reconcile-rs/issues/150) |
| F19 | Low | dependency hygiene | ◐ | bincode `with_limit` landed (#151); `overflow-checks` and `cargo audit`/`cargo deny` in CI still absent — [#203](https://github.com/Akvize/reconcile-rs/issues/203)/[#205](https://github.com/Akvize/reconcile-rs/issues/205) |

**Score:** 13 resolved · 6 partial (F9, F10, F16, F17, F18, F19) · 0 open. All Critical resolved; all
but one High resolved or mitigated.

---

## 3. Maturity checklist

- [x] CI green under `-D warnings`
- [x] Property tests for convergence (`proptest_fingerprint_tree_map.rs`)
- [x] Malformed-packet fuzz harness (`fuzz_packets.rs`)
- [x] Security model documented (README "Security model")
- [x] Pluggable persistence documented (README "Persistence")
- [x] Release pipeline implemented (`tags.yml`, all five crates, dependency order) — not yet **run**,
      see the publish status above; semver cadence is a per-release maintainer call
- [ ] `CHANGELOG.md`
- [x] CI code coverage + doc-tests (#97) — Codecov (`cargo llvm-cov`, informational) + `cargo test --doc`
- [ ] `cargo audit` / `cargo deny` in CI — [#203](https://github.com/Akvize/reconcile-rs/issues/203)/[#205](https://github.com/Akvize/reconcile-rs/issues/205)
- [ ] Declare + CI-pin MSRV (`rust-version`) — [#189](https://github.com/Akvize/reconcile-rs/issues/189)
- [ ] `overflow-checks = true` in the release profile — [#205](https://github.com/Akvize/reconcile-rs/issues/205)
- [ ] Bound the `peers` map in unauthenticated mode — [#150](https://github.com/Akvize/reconcile-rs/issues/150)

---

## 4. Open items & roadmap

### Security / confidentiality (umbrella [#96](https://github.com/Akvize/reconcile-rs/issues/96))
- ✅ Confidentiality + integrity — XChaCha20-Poly1305 AEAD on payloads.
- ◯ Per-node authentication / anti-MITM — [#136](https://github.com/Akvize/reconcile-rs/issues/136).
- ◯ Forward secrecy (ephemeral-key handshake) — [#135](https://github.com/Akvize/reconcile-rs/issues/135).
- ◯ Key rotation / management — [#137](https://github.com/Akvize/reconcile-rs/issues/137).

### Scaling & robustness
- ✅ Multi-location reconciliation, geography-aware gossip — [#53](https://github.com/Akvize/reconcile-rs/issues/53) (closed).
- ✅ Runtime reconfiguration: live topology/gossip-cadence changes (README "Multiple geographical locations").
- ✅ DNS-driven decommission hardened against resurrection — [#201](https://github.com/Akvize/reconcile-rs/issues/201) (closed).
- ◯ Bounded-fan-out membership (SWIM/HyParView) instead of random probing — [#147](https://github.com/Akvize/reconcile-rs/issues/147)/[#190](https://github.com/Akvize/reconcile-rs/issues/190).
- ◯ Persistence robustness (snapshot clone stalls writes, missing fsync, crash-loop on transient load
  failure) — [#202](https://github.com/Akvize/reconcile-rs/issues/202).
- ◯ Larger-than-datagram payloads — [#230](https://github.com/Akvize/reconcile-rs/issues/230) (documented gap, README "Value-size ceiling").
- ✅ Lightweight dateless read replica (`ReadReplicaMap`) — [#128](https://github.com/Akvize/reconcile-rs/issues/128) (closed).
- ✅ Observability: `tracing` spans + `metrics` facade + optional Prometheus endpoint — [#94](https://github.com/Akvize/reconcile-rs/issues/94) (closed).
- ✅ Collection-shaped read API on both `ReplicatedMap` and `ReadReplicaMap` — `len`/`is_empty`/
  `contains_key`/`for_each`/`for_each_in_range`/`to_vec`/`range_to_vec`/`keys`/`values`, tombstones
  excluded consistently across both types — [#179](https://github.com/Akvize/reconcile-rs/issues/179)
  (closed).

### API and performance
- ✅ Round out the write API: atomic `update`/`upsert`/`get_or_insert_with`, `clear`/`retain`/
  `delete_range`, and a `load_bulk` no-broadcast seed path (`just_*` demoted off the published
  surface) — [#180](https://github.com/Akvize/reconcile-rs/issues/180) (closed).

### Tracked, not yet started
- Bulk-build throughput, point-read indexing, per-entry memory overhead, configurable snapshot
  cadence — [#170](https://github.com/Akvize/reconcile-rs/issues/170)–[#173](https://github.com/Akvize/reconcile-rs/issues/173).
- Comparative benchmark suite — [#174](https://github.com/Akvize/reconcile-rs/issues/174).
- Cut sync latency below O(log n) sequential RTTs (hybrid RBSR + Rateless IBLT, generic monoid
  summary) — [#185](https://github.com/Akvize/reconcile-rs/issues/185); see also
  [#257](https://github.com/Akvize/reconcile-rs/issues/257) (the current split fan-out is
  unbenchmarked and hard-coded).
- Pluggable per-value conflict resolution beyond LWW-Register —
  [#184](https://github.com/Akvize/reconcile-rs/issues/184).

Full SOTA gap analysis: [`SOTA.md`](./SOTA.md) §2.4.

---

## 5. Invariants

The load-bearing correctness/security properties are documented once, in
[`ARCHITECTURE.md`](./ARCHITECTURE.md) §5 — not duplicated here.

---

## 6. Maintaining this file

Update on any change that moves a finding's status, ticks a maturity box, or closes a roadmap item:
bump **Last updated**, flip the status cell (add the PR/issue), keep the headline honest. SOTA
positioning stays in [`SOTA.md`](./SOTA.md); architecture and invariants stay in
[`ARCHITECTURE.md`](./ARCHITECTURE.md).

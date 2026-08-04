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

- **Last updated:** 2026-06-12
- **Baseline:** `claude/determined-franklin-s3tvt1` @ `f1423ce` (2026-06 correctness sprint; pending
  merge to main)
- **Manifest:** `0.2.1` (unpublished; semver and publish policy tracked in
  [#204](https://github.com/Akvize/reconcile-rs/issues/204))

---

## 1. Headline

The **algorithmic core** (HRTree + range fingerprint + RBSR diff) is correct and SOTA-aligned. The
**critical engineering and distributed-design defects** found by the original review have since been
fixed: the crate now has a collision-resistant 256-bit fingerprint, HLC-keyed conflict resolution,
causal-stability tombstone GC, malformed-packet hardening, optional per-datagram authentication and
payload encryption, pluggable persistence, runtime observability, and a lightweight dateless
read-only mirror. A 2026-06 adversarial audit filed implementation-level correctness bugs as
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
| F7 | High | crafted `HashSegment` → panic/underflow | ✅ | #112 — bound validation + `checked_sub` |
| F8 | High | `DefaultHasher` unstable on the wire | ✅ | #111 — wire fingerprint is BLAKE3 (`version_hash` still fixed-key `DefaultHasher`) |
| F9 | High | UDP amplification / reflection | ◐ | mitigated by #108 (auth) + #106; rate-limiting / path validation still open |
| F10 | High | IP-scan discovery, O(N²) membership | ◐ | [#147](https://github.com/Akvize/reconcile-rs/issues/147) — `Discovery` port + `DnsDiscovery` (k8s headless-Service DNS, no IP-scan) lands a cloud-native discovery path; bounded-fan-out membership (SWIM/HyParView) still open |
| F11 | High | no property-testing / fuzzing | ✅ | #113 — `tests/proptest_hrtree.rs`, `tests/fuzz_packets.rs` |
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
- [x] Property tests for convergence (`proptest_hrtree.rs`)
- [x] Malformed-packet fuzz harness (`fuzz_packets.rs`)
- [x] Security model documented (README "Security model")
- [x] Pluggable persistence documented (README "Persistence")
- [ ] Semver and publish policy — `0.2.1` exists but policy is not yet settled ([#204](https://github.com/Akvize/reconcile-rs/issues/204))
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
  path (headless-Service DNS + grace-period decommission of vanished pods) that sidesteps the random
  IP-scan; bounded-fan-out membership (SWIM/HyParView) still open
  (F10 — [#147](https://github.com/Akvize/reconcile-rs/issues/147)).
- ◯ Bound the `peers` map; cap messages/segments per datagram; bincode limit
  (F18 — [#150](https://github.com/Akvize/reconcile-rs/issues/150), F19 — [#151](https://github.com/Akvize/reconcile-rs/issues/151)).
- ◯ Larger-than-datagram payloads — [#2](https://github.com/Akvize/reconcile-rs/issues/2).
- ✅ Lightweight dateless read-only mirror (`ReconcileMirror`), #109-safe — PR #133 ([#128](https://github.com/Akvize/reconcile-rs/issues/128)).
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
[#204](https://github.com/Akvize/reconcile-rs/issues/204) release pipeline/version drift,
[#205](https://github.com/Akvize/reconcile-rs/issues/205) hygiene batch) are security/robustness-grade
and scheduled per [#206](https://github.com/Akvize/reconcile-rs/issues/206).

### Architecture refactor
Tracked in [`ARCHITECTURE.md`](./ARCHITECTURE.md) and issue
[#138](https://github.com/Akvize/reconcile-rs/issues/138) (hexagonal ports & adapters). Steps:
bound bundles & encapsulation → dissolve diff traits → `Clock` port → `Entry`/`State` type →
`Transport`/`Codec` ports → workspace split. None of these change runtime behaviour except the
`Entry`/`State` step (wire/on-disk format), and all preserve the invariants below.

Refinements adopted after a `file:line` review of the sequence (see issue
[#138](https://github.com/Akvize/reconcile-rs/issues/138)): the `Clock` port returns the concrete
`Timestamp` (no generic associated type); the `Entry`/`State` step also dissolves `Projectable`/`ValueOnly`
into `State<V>` and is guarded by invariant 8 below; the `Codec` port carries a decode cap and the
`BincodeCodec` adapter sets `with_limit` (partially closing
[#151](https://github.com/Akvize/reconcile-rs/issues/151)); the `Transport`/`Codec` ports live in
`reconcile-net` (the `Clock`/`Persistence` ports in `reconcile-core`).

Progress:
- ✅ Step 1 — bound bundles & encapsulation ([#140](https://github.com/Akvize/reconcile-rs/issues/140), PR #155).
- ✅ Step 2 — dissolve the diff traits ([#141](https://github.com/Akvize/reconcile-rs/issues/141), PR #156):
  `HashRangeQueryable` / `Diffable` removed; range-hash querying is now inherent on `HRTree`
  (`hash` / `insertion_position` / `key_at` `pub(crate)`, `len` / `is_empty` public) and
  `start_diff` / `diff_round` are free functions in the `pub(crate) proto` module over `&HRTree`,
  with `HashSegment` / `DiffRange` no longer on the public surface. Iso-functional; invariants 3–4
  preserved byte-for-byte.
- ✅ Step 3 — `Clock` port ([#142](https://github.com/Akvize/reconcile-rs/issues/142), PR #158): `pub trait Clock`
  (`now`/`observe`, returning the concrete `Timestamp`) is the domain's time seam; `HlcClock` is the
  default adapter owning the single `chrono` read; the engine holds it as `Arc<dyn Clock>` (object-safe,
  no clock type parameter on the engine/store/`Config`) and mints/observes only through it. A
  deterministic `ManualClock` test adapter makes HLC behaviour reproducible without wall-clock time.
  Iso-functional; invariant 2 (HLC total order) preserved.
- ✅ Naming cleanup alongside the port: type `Hlc`→`Timestamp` (#159) and module `hlc`→`clock` (#163).
- ◯ Steps 4–6 — `Entry`/`State` → `Transport`/`Codec` → workspace split.

---

## 5. Load-bearing invariants

Any change must preserve these (they encode the fixes above):

1. Fingerprint format & arithmetic (`[u64;4]`, per-element BLAKE3, add/sub mod 2²⁵⁶) + golden vectors.
2. HLC total order `(wall_ms, counter, node_id)`; merge uses strict `>`.
3. Range emptiness/equality decided on `size`, never on `hash`.
4. `diff_round` validates incoming bounds (`checked_sub`, no `unimplemented!`).
5. Authenticate-before-deserialize (MAC on raw bytes before decoding).
6. Causal-stability tombstone gate before GC.
7. `version_hash` determinism.
8. Value-only projection hash is timestamp-less — the dated cell keeps a timestamp-**inclusive**
   `Hash` (used by `version_hash`), while its value-only projection (the dateless mirror channel)
   hashes the value alone, so a dated store and a dateless mirror agree on per-element fingerprints.

---

## 6. Maintaining this file

Update on any change that moves a finding's status, ticks a maturity box, or closes a roadmap item:
bump **Last updated** and **Baseline**, flip the status cell (and add the PR/issue), and keep the
headline honest. SOTA positioning and rationale stay in [`SOTA.md`](./SOTA.md); target design stays
in [`ARCHITECTURE.md`](./ARCHITECTURE.md).

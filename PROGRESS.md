# Project status — `reconcile-rs`

> **Living document.** Real-time view of correctness, security, and maturity. Complements, does not
> duplicate: [`SOTA.md`](./SOTA.md) (durable positioning/glossary/bibliography) and
> [`ARCHITECTURE.md`](./ARCHITECTURE.md) (the architecture and its invariants, §5 there).
> **[Issue #138](https://github.com/Akvize/reconcile-rs/issues/138)** tracks the architecture
> migration to closure.

- **Last updated:** 2026-08-11.
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
  (closed 2026-08-10; this file had claimed that prematurely while the issue was still open).
  Delivered in the callback shape the issue itself recommends; the `iter` half of its title never
  landed and is tracked by [#270](https://github.com/Akvize/reconcile-rs/issues/270)/[#271](https://github.com/Akvize/reconcile-rs/issues/271)/[#291](https://github.com/Akvize/reconcile-rs/issues/291).

### API and performance
- ◐ **Public-API audit (2026-08-10)** — the public surface of all five crates reviewed against what
  dependents need (`BTreeMap` and the RSOS contract for `rsos`, RBSR Algorithm 1/2 for `rbsr`, the
  embedded-IMDG baseline for the facade, and implementability-from-outside for the four ports). The
  read/write map surface is confirmed coherent; the defects sit underneath it, in trait impls,
  generic bounds and panic-safety. **Nine P0 items, none previously tracked**, filed with their
  evidence as [#282](https://github.com/Akvize/reconcile-rs/issues/282)–[#299](https://github.com/Akvize/reconcile-rs/issues/299)
  under [#206](https://github.com/Akvize/reconcile-rs/issues/206);
  [#72](https://github.com/Akvize/reconcile-rs/issues/72)/[#92](https://github.com/Akvize/reconcile-rs/issues/92)/[#137](https://github.com/Akvize/reconcile-rs/issues/137)/[#185](https://github.com/Akvize/reconcile-rs/issues/185)/[#189](https://github.com/Akvize/reconcile-rs/issues/189)/[#202](https://github.com/Akvize/reconcile-rs/issues/202)/[#205](https://github.com/Akvize/reconcile-rs/issues/205)/[#257](https://github.com/Akvize/reconcile-rs/issues/257)
  were widened rather than duplicated. Three P0s carry consequences beyond ergonomics:
  `FingerprintTreeMap`'s `PartialEq` decides equality on the fingerprint alone, ignoring size —
  F1/#106's class, in the map's own `==`, against [`ARCHITECTURE.md`](./ARCHITECTURE.md) §5
  invariant 3 ([#282](https://github.com/Akvize/reconcile-rs/issues/282)); every `ReplicatedMap`
  write panics outside a Tokio runtime, and `with_discovery`'s authoritative-source precondition is
  a `debug_assert!`, a no-op in release
  ([#283](https://github.com/Akvize/reconcile-rs/issues/283)). Two need a maintainer **decision**
  before the first split-aware release freezes the signatures:
  [#288](https://github.com/Akvize/reconcile-rs/issues/288) (`Clock` is re-exported as a port with
  zero public implementors and no injection seam) and
  [#298](https://github.com/Akvize/reconcile-rs/issues/298) (the generic monoid — the one
  non-additive change, since `RangeAggregate` is the wire type; see `ARCHITECTURE.md` §7).
- ◐ **RSOS/AELMDB literature pass (2026-08-10)** — arXiv:2603.19820 read in full against its four
  published repositories (`amparore/{aelmdb, negentropy-aelmdb, lmdbxx-aelmdb, bench-aelmdb}`). The
  design verdict is unchanged and now better evidenced: it *is* the same design, and its combiner is
  literally ours (256-bit addition over LE 64-bit limbs with carry). Two deltas run in our favour
  and were not visible from the abstract — AELMDB never hashes (its lift extracts a byte slice the
  application must have made collision-resistant, where `rsos` computes BLAKE3 over the canonical
  encoding of key *and* value), and Negentropy's comparison map is SHA-256 truncated to 128 bits,
  which the paper itself (§6.1) calls only "probabilistically sound" with the collision analysis out
  of scope, where `rbsr` compares the aggregate itself. Three outcomes, all landed here: `SOTA.md`
  §1.3/§2.1/§2.2/§2.3 revised, six references added; two factual errors corrected (`SOTA.md`'s
  `O(d log n)` claim for *this* implementation, and `rsos/src/lib.rs` calling AELMDB
  "content-addressed" — it is a copy-on-write B+-tree addressed by page number); and #257
  reclassified with a measurement behind it (above). One caveat worth keeping: the paper's headline
  4.69×–13.98× is persistent-vs-persistent, its in-memory reference backend beats AELMDB in all six
  families despite not being an RSOS at all, and its widest-margin family runs at `d ≈ 21 % of n` —
  the opposite of the large-n/small-d profile. Nothing in it argues we should move off an in-memory
  store; it argues that *if* we add persistence, the aggregates belong in the persistent tree rather
  than in an auxiliary index.
- **PRs #218–#221 were never merged** (established 2026-08-10). Their bodies read as landed work
  ("MSRV declared", "`LoadError` re-exported", "Closes #189"); `git log -S` finds **zero**
  occurrences of every identifier they introduce, so nothing was reverted and the workspace split is
  not implicated. All four sat on a 7-deep stacked chain whose every PR targeted the *previous
  feature branch* rather than `main`, leaving the stack inert; closed unmerged on 2026-08-07 once
  `main` had moved past them. The branches are live on the remote — re-land fresh against `main`
  (#217→#248 is the precedent) rather than rebasing the stack. #189/#202/#205 stayed correctly open
  throughout. Nothing systematic: 10 of 10 spot-checked merged PRs survive intact in `main`. The one
  gap worth closing mechanically, per AGENTS.md §10: nothing flags a PR whose base is not `main` and
  which has been open for months.
- ✅ Round out the write API: atomic `update`/`upsert`/`get_or_insert_with`, `clear`/`retain`/
  `delete_range`, and a `load_bulk` no-broadcast seed path (`just_*` demoted off the published
  surface) — [#180](https://github.com/Akvize/reconcile-rs/issues/180) (closed 2026-08-10; this file
  had claimed that prematurely while the issue was still open). One bullet was unshipped and is now
  [#299](https://github.com/Akvize/reconcile-rs/issues/299): conditional writes
  (`compare_and_swap`/`insert_if_absent`) exist nowhere in the tree — and under LWW without
  consensus a cluster-wide compare-and-swap is not soundly implementable, so that bullet needs a
  decision rather than an implementation.
- ✅ `FingerprintTreeMap`'s own comfort write API (`rsos`, one layer below `ReplicatedMap`):
  `contains_key`, `clear`, `retain`. A `std`-style `entry()` returning a live `&mut V` is
  deliberately not offered — see the rationale on `with_mut`'s doc comment (a bare mutable handle
  lets a caller skip the re-lift/fingerprint-propagation step `with_mut` guarantees); the
  get-or-insert half of what `entry()` usually buys already exists one layer up
  (`ReplicatedMap::upsert`/`get_or_insert_with`).
- ✅ **The split fan-out was a communication-complexity regression, not a tuning gap** —
  [#257](https://github.com/Akvize/reconcile-rs/issues/257), reclassified 2026-08-10 (it read
  "unbenchmarked and hard-coded"), closed 2026-08-11.

  **Step A — the seam.** The loop's two tuning decisions are now a `RefinementPolicy` in `rbsr`
  (`Decision` = SKIP/IDLIST/SPLIT, `Comparison`, `SplitStride`, `FanOut`); `protocol_round_with_policy`
  takes one, `protocol_round` pins a named `DEFAULT_POLICY`. It landed behaviour-preserving — the
  default was `SqrtFanOut`, byte-for-byte the previous rule, so `tests/wire_format.rs`'s golden
  vector and the convergence proptests passed unchanged. Three policies ship, each named for the
  rule it applies rather than for where it comes from: `FixedFanOut`, `SqrtFanOut`,
  `EnumerateBelowThreshold`. The policy is local and never advertised; mixed pairs converge, pinned
  by `peers_running_different_policies_still_converge` and a proptest over every pair.
  `protocol_round` also returns a `RoundOutcome` tally, which is the only signal a consumer without
  a `tracing` subscriber gets for the malformed-segment drop path.

  **Step B — the measurement.** `benches/protocol.rs` sweeps policy × `n` × `d` × difference
  clustering, reporting rounds, bytes, datagrams, IP fragments, IDLIST *elements* and local
  `Aggregate`/`Rank`/`Select` counts, plus a timed drive (the paper's `T_loc`). Locating one missing
  element, differences scattered:

  | n | `√m` bytes / msgs / `T_loc` | `b`=16 bytes / msgs / `T_loc` |
  |---:|---:|---:|
  | 10³ | 2 041 / 6 / 12.9 µs | 1 701 / 6 / 8.2 µs |
  | 10⁴ | 5 395 / 8 / 43.8 µs | 2 195 / 6 / 18.2 µs |
  | 10⁵ | 16 553 / 6 / 460 µs | 2 789 / 6 / 25.2 µs |
  | 10⁶ | **53 046** / 8 / **2.10 ms** | **3 834** / 8 / **45.0 µs** |

  **This corrected a claim this file used to make.** Rounds were recorded here as "`Θ(log log n)`,
  flat at 6–8 … where fixed-`b` pays `O(log n)`". Measured head to head at n = 10⁶, *both* take 8
  one-way messages (log₁₆ 10⁶ ≈ 5 ≈ the iterated-square-root depth); the separation only reaches 2×
  near n ≈ 10¹². So `√m` paid ~14× the bytes, ~13× the local RSOS queries and 47× the local CPU and
  bought nothing observable back. Corrected in `SOTA.md` §1.3/§2.2. Two qualifications: every bench
  runs at RTT ≈ 0 ([#280](https://github.com/Akvize/reconcile-rs/issues/280)), and the gap closes to
  ~7 % at d = 100 scattered — `√m` is worst exactly in the small-`d` regime RBSR exists for, and
  competitive once `d` approaches `√n`. The earlier ceiling finding also sharpens: at n = 10⁶ with
  d = 100 the widest round reaches **160 908 B over 3 300 ranges = 3 datagrams / ~189 fragments**, so
  the ceiling is reachable at 10⁶, not only at 10 M — still degrading into extra datagrams rather
  than being dropped, as `send_messages_paced` chunks at `BUFFER_SIZE`.

  `EnumerateBelowThreshold` wins the refinement column and loses on values: at n = 10⁵, d = 100 it
  saves 40 % of refinement bytes while enumerating **5 036 elements against 100**. Break-even is
  13–37 B per entry, and a `Timestamp` alone encodes to 23 B, so the paper's `t` is a net loss on
  this wire format. That is why `benches/protocol.rs` reports IDLIST elements alongside ranges.

  **The branching factor was swept** (`fan_out_sweep`, *b* = 2…256) rather than inherited from
  Negentropy. At n = 10⁶, d = 1:

  | `b` | 2 | 4 | 8 | 16 | 32 | 64 | 256 |
  |---|---:|---:|---:|---:|---:|---:|---:|
  | bytes | 2 061 | **1 960** | 2 613 | 3 834 | 5 021 | 9 668 | 25 880 |
  | one-way msgs | 22 | 12 | 10 | 8 | **6** | 6 | 6 |
  | widest round | 96 B | 202 B | 414 B | 802 B | 1 614 B | 3 238 B | 12 982 B |
  | `T_loc` | 25.1 µs | **21.8 µs** | 33.0 µs | 48.5 µs | 73.3 µs | 172 µs | 975 µs |

  Bytes and `T_loc` follow *b*/ln *b* (minimum near *b* = 3–4); messages fall as log_*b* n to a floor
  no larger *b* improves on; the widest round grows linearly in *b* and is the hard ceiling — already
  past one datagram at *b* = 16 for n = 10⁵, d = 100.

  **Decision: the default is `FixedFanOut(FanOut::NEGENTROPY)`** (2026-08-11). *b* = 16 is the only
  swept value **never worse than `√m` on rounds** in any measured (n, d, clustering), so no
  deployment traded latency for the bandwidth win — which is what made the switch decidable without
  an RTT lane. *b* = 32 saves one more round-trip at 10⁶ alone, for +31 % bytes, +51 % CPU and double
  the widest round; *b* = 4 is the bytes/CPU optimum at two extra round-trips, break-even near an RTT
  of 8 µs at 1 Gb/s, i.e. worth it only in-process. The switch is a **behaviour** change, not a wire
  change — mixed pairs converge — so a cluster migrates one node at a time with no flag day. With a
  constant *b*, communication is `O(d log n)` again rather than `Θ(√n)` and the paper's
  `T_loc = O(hL + bhI + K)` is quotable for this crate. `SqrtFanOut` stays shipped, supported and
  pinned by its own test.

  **Still open.** Exposing a policy through `reconcile`'s facade (`Config` is `Copy`, so a boxed
  policy needs a different carrier), and the interaction with the 40 B/range wire aggregate (~79 % of
  the bytes measured under `√m`) — now separable, since the fan-out is a caller's choice rather than
  an edit to the protocol loop.
### Tracked, not yet started
- Bulk-build throughput, point-read indexing, per-entry memory overhead, configurable snapshot
  cadence — [#170](https://github.com/Akvize/reconcile-rs/issues/170)–[#173](https://github.com/Akvize/reconcile-rs/issues/173).
- Comparative benchmark suite — [#174](https://github.com/Akvize/reconcile-rs/issues/174), rescoped
  to in-repo reproducibility (external comparisons optional/non-CI). `benches/system.rs` covers
  point-read, memory footprint, bulk-load, cold anti-entropy convergence, gossip fan-out/propagation
  scaling, and durable rejoin — see `benches/README.md`. Still open: an external (e.g. Redis)
  comparison, non-CI and feature-gated.
- Cut sync latency below O(log n) sequential RTTs (hybrid RBSR + Rateless IBLT, generic monoid
  summary) — [#185](https://github.com/Akvize/reconcile-rs/issues/185).
- Pluggable per-value conflict resolution beyond LWW-Register —
  [#184](https://github.com/Akvize/reconcile-rs/issues/184).
- `FingerprintTreeMap` iterator refinements: `size_hint`/`ExactSizeIterator`/`FusedIterator`,
  `Debug`/`Clone`, a fully-lazy traversal (stack built on first `next()` rather than at
  construction), `DoubleEndedIterator` + `seek_lower_bound`/`seek_upper_bound` — SOTA.md §2.4 P1
  item 5's "remaining" note — [#92](https://github.com/Akvize/reconcile-rs/issues/92) (umbrella;
  consolidates the closed #89–#91).
- Zero-copy borrowing iterator on `ReplicatedMap`/`ReadReplicaMap` (`for x in map.iter()`, no clone,
  no callback): blocked by the streaming-iterator problem (`Iterator::Item` can't borrow from
  `&mut self`) combined with `#![forbid(unsafe_code)]` ruling out a self-referential
  guard-plus-iterator struct. Interim, no-unsafe escape hatch (expose the read guard; caller holds
  the lock for the iteration) — priority: low, only if a real workload needs it —
  [#270](https://github.com/Akvize/reconcile-rs/issues/270). The actual fix needs a persistent
  copy-on-write core map (`Arc`-snapshot reads, no lock held) — epic
  [#271](https://github.com/Akvize/reconcile-rs/issues/271), which converges with the prolly-tree /
  structural-sharing (content-addressing) direction SOTA.md §2.4 already notes as the big
  persistence gap.
  - **Decide #271 against LMDB explicitly, before writing the copy-on-write core.** A copy-on-write
    B+-tree with lock-free MVCC readers and a single writer is what the epic sets out to build; it
    is also, exactly, what LMDB already is — and arXiv:2603.19820's AELMDB shows the full RSOS
    contract (composable range aggregates + rank/select) fits inside that engine's own branch pages.
    The number that was missing from the build-vs-adopt call now exists: on the paper's own
    published data, the persistent aggregate-augmented engine costs **~2× an in-RAM array** on the
    reconciliation hot path (`Vector`/AELMDB `T_rec` ratio 0.39×–0.59× across six scenario families,
    recomputed from `bench-aelmdb`'s `results-linux.csv`), for ~1.0× the disk and *lower* RSS than
    the auxiliary-tree baseline. Adopting is not obviously right — `#![forbid(unsafe_code)]` (AGENTS
    §5) rules out mmap and makes an LMDB binding an FFI dependency in the domain's data path, the
    keys would have to be encoded into byte strings, and AELMDB is a one-commit research fork. But
    those are arguments to *write down*, not to leave implicit. The `Rsos` trait already makes the
    question askable without disturbing anything above it: a second realization is an
    implementation of one trait. Blocking on nobody; needs a maintainer decision recorded on #271.

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

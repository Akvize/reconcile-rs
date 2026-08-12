# Project status — `reconcile-rs`

> **Living document.** Real-time view of correctness, security, and maturity. Complements, does not
> duplicate: [`SOTA.md`](./SOTA.md) (durable positioning/glossary/bibliography) and
> [`ARCHITECTURE.md`](./ARCHITECTURE.md) (the architecture and its invariants, §5 there).
> **[Issue #138](https://github.com/Akvize/reconcile-rs/issues/138)** tracks the architecture
> migration to closure.

- **Last updated:** 2026-08-11.
- **Release 1.0.0:** what the release commits to and what gates it — **§6 below**, mirroring
  [#206](https://github.com/Akvize/reconcile-rs/issues/206) (rewritten 2026-08-11 around an explicit
  definition of 1.0; the 2026-06 phase plan is archived in its comments).
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
correctness-grade. **The 2026-08-10 public-API audit changed that picture** (§4): two of its nine P0s
are correctness-grade rather than ergonomic — [#283](https://github.com/Akvize/reconcile-rs/issues/283)
(`with_discovery`'s guard is a release-build no-op, so a speculative source releases the
causal-stability gate) and [#284](https://github.com/Akvize/reconcile-rs/issues/284) (a third-party
RSOS backend is a remote-driven panic surface). Both are Gate B in §6. Remaining work beyond the
gate list is scaling and the confidentiality roadmap.

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
| F18 | Medium | resource exhaustion (`peers` map, bincode bomb) | ✅ | per-datagram message/segment caps landed (#151); the `peers` map is bounded by `Config::max_peers` (default 1024, `src/replicated_map.rs:84`) — [#150](https://github.com/Akvize/reconcile-rs/issues/150) closed by PR #245 |
| F19 | Low | dependency hygiene | ◐ | bincode `with_limit` landed (#151); `overflow-checks` and `cargo audit`/`cargo deny` in CI still absent — [#312](https://github.com/Akvize/reconcile-rs/issues/312) (previously mis-cited here as #203/#205, neither of which mentions either item) |

**Score:** 14 resolved · 5 partial (F9, F10, F16, F17, F19) · 0 open. All Critical resolved; all
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
- [ ] `CHANGELOG.md` + a `0.2.1` → 1.0 migration guide — [#310](https://github.com/Akvize/reconcile-rs/issues/310)
- [x] CI code coverage + doc-tests (#97) — Codecov (`cargo llvm-cov`, informational) + `cargo test --doc`
- [ ] `cargo audit` / `cargo deny` in CI — [#312](https://github.com/Akvize/reconcile-rs/issues/312)
- [ ] Declare + CI-pin MSRV (`rust-version`), on all five manifests — [#189](https://github.com/Akvize/reconcile-rs/issues/189)
- [ ] `[package.metadata.docs.rs] all-features` + `keywords`/`categories` on `reconcile` — [#189](https://github.com/Akvize/reconcile-rs/issues/189)
- [ ] `overflow-checks = true` in the release profile — [#312](https://github.com/Akvize/reconcile-rs/issues/312)
- [x] Bound the `peers` map in unauthenticated mode — [#150](https://github.com/Akvize/reconcile-rs/issues/150) (PR #245; `Config::max_peers`, default 1024)
- [ ] `SECURITY.md` + private vulnerability reporting — [#313](https://github.com/Akvize/reconcile-rs/issues/313)
- [ ] Mechanical semver / public-API gate in CI — [#311](https://github.com/Akvize/reconcile-rs/issues/311)

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
  were widened rather than duplicated. Three P0s carry consequences beyond ergonomics.
  [#282](https://github.com/Akvize/reconcile-rs/issues/282) — `FingerprintTreeMap`'s `PartialEq`
  deciding equality on the fingerprint alone, ignoring size, which is F1/#106's class inside the
  map's own `==`, against [`ARCHITECTURE.md`](./ARCHITECTURE.md) §5 invariant 3 — is **fixed**, with
  the two other defects in that issue: `cc4d7c4` (equality on the whole aggregate), `bb64f56`
  (ranges by value) and `5b8d138` (panic-safe `with_mut`); `Eq` is bounded at
  `rsos/src/fingerprint_tree_map.rs:760`. Still open:
  [#283](https://github.com/Akvize/reconcile-rs/issues/283) — every `ReplicatedMap` write panics
  outside a Tokio runtime, and `with_discovery`'s authoritative-source precondition is a
  `debug_assert!`, a no-op in release. Two need a maintainer **decision**
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

## 6. Release 1.0.0 — the gate list

Status mirror of [#206](https://github.com/Akvize/reconcile-rs/issues/206), which owns the plan and
the reasoning. Update a row here when its issue moves; don't restate the argument.

**What 1.0.0 commits to** — three freezes: the public API of `reconcile` (semver from then on), the
wire (any 1.x node reconciles with any other 1.x node), and the on-disk format. The third is
**already true**: `src/snapshot.rs:42-46` writes `RCNL` + a `u32` version and rejects a foreign one
with a descriptive error. And three things it does **not** claim, which the README must say in the
same breath: not feature-complete (full replication only), not a consensus system (AP + LWW, so no
cluster-wide conditional writes — [#299](https://github.com/Akvize/reconcile-rs/issues/299)), not
authenticated by default (AGENTS.md §8).

**A gate is** (A) a non-additive change, so deferring it costs a 2.0.0; (B) a defect that makes one
of those three claims false the day it is made; or (C) something without which the release cannot
happen, be found, or be verified. Everything else is post-1.0 **by construction** — including work
more valuable to users than most of this list (#271, #185, #190). Valuable ≠ blocking.

### Gate A — non-additive (deferring costs a 2.0.0)

| Issue | Freezes wrong if shipped as-is |
|---|---|
| [#298](https://github.com/Akvize/reconcile-rs/issues/298) | `Aggregate<M>` — the **only** truly non-additive item: `RangeAggregate` is the wire type, so generalizing later is a wire break *plus* an API break (`ARCHITECTURE.md` §7) |
| [#309](https://github.com/Akvize/reconcile-rs/issues/309) | The wire carries no version field. Adding one after the freeze **is** the break it prevents; today a peer on another wire generation is indistinguishable from noise (dropped by the F2/#107 hardening) |
| [#297](https://github.com/Akvize/reconcile-rs/issues/297) | Foreign types in public signatures (`parking_lot`, `ipnet`, `rand`, `tokio`, `bincode`) — every dependency bump becomes a breaking change of *our* API |
| [#286](https://github.com/Akvize/reconcile-rs/issues/286) | `ClusterKey` at the boundary — forces dropping `Config: Copy`, which is what structurally forbids the `Drop` `zeroize` needs |
| [#293](https://github.com/Akvize/reconcile-rs/issues/293) | `Config`: private fields, `#[non_exhaustive]`, one behaviour for the `MAX_NETS` cap; and `Config::default()` can never converge (`port: 0` doubles as destination) |
| [#292](https://github.com/Akvize/reconcile-rs/issues/292) | `run(self, shutdown)` signature + `peers`/`members`/`local_addr`/`sync_state`/`snapshot_now` |
| [#285](https://github.com/Akvize/reconcile-rs/issues/285) | `Authenticator` must carry a multi-key accept set — a public-enum change, and what makes #137 expressible |
| [#287](https://github.com/Akvize/reconcile-rs/issues/287) | `Transport::Addr` (dead freedom) and `Discovery`'s missing error type — both trait changes |
| [#288](https://github.com/Akvize/reconcile-rs/issues/288) | **Decision**: `Clock` is a published port with zero public implementors and no injection seam — open it or close it |
| [#296](https://github.com/Akvize/reconcile-rs/issues/296) | `add_pre_insert`/`add_on_update` are setters that silently discard the previous hook |
| [#294](https://github.com/Akvize/reconcile-rs/issues/294) | `ReadReplicaMap::fingerprint` never equals `ReplicatedMap::fingerprint` between converged nodes — rename before the trap freezes in |
| [#289](https://github.com/Akvize/reconcile-rs/issues/289) [#290](https://github.com/Akvize/reconcile-rs/issues/290) [#291](https://github.com/Akvize/reconcile-rs/issues/291) | *Partial*: the naming freezes only (`ItemRange` vs `Range`, retiring `for_testing`, `Borrow` lookup). The rest is additive |

### Gate B — the 1.0 claims must be true

| Issue | Why a 1.0 cannot carry it |
|---|---|
| [#283](https://github.com/Akvize/reconcile-rs/issues/283) | `with_discovery`'s precondition is a `debug_assert!` — **a no-op in release** (`src/replicated_map.rs:429`), so a speculative source decommissions live members and releases the causal-stability gate (`ARCHITECTURE.md` §5 inv. 6). Plus: writes panic off-runtime; `get`-then-`insert` self-deadlocks |
| [#284](https://github.com/Akvize/reconcile-rs/issues/284) | `RsosView` states no inter-method laws → a third-party backend is a panic surface driven by the remote peer's key choice (`rbsr/src/protocol.rs:389`). Publishing the crates means publishing the contract |
| [#203](https://github.com/Akvize/reconcile-rs/issues/203) | `mac-hmac` is compiled by **no** CI lane (`--all-features` lets `mac-blake3` win) — a published crypto backend never built. Also single-OS |
| [#202](https://github.com/Akvize/reconcile-rs/issues/202) | Durability: snapshot clone stalls writes, missing dir fsync, crash-loop on transient load failure |
| [#230](https://github.com/Akvize/reconcile-rs/issues/230) | A value larger than one datagram never converges, silently — reject at write time |
| [#205](https://github.com/Akvize/reconcile-rs/issues/205) | Items 1, 3, 5, 7, 8. Item **7** is also a Gate-A item: `internal-testing` is an ordinary feature of a published crate, so 1.0 would freeze `reconcile::testing` into the public surface |

### Gate C — release mechanics and semver coherence

| Issue | State |
|---|---|
| [#308](https://github.com/Akvize/reconcile-rs/issues/308) | ✅ **Decided 2026-08-11**, recorded in AGENTS.md §11. `rsos`/`lww-register`/`reconcile-gossip` have types in `reconcile`'s public API, so their majors are coupled to its and they go **`1.0.0` with it**; `rbsr` has none — no `pub use rbsr::`, and `initial_ranges`/`protocol_round`/`RangeAggregate` are reached only from the `pub(crate)` `src/replica.rs` — so it **stays `0.x`**, gated on [#289](https://github.com/Akvize/reconcile-rs/issues/289). Chosen for reversibility: `0.x → 1.0` later is additive, `1.0 → 2.0` is not — **and #307 vindicated that within the day**: `feat(rbsr)!` landed on `main` 2026-08-11, changing `protocol_round`'s return type to `RoundOutcome` and retiring `SqrtFanOut` as the default, a breaking `rbsr` API change that a 1.0 line would have had to carry as a 2.0. Remaining: the version bumps, the crates.io card wording, a mechanical re-check via #311 |
| [#189](https://github.com/Akvize/reconcile-rs/issues/189) | Reopened 2026-08-11 — was closed `completed` while nothing landed. No `rust-version` anywhere, no MSRV lane, no `docs.rs` metadata (so `encryption`/`zeroize`/`metrics`/`dns-hickory` are invisible on the rendered docs), no `keywords`/`categories` on the published crate |
| [#310](https://github.com/Akvize/reconcile-rs/issues/310) | No `CHANGELOG.md`, no `0.2.1` → 1.0 migration guide |
| [#311](https://github.com/Akvize/reconcile-rs/issues/311) | No mechanical semver / public-API gate — after 1.0 that rule would be enforced by eye, which AGENTS.md §10 forbids. Also what re-verifies #308's "no `0.x` crate in the public API" mechanically rather than by review |
| [#312](https://github.com/Akvize/reconcile-rs/issues/312) | `cargo audit`/`cargo deny` and `overflow-checks`, previously mis-cited in this file as tracked by #203/#205 |
| [#313](https://github.com/Akvize/reconcile-rs/issues/313) | No `SECURITY.md` — a documented threat model with no disclosure channel |
| [#204](https://github.com/Akvize/reconcile-rs/issues/204) | **Mostly resolved in the tree**: `tags.yml` verifies tag against manifest and publishes in dependency order; the docs no longer say `0.0.0-git`. What remains is its Problem 3 — the version decision |

**The version decision.** `0.2.1` is live and predates the split, so the next tag is breaking whatever
number it carries. Straight to `1.0.0` is defensible only if Gate A is complete; otherwise cut a
`0.3.0` carrying the breaks and tag 1.0.0 from it. Which one, and why, is a required output of #206.

### Not gates

Open and wanted, none blocking: performance/evidence (#170–#174, #187, #280, #281 — #257 landed
2026-08-11 via #307 and is no longer among them), the
persistent-core-map epic (#270–#277), scaling (#185, #186, #190, #147, #178), the grid layer (#193,
#191, #192, #184), the crypto roadmap (#96, #135, #136, #137), docs/ergonomics (#72, #92, #231,
#232), and the additive remainder of #289/#290/#291 — #289 now also carrying `rbsr`'s eventual
promotion to 1.0, per #308. #299 is a decision to write down and should ride along regardless.

---

## 7. Maintaining this file

Update on any change that moves a finding's status, ticks a maturity box, closes a roadmap item, or
closes a §6 release gate: bump **Last updated**, flip the status cell (add the PR/issue), keep the
headline honest. §6 mirrors [#206](https://github.com/Akvize/reconcile-rs/issues/206) — when the two
disagree, #206 wins and this file is what is stale. SOTA
positioning stays in [`SOTA.md`](./SOTA.md); architecture and invariants stay in
[`ARCHITECTURE.md`](./ARCHITECTURE.md).

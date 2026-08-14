# State of the Art — `reconcile-rs` positioning

> **Reference document.** Where `reconcile-rs` sits in the landscape of set reconciliation, diffable
> data structures, and replica consistency — plus a glossary and bibliography. This is **durable
> background**: the field positioning and the design taxonomy move slowly, unlike the code. It
> deliberately carries **no status or findings** — for the live correctness, security and maturity
> state see [`PROGRESS.md`](./PROGRESS.md); for the target design see [`ARCHITECTURE.md`](./ARCHITECTURE.md).
>
> - **Literature survey dated:** 2026-05-30, with a targeted addendum on 2026-08-10 (arXiv:2603.19820
>   read in full against its four published repositories; §1.3/§2.1/§2.2/§2.3 revised and eight
>   references added — sources cited inline and in the [bibliography (§4)](#4-bibliography)) and a
>   cross-community pass on 2026-08-14 ([§4.1](#41-cross-community-vocabulary) — the `cs.IT`/`cs.NI`
>   dialect this document had never searched; §2.2 revised, [§4.3](#43-search-log) opened).
> - **Scope:** the FingerprintTreeMap as a *data structure* and RBSR as an *algorithm*, compared to the published
>   state of the art — not an audit of any particular commit.
> - **Navigation:** a [glossary (§3)](#3-glossary) defines ~120 terms and an
>   [alphabetical index (§5)](#5-alphabetical-index) lists them; first uses in the text link to it.
> - **`Fxx`** denotes a finding from the original code audit; its current status lives in
>   [`PROGRESS.md`](./PROGRESS.md).

---

## 1. Objective and relevance vs the SOTA

### 1.1 The stated objective

Per the README: *"a scalable Web service with a non-persistent and eventually consistent key-value
store [...] avoiding any latency related to using an external store such as Redis. All the data is
available locally on all instances"*. In other words: **each web-service replica embeds the full
dataset in memory**, replicas reconcile peer-to-peer, and the user is notified of changes via an
insertion hook.

### 1.2 Relevance and real niche

The niche is **real but narrow**: there is no mature equivalent in the Rust/Tokio ecosystem of
Hazelcast's *Replicated Map* or Akka/Pekko's *Distributed Data* (all JVM). For a **read-heavy** Rust
web service with a moderate working set and rare/benign conflicts (feature flags, routing tables,
presence, configuration), an in-memory replicated cache with local O(log n) reads and no Redis
dependency is legitimately attractive.

**But the "scalable / avoid Redis" positioning inverts the real trade-offs:**

- The latency argument only holds for **reads**. Writes are only *eventually* visible on peers;
  "avoiding Redis latency" actually amounts to **trading a synchronous consistent store for an
  asynchronous inconsistent one** — a consistency-model change dressed up as a latency optimization.
- The topology **does not scale by construction**: full dataset on every replica → memory bounded by
  the smallest node, and **every write is amplified to all nodes** → write throughput *decreases* as
  replicas are added. This is the documented failure mode of replicated caches (Oracle Coherence,
  Apache Ignite). Pekko Distributed Data explicitly recommends **not exceeding ~100,000 entries** in
  full replication — to be compared with the README's "millions of elements" promise.

### 1.3 The SOTA of set reconciliation (sourced)

| Family | Comm. | Compute | RTT | Knows *d*? | Adversarial robustness | Maturity |
|---|---|---|---|---|---|---|
| Naive XOR RBSR | O(d log n) | O(d log n) | **O(log n)** | No (self-adapting) | **Weak** (forgeable XOR) | Earthstar, Willow |
| **Secure-fingerprint RBSR (≥256-bit), fixed fan-out *b*** | O(d log n) | O(d log n) | O(log n) | No | Good | Negentropy (prod, *b*=16) |
| **↳ as instantiated in reconcile-rs** (`b`=16, swappable policy) | O(d log n) | O(d log n) | O(log_16 n) sequential — *8 one-way msgs at n=10⁶* | No | Good | reconcile-rs |
| IBLT / Difference Digest | O(d·(b+log U)) | **O(d)** | 1 (+estim.) | **Yes** | Weak | blockchains |
| **Rateless IBLT (SIGCOMM 2024)** | **≈ d** (3-4× < non-rateless) | **linear** (2-2000× < minisketch) | **1 streaming** | **No** | **Designed for adversarial** | Ethereum state-sync |
| minisketch / PinSketch (CPI) | **optimal ≈ b·d** | O(d²) | 1 (+ext.) | **Yes (capacity)** | deterministic if capacity OK | Bitcoin Erlay (BIP 330) |
| Merkle-tree diffing | O(d log n) | O(d log n) | O(log n) | No | hash-dependent | Dynamo, Cassandra, Riak |

Sources: Meyer arXiv:2212.13567 & logperiodic.com/rbsr.html; *Practical Rateless Set
Reconciliation*, SIGCOMM 2024, arXiv:2402.02668; minisketch (bitcoin-core) & BIP 330; Erlay
(CCS 2019); arXiv:2603.19820 (RSOS, 2026).

**The published O(d log n) / O(log n) figures assume the constant branching factor of Algorithm 2,
and this implementation uses one** — `rbsr`'s default `RefinementPolicy` is `FixedFanOut(16)`, so
the family's bounds describe it. The fan-out is a *local, swappable* choice rather than a wire
contract, and `rbsr` also ships `SqrtFanOut`, which cuts at `step = ⌊√m⌋`: the first SPLIT then
advertises ~√n ranges *whatever d is*, trading Θ(√n) communication for a Θ(log log n) recursion
depth. That is a different point of the same curve, not a different family; the two coincide on
round count below n ≈ 10¹², so the trade is one-sided in the reachable range. Both are measured in
`benches/protocol.rs`; see [§2.2](#22-competitors-at-the-reconciliation-algorithm-level) for the
numbers.

**Key takeaway:** for the **large-n / small-d / latency-sensitive** profile, fixed-*b* RBSR is the
**worst family on latency** (O(log n) sequential RTTs) whereas **Rateless IBLT** finds the diff in a
single streaming exchange, with no *d* estimation, and with adversarial robustness — it is the
**current SOTA choice** for this use case. reconcile-rs sits squarely in the fixed-*b* row and
therefore inherits that weakness; its alternative `√m` policy moves along the same curve (fewer
RTTs, heavier rounds) without escaping it, and measurably does not even buy the RTT saving below
n ≈ 10¹² (§2.2). Escaping the trade-off rather than moving along it is
[#185](https://github.com/Akvize/reconcile-rs/issues/185)'s job.

### 1.4 The SOTA of Merkle/anti-entropy structures

Important panel nuance: **FingerprintTreeMap does NOT belong to the Merkle Search Tree (MST) / prolly-tree
family**, and that is a point in its favor. MST (Auvolat & Taïani, SRDS 2019) and prolly-trees
(Dolt/Noms) *need* **insertion-order independence** because they diff by comparing the hashes of the
tree's **internal nodes**. FingerprintTreeMap, by contrast, diffs **value-defined ranges**: the cumulative
256-bit additive fingerprint (per-element BLAKE3, combined mod 2²⁵⁶) over `[a,b)` is identical on two
peers iff the *content* of the range is identical, **regardless of each one's B-tree shape**.
FingerprintTreeMap therefore obtains the convergence guarantee that MST/prolly pay for with
history-independence, **without paying for it** — and, since addition-with-carry is not GF(2)-linear
the way XOR is, also escapes the MST "leading-zeros" attack on firmer ground than a linear combiner
would. The B-tree's order-dependence is therefore **not** a defect here, and its history-dependence
(§2.1) is not a defect either — see the annotation there.

### 1.5 The SOTA of consistency and conflict resolution

- **Physical-clock LWW**: a documented anti-pattern (Jepsen/Kingsbury "The trouble with
  timestamps"; real NTP incidents). The "winner" is the node with the most-advanced clock, not the
  causally latest write → silent lost update.
- **Minimal SOTA fix**: **Hybrid Logical Clocks (HLC, Kulkarni 2014)** — 64-bit drop-in, monotonic,
  respects causality, divergence bounded by ε; adopted by CockroachDB and MongoDB.
- **Tie-break**: must be a **deterministic total order** (e.g. `(HLC, node_id)`). The current `keep
  local on equal` is non-convergent.
- **Tombstone GC**: the safe criterion is **causal stability** (acknowledgment by all replicas), not
  a wall-clock timer. Even Cassandra's `gc_grace_seconds` (default **10 days**) is safe *only on the
  condition* that a complete repair covers the window — i.e. no fixed duration, short or long, is
  sufficient on its own (ScyllaDB makes this explicit with repair-based GC). The pre-fix **60 s**
  wall-clock purge could not honor that precondition; GC is now gated on causal stability (F4/#109).

### 1.6 The embedded in-memory data grid (IMDG) use case

Framed as a product rather than an algorithm, reconcile-rs is an **embedded in-memory data grid**:
the state lives in-process, next to the application, fully replicated across a fleet of equal nodes.
Its category is the **masterless / AP / gossip** corner of the IMDG space — adjacent to Hazelcast,
Apache Ignite, Oracle Coherence and Infinispan (all JVM, all a separate cluster to operate), but as
a single embeddable Rust library. The pitch is "replicated state without standing up Redis/etcd":

- **Reads are local** — an in-process lookup, no network hop or (de)serialization. This is the one
  place reconcile-rs is unambiguously faster than a networked store; it is a *read-latency* and
  *operational-simplicity* play, not a write-path or consistency improvement.
- **Redundancy, not sharding** — full replication means any surviving node holds the whole dataset,
  so the grid tolerates losing nodes; the flip side is §1.2's memory / write-amplification ceiling.
- **Partition tolerance with automatic convergence** — nodes keep serving while partitioned and
  re-converge by anti-entropy on heal, with no manual conflict resolution (LWW).

Fit-for-purpose guidance (good fit / wrong tool) lives once, in README.md's "When to use this" —
not duplicated here. **Path to best-of-breed:** the open performance/scaling roadmap that moves
reconcile-rs from the "real but narrow" niche of §1.2 to a credible Rust IMDG is tracked in
[`PROGRESS.md`](./PROGRESS.md) §4 — its *SOTA axis index* is the per-axis view, one row per axis
below — not here.

---

## 2. Competitor audit and differentiators

> This section refocuses the analysis on the **FingerprintTreeMap as a data structure** (and its protocol),
> not on the full system. *(All structure/algo names below are defined in the
> [glossary §3.2](#g92).)* Methodological anchor: the FingerprintTreeMap **is not a [Merkle tree](#g92) in the
> [MST](#g92)/[prolly](#g92) sense**. It is a *[Range-Summarizable Order-Statistics Store](#g92)*
> (RSOS) — a B-tree augmented, per node, with a **composable subtree summary** (a 256-bit additive
> fingerprint)
> **+ an order statistic** (the subtree size). This abstraction was formalized in 2026
> (arXiv:2603.19820) as the backend that range-based reconciliation (RBSR, Meyer 2023) needs. Its
> **true peer group** = the other diffable structures; its **true algorithmic competitor** = the
> other set-reconciliation families.

### 2.1 Competitors at the "diffable data structure" level

#### Merkle Search Tree (MST) — Auvolat & Taïani, SRDS 2019
A search B-tree where a key's **level** is derived from the **hash of the key** (leading zeros →
fanout) ⇒ two replicas with the same key set produce the **same tree and same root hash**,
regardless of insertion order (*history-independence*). Diff = root-hash comparison (O(1)) then
descent comparing **internal node hashes**.
- ✅ History-independent (necessary because it diffs *nodes*); compact page serialization/diff;
  mature, **fuzz-tested** Rust crate (`merkle-search-tree`, domodwyer); production **Bluesky/atproto**
  (one MST per repository).
- ❌ **"Leading-zeros" attack**: an attacker forges keys with very deep hashes to inflate height and
  unbalance the tree. ❌ Only probabilistic balancing; no native rank/select.
- **vs FingerprintTreeMap:** MST *pays* for history-independence; FingerprintTreeMap does not (value-based diff, §2.3) and
  **escapes the leading-zeros attack**. But MST gains structural sharing (versioning) that FingerprintTreeMap
  lacks.

#### Prolly trees (Noms, Dolt) — *probabilistic B-trees*
A **content-addressed** B-tree, boundaries fixed by a **rolling-hash chunker** (~4 KB).
History-independent, self-balancing, and crucially **structural sharing**: unchanged subtrees share
identical chunks across versions.
- ✅ SOTA of **diffable AND versioned** ordered stores: diff/merge touch only changed chunks (the
  foundation of Dolt, "the first version-controlled relational database"). Dolt hashes **keys only**
  → a value update does not move boundaries. Resists the leading-zeros attack.
- ❌ Heavy machinery (rolling hash, chunks, CAS); higher latency than an in-mem B-tree; designed for
  **persistence**.
- **vs FingerprintTreeMap:** prolly = SOTA if you want **versioning + persistence + branch/merge**. FingerprintTreeMap is
  simpler/faster in memory but offers **none** of those. Central trade-off "simplicity/speed vs
  versioning/durability".

#### Merkle radix / Sparse Merkle Tree / "Merklized KV" (Gustafson 2023)
Position by the key's **prefix bits** (trie); history-independent by construction; the basis of
Ethereum (Merkle-Patricia) and SMTs.
- ✅ Deterministic, prefix scans, compact inclusion proofs.
- ❌ Depth ∝ key length (not log n); fixed fanout; less suited to arbitrary range diffs. Relevant
  mostly for **cryptographic proofs**, not for the "large in-memory KV, small diffs" profile.

#### Fixed-depth Merkle tree (Dynamo / Cassandra / Riak)
- ✅ Proven at massive production scale (anti-entropy repair).
- ❌ **Over-streaming**: a leaf covers a *range* of partitions (Cassandra: depth 15 = 32K leaves) →
  a single differing row forces streaming the whole leaf (~30 partitions for 1 bad in 1M). ❌ Tree
  rebuild when token ranges move.
- **vs FingerprintTreeMap:** this is precisely the defect RBSR/FingerprintTreeMap fix (the recursion tightens onto the
  actually-differing elements). **Clear advantage to FingerprintTreeMap** on this axis.

#### RSOS / AELMDB (arXiv:2603.19820, 2026) — *the most direct competitor*
The paper formalizes "**B+-tree augmented with subtree counts + composable summaries**" as the RSOS
abstraction, proves RBSR's local-cost bounds on this backend, and ships **AELMDB**: a **persistent,
memory-mapped** LMDB extension, evaluated with Negentropy. Read in the source
(`github.com/amparore/aelmdb`), the fork touches only **branch pages** — a branch node becomes
`[child pgno | aggregates | separator key]` with `aggregates := [entries?][keys?][hashsum?]` — and
binds the summary width into the on-disk format tag. It is **not** content-addressed: LMDB is a
copy-on-write B+-tree addressed by page number.
- **vs FingerprintTreeMap:** **it is the same design**, and its combiner is literally ours —
  addition modulo 2²⁵⁶ over little-endian 64-bit limbs with carry (`mdb_hashsum_add`), the C mirror
  of `Fingerprint::combine`. Two deltas run the *other* way, in our favour, and were not visible
  from the abstract alone:
  - **AELMDB does not hash.** Def. 3.4's lift φ is realized by *extracting a fixed-size byte slice*
    at a configured offset from the key or the value (`MDB_AGG_HASHSUM` + `mdb_set_hash_offset`);
    the engine assumes the application already embedded a collision-resistant id. `rsos` owns that
    end instead — BLAKE3 over the canonical encoding of (key, value) — so it summarizes the
    **value**, not merely an identity the caller vouches for.
  - **The comparison map is exact here, probabilistic there.** Negentropy's `f_p` is
    `SHA-256(Σ ‖ varint(count))` truncated to **128 bits**; the paper (§6.1) states plainly that
    this makes it "probabilistically sound rather than information-theoretically exact" and leaves
    the end-to-end collision analysis out of scope. `rbsr` compares the aggregate itself (full
    256-bit fingerprint + count), i.e. `f_p = id`, so Prop. 4.1's sound-skip assumption reduces to
    the injectivity of Σ with no truncation term. The price is 40 B/range against 16 B — see §2.2.
- The remaining delta the other way is **persistence**: AELMDB is LMDB-backed (memory-mapped,
  durable); FingerprintTreeMap is in-memory only. **The structure's SOTA in this niche = "persistent
  RSOS with a secure fingerprint" — persistence is the gap that remains.**
- **What the paper's evaluation does and does not establish.** Its headline (AELMDB 4.69×–13.98×
  faster than the `BTreeLMDB` baseline on reconciliation time) is scoped by §7.1 to single-machine,
  fixed-protocol, reconciliation-heavy workloads. Two readings the text does not foreground, both
  recomputed from the published `results-linux.csv`: the in-memory `Vector` backend is *faster than
  AELMDB in all six families* (0.39×–0.59×) despite not being an RSOS at all (its `fingerprint()`
  scans the range, O(k) not O(log n)); and the 13.98× family runs at **d ≈ 21 % of n** with 1–3
  protocol messages — the opposite of RBSR's large-n/small-d target, a regime where the cost is
  dominated by enumeration and point access rather than range aggregation. The result to carry
  forward is therefore *"an aggregate-augmented persistent engine costs ~2× an in-RAM array on the
  hot path"*, which is the number that was missing from the persistence decision (see
  [`PROGRESS.md`](./PROGRESS.md) §4 on #271), not *"in-tree aggregates beat memory"*.

| Structure | Position/boundary | History-indep. | Diffs… | Structural sharing / versioning | Persistence | Resists leading-zeros | Maturity |
|---|---|---|---|---|---|---|---|
| **FingerprintTreeMap** | B-tree splits (insertion order) | **No** | **value ranges** | No | No (in-mem) | **Yes** (n/a) | pre-alpha |
| MST | level = hash(key) | Yes | nodes | partial | impl-dependent | **No** | mature (Bluesky) |
| Prolly tree | rolling-hash on content | Yes | chunks | **Yes** (CAS) | **Yes** | Yes | mature (Dolt) |
| Merkle radix/SMT | prefix bits | Yes | hash paths | partial | yes | Yes | mature (Ethereum) |
| Fixed-depth Merkle | token range | partial (rebuild) | nodes | no | yes | yes | mature (Cassandra) |
| **RSOS/AELMDB** | augmented B+-tree | not required | ranges | no | **Yes** (LMDB) | yes | research 2026 |

FingerprintTreeMap's **No** on history-independence is **not a weakness**: it is the RSOS family's whole point
(§2.3 #1). MST/prolly *need* history-independence because they diff internal-node hashes; FingerprintTreeMap
diffs value-defined ranges instead and never compares a node hash, so two peers with different tree
shapes still converge.

### 2.2 Competitors at the "reconciliation algorithm" level

The FingerprintTreeMap implements **RBSR**; its competitors are not tree structures.

| Family | Communication | Compute | RTT | Knows *d*? | Adversarial robustness | Maturity |
|---|---|---|---|---|---|---|
| **RBSR, fixed fan-out *b*** (secure fingerprint) | O(d log n) | O(d log n) | **O(log n) sequential** | No (self-adapting) | **Good** | Earthstar/Willow (naive XOR) — Negentropy (secure, *b*=16) |
| **↳ reconcile-rs, default policy** (`b`=16) | O(d log n) | O(d log n) | O(log_16 n) sequential | No | **Good** | reconcile-rs |
| **↳ reconcile-rs, `SqrtFanOut` policy** (fan-out `√m`) | **Θ(√n)**, d-independent | O(d log n) | Θ(log log n) sequential — *equal to `b`=16 below n≈10¹²* | No (self-adapting) | **Good** | reconcile-rs |
| **Rateless IBLT** (SIGCOMM 2024) | **≈ d** (3-4× < non-rateless) | **linear** (2-2000× < minisketch) | **1 streaming exchange** | **No** | **designed for adversarial** | Ethereum state-sync |
| minisketch/PinSketch (CPI) | **optimal ≈ b·d** | O(d²) | 1 (+ext.) | **Yes (capacity)** | deterministic if capacity | Bitcoin Erlay (BIP 330) |
| CertainSync (2025) | bound f(d,U) | linear | rateless | No | **deterministic success** | SIGMETRICS research |
| Classic IBLT | O(d·(b+log U)) | O(d) | 1 (+estim.) | **Yes** | weak | blockchains |

**Critical reading (stated profile: large n, small d, latency-sensitive, P2P):**
- Fixed-*b* RBSR is the **worst family on latency**: O(log n) **sequential RTTs** to isolate a
  difference. On a 1 ms-RTT LAN that's several ms; on WAN far more — a cost the README's loopback
  benchmarks hide (cf. F16).
- **A size-derived fan-out sits off that point of the curve, and `rbsr` ships one.** `SqrtFanOut`
  cuts at `step = ⌊√m⌋`, so a range of *m* elements is replaced by ~√m children of ~√m elements
  each. Repeated square-rooting bottoms out in **Θ(log log n)** rounds, not O(log n) — but the first
  SPLIT emits ~√n range aggregates **regardless of d**, so communication is **Θ(√n)**, not
  O(d log n). This is not a change of logarithmic base; it is a different complexity class in both
  columns, one better and one worse. It is not the default; the numbers below are why.

  `benches/protocol.rs` measures **both columns against a fixed *b* in the same harness** (`u64`
  keys, d = 1, one element missing):

  | n | `√m` bytes | `√m` msgs | `b`=16 bytes | `b`=16 msgs | `t`=32/`b`=16 bytes | msgs |
  |---:|---:|---:|---:|---:|---:|---:|
  | 10³ | 2 041 | 6 | 1 701 | 6 | 1 476 | 4 |
  | 10⁴ | 5 395 | 8 | 2 195 | 6 | 1 520 | 4 |
  | 10⁵ | 16 553 | 6 | 2 789 | 6 | 2 294 | 5 |
  | 10⁶ | **53 046** | 8 | **3 834** | 8 | **3 246** | 6 |

  ~×3.2 per decade of *n* on the `√m` bytes (i.e. √10). Concretely: **one dropped UDP update in a
  1 M-entry map costs ~53 kB on the next anti-entropy round**, against 3.8 kB for a fixed *b* = 16 —
  and ~13× the local `Aggregate`/`Rank`/`Select` queries with it (2 094/2 092/1 040 against
  155/152/70).

  The paper's local cost *T_loc* is the widest gap, and it is pure CPU, so no RTT caveat touches it:
  the timed two-peer drive at d = 1 runs **2.10 ms under `√m` against 45.0 µs at *b* = 16 (≈47×)** at
  n = 10⁶, 460 µs against 25.2 µs at 10⁵, and only 1.6× apart at 10³. Steeper than the query-count
  ratio because a `√n` fan-out's queries are individually dearer — ~1 000 `Select`s at spread-out
  ranks and ~1 000 wide `Aggregate`s per round touch far more of the tree than a narrow descent.

  **The compensation is not observable in the reachable range.** Θ(log log n) beats Θ(log_16 n)
  asymptotically, but at n = 10⁶ the iterated square root bottoms out in ~4 levels and log₁₆ 10⁶ ≈ 5;
  the measured message counts are *identical* (8 and 8). The separation only reaches a factor of two
  around n ≈ 10¹², far past what a fully-replicated in-memory store holds. So the `√m` rule pays the
  Θ(√n) bytes and buys nothing back at any size this crate targets.

  Two qualifications keep it from being a one-line verdict. First, every benchmark here runs at
  RTT ≈ 0 ([#280](https://github.com/Akvize/reconcile-rs/issues/280)), so the message column is a
  *count*: equal counts do mean equal round-trips, but a policy losing on that column could not be
  priced in seconds. Second, the gap closes as *d* grows and scatters — at d = 100 over 10⁶ elements
  the two are within 7 % (270 940 B against 253 153 B), because ~√n ranges stop being overhead once
  the difference genuinely needs that many. `√m` is worst exactly in the small-*d* regime RBSR
  exists for.

  The widest single round at d = 1 is 50 781 B — inside the 65 507-byte datagram ceiling, but ~35 IP
  fragments at a 1500-byte MTU, any one of which loses the whole round. At d = 100 over 10⁶ elements
  it reaches **160 908 B over 3 300 ranges**, i.e. three datagrams: `send_messages_paced` chunks past
  the ceiling rather than failing, so this degrades into extra datagrams and ~189 fragments rather
  than breaking. It is a bandwidth and fragmentation cost, not a bug — but it is the reason #257 is a
  communication-complexity regression rather than a tuning gap.
- **Sweeping *b* itself lands on 16.** `benches/protocol.rs`'s `fan_out_sweep` runs *b* = 2…256.
  Bytes and local work follow *b*/ln *b* (minimum near *b* = 3: 1 960 B at *b* = 4 against 3 834 B at
  16, n = 10⁶, d = 1); one-way messages fall as log_*b* n to a floor of 6, reached at *b* = 32.
  *b* = 16 is the only swept value **never worse than `√m` on rounds** across every
  measured (n, d, clustering), while spending 13.8× fewer bytes, ~45× less *T_loc* and a 63×
  narrower widest round.
  *b* = 4 wins on bytes and CPU but costs two round-trips — break-even at an RTT of ≈8 µs at 1 Gb/s,
  i.e. only worth it when the "network" is in-process. Because the policy never crosses the wire and
  mixed pairs converge, changing *b* is a per-node behaviour choice, not a cluster-wide format
  decision.
- **The arity question has a forty-year analytical treatment, in a literature neither RBSR paper
  cites** ([§4.1](#41-cross-community-vocabulary), [§4.4](#44-bibliography)). Random-access *tree
  algorithms* solve "split a population into `q` groups when the location of the conflicts is
  unknown, recurse on the conflicted groups" — structurally the refinement loop, with collision
  feedback where this protocol has fingerprint inequality.

  | | Result | Bears on |
  |---|---|---|
  | Mathys & Flajolet 1985 | `Q` ∈ {2, 3} preferred; throughput for `Q` > 3 "quickly degrades" | the measured `b`/ln `b` minimum at `b` = 3–4 lands in the same place |
  | Vogel et al. 2024 | **disproves** binary-optimality: maximal throughput is reachable at *any* `d` ≥ 2 **given suitable splitting probabilities** | the binding axis may be the split *distribution*, not the arity |

  Two cautions before either is quoted as support. The objectives differ — channel throughput
  against wire bytes plus `T_loc` — so the coinciding optimum is a **convergence to investigate, not
  a transferred theorem**; and this crate's binding constraint, the widest-round datagram/MTU
  ceiling, has no counterpart in the channel model. What does transfer cleanly is the *question*
  Vogel et al. reopen: Algorithm 2 and `FixedFanOut` both cut a **balanced** partition by equal rank
  (Def. 3.8), and nothing in RBSR requires that.
  [#318](https://github.com/Akvize/reconcile-rs/issues/318)'s uneven, signal-driven split therefore
  has adjacent analytical support it was not claiming.
- **arXiv:2603.19820 §8 names this gap as open**, in the vocabulary `benches/protocol.rs` measures:

  > it remains important to evaluate the RSOS view across other storage engines and update regimes,
  > and **to understand more systematically how split policies, branching factors, and enumeration
  > thresholds interact with the underlying index**

  The same paragraph asks for "instance-sensitive bounds for local work that depend not only on
  `|∆(X,Y)|` but also on the **ordered shape of the mismatches**" — the `Clustering` axis that target
  already sweeps. Two of the paper's stated open directions are instrumented here.
- **A benchmarking framework for set reconciliation already exists, and excludes RBSR.** GenSync
  (IEEE TNSM 2022) ships FullSync / CPISync / IBLTSync behind one interface with a cgroup-based
  lane injecting latency, bandwidth, loss and CPU budget, and reports **no universally dominant
  protocol** with the winner sensitive to network parameters. Two consequences: any harness claim
  here scopes to refinement policies *inside* RBSR, which GenSync does not carry; and its injection
  lane is prior art for [#280](https://github.com/Akvize/reconcile-rs/issues/280) — evaluate before
  building.
- **Every cost model on this page is two-party; the system is N-party.** RBSR, PSR, CPI and RIBLT
  all state their bounds for one pair of peers, while `ReplicatedMap` runs a gossip cluster whose
  per-write amplification is O(N) (§1.2, `benches/system.rs::gossip_fanout`). Multi-party set
  reconciliation has its own literature (Mitzenmacher & Pagh, *Distributed Computing* 2018) that no
  RBSR work cites. Unexamined axis, not a known-good one.
- **The `t` column is not free, and the byte table above does not show its price.** The paper's
  enumeration threshold wins the refinement column by *stopping early* — and everything it stops on
  is then shipped as values, almost all of which the peer already holds. At n = 10⁵, d = 100
  scattered, `t`=32/`b`=16 advertises 88 817 B of refinement against `√m`'s 147 726 B, but enumerates
  **5 036 elements against 100**: a 50× value amplification hidden behind a 40 % refinement saving.
  This is why `benches/protocol.rs` reports IDLIST elements alongside advertised ranges, and why a
  policy comparison that reports only wire ranges would pick the wrong default.
- **The wire aggregate compounds it.** `RangeAggregate` carries a full 256-bit `Fingerprint` plus a
  `usize` count — 40 B per advertised range, against Negentropy's 16 B truncated comparison value
  (§2.1). That is the right trade in isolation (see the `f_p` note in §2.1), but at √n ranges per
  round it is ~79 % of the bytes measured above. The two decisions are only separable if the
  fan-out shrinks — which, now that the fan-out is a swappable policy, is a one-line change to a
  caller rather than an edit to the protocol loop.
- **Rateless IBLT** finds the diff in a **single streaming exchange**, without estimating *d*, with
  explicit adversarial robustness and linear compute → **single-shot SOTA choice** for this use case.
- **But** RBSR keeps two assets that sketches lack: **self-adapting** (no *d* estimation, no failure
  if *d* is mis-guessed) and **ordered-range reconciliation** (partial sync by prefix/subspace —
  what Willow exploits in 3D). Sketches reconcile an *opaque* set.
- **Conclusion:** a **hybrid** SOTA design — RBSR to localize coarsely + a sketch (Rateless IBLT) to
  drain the still-unresolved leaf ranges in one shot — would beat pure FingerprintTreeMap on latency without losing
  adaptiveness.

### 2.3 Real differentiators of the approach (structural strengths)

1. **Value-based range diff ⇒ history-independence is not needed** *(the deepest differentiator)*.
   MST/prolly *must* be history-independent because they compare **internal node hashes** (different
   tree shapes → false positives). FingerprintTreeMap never compares nodes: it computes the **cumulative
   256-bit additive fingerprint over `[a,b)`**, identical on two peers **iff the range content is
   identical**, regardless of each one's B-tree shape. → Convergence guaranteed **without paying**
   for history-independence, and **immunity to the MST leading-zeros attack** (addition-with-carry is
   not GF(2)-linear, unlike XOR).

   **The strongest counter-argument on record**, and the reason this is a *differentiator* rather
   than a free lunch: Meyer & Scherer (2024) show RBSR can be realized with **conventional
   (non-homomorphic) hashes** over history-independent, clamping-invariant search trees. That is a
   different point on the same design plane — it pays for history-independence but then owes
   nothing to a composable-monoid summary. Two consequences worth holding: the additive combiner is
   a *choice*, not a requirement of RBSR; and the theoretical half of
   [#298](https://github.com/Akvize/reconcile-rs/issues/298)'s motivation (generalize to
   `RSOS<M: Monoid>`) weakens accordingly — its wire-format half (`RangeAggregate` is the wire type,
   so the generalization is not additive later) stands on its own and is the half that fixes the
   timing.
2. **It is a SOTA-2026-conformant RSOS**: the `tree_hash` cache (composable summary) + `tree_size`
   (order statistic) → range-summary and rank/select queries in **O(log n)** (the arXiv:2603.19820
   contract). Core *aligned* with the most recent theory.
3. **Cheap incremental maintenance**: `tree_hash ^= diff_hash` + `tree_size += 1` propagated along
   the single root→leaf path → O(log n) amortized. The 2-3× factor vs `BTreeMap` is the *expected*
   price of these two invariants, not an anomaly.
4. **A single structure stores AND reconciles**: no separate Merkle tree to maintain (contrast
   Cassandra which builds the tree at repair time). The store *is* the reconciliation index.
5. **Avoids Cassandra's over-streaming**: the SPLIT recursion tightens onto the ranges that
   actually differ instead of streaming a whole fixed partition. (The split fan-out here is
   `√m` by rank, not a fixed branching factor — arXiv:2603.19820's Algorithm 2 is stated for a
   fixed `b`, and Negentropy's default is `b = 16`; neither is what this implementation does. That
   deviation is no longer only a note: §2.2 quantifies what it costs, and it is the one place where
   this implementation is *worse* than the family it belongs to.)
6. **Rust-native, in-process, embeddable**: a real ecosystem niche (mature equivalents = JVM).

### 2.4 The design axes of a *true* SOTA RSOS

These are the axes along which an RSOS is judged against the state of the art — the **design
target** for a structure of this family ("persistent RSOS with a secure, generic fingerprint").
They are described here as durable design goals; which of them `reconcile-rs` has since addressed
is tracked in [`PROGRESS.md`](./PROGRESS.md) §4's *SOTA axis index*, one row per axis below (the
`Fxx` pointers map to its §2 table).

**P0 — Correctness of the structure itself:**
1. **Secure and wide fingerprint**: replace the 64-bit XOR with a **≥256-bit, non-GF(2)-linear**
   combiner (hash-then-add mod 2²⁵⁶, MSet-Mu-Hash/LtHash) or *keyed*. XOR = self-inverse + linear →
   craftable collisions (Gaussian elimination ~2 s even in 256-bit) + birthday at 2³². The path
   taken by Negentropy. **This is THE criterion that separates a "toy" structure from a SOTA one.**
   (cf. F6)
2. **Decouple "empty" from "hash==0"** (`size==0`) — otherwise the structure can claim "converged"
   while having lost data. (cf. F1)
3. **Stable, versioned hash as a wire contract** (pinned SipHash/xxHash/BLAKE3 + golden-vector).
   (cf. F8)

**P1 — Generality (what makes it a *structure*, not a special case):**
4. **Generic summary over a monoid**: today `rsos` hardwires its range summary to the 256-bit additive
   `Fingerprint` (`ARCHITECTURE.md` §7, tracked as `BYOLiftingMonoid`); generalizing to `RSOS<M: Monoid>`
   also enables sum/min/max/count and sketches. Enables **embedding a sketch in the leaves** (hybrid
   RBSR + Rateless IBLT) to break the O(log n) RTT cost (§2.2).
5. **Fully expose the RSOS contract** — ✅ **done**: `rank`/`select`/`range` are `pub` on the standalone
   `rsos` crate's `FingerprintTreeMap` (ARCHITECTURE.md §3.2), a reusable generic building block
   independent of `reconcile`. (Previously in tension with an earlier ARCHITECTURE.md draft that kept
   these `pub(crate)` inside the monolithic crate; resolved in favor of exposure once `rsos` became its
   own published-intent crate.) Remaining: lazy + double-ended iterators (repo issue #92 — the
   umbrella that consolidated #89–#91).

**P2 — Durability & distributed properties carried by the structure:**
6. **Persistence / content-addressing** *(the big gap vs prolly/AELMDB)*: (a) snapshot+WAL including
   tombstones, or (b) the true SOTA step — **node content-addressing** for *structural sharing*
   (versioning, diff between snapshots, incremental cold start).
7. **Conflict metadata in the value**: HLC + total tie-break `(timestamp, node_id)`; ideally
   **pluggable CRDT** values; versioned tombstones with **causal-stability GC**. (cf. F4, F5)

**P3 — What makes it *believed* to be SOTA:**
8. **Property-testing + fuzzing as a foundation**: `proptest` vs `BTreeMap` oracle +
   `check_invariants`, and especially **the convergence property** (two random trees → diff loop →
   identical state + ranges = true symmetric difference, under reordered/duplicated/dropped
   messages). The category standard (`merkle-search-tree` is fuzz-tested). (cf. F11)
9. **First-class adversarial robustness**: segment-bound validation, allocation bounds, bounded
   fan-out — to hold up against hostile peers (the MST/Willow use case).

**SOTA target by axis:**

| Axis | SOTA target |
|---|---|
| Summary | ≥256-bit non-linear/keyed, **generic (monoid)** |
| Empty vs hash | emptiness/equality decided on `size`, never on the fingerprint |
| Hash | fixed, versioned hash as a wire contract |
| Backend | **persistent RSOS** (AELMDB-style), ideally content-addressed |
| Algo | **hybrid RBSR + Rateless IBLT** for single-shot latency |
| Conflicts | HLC + deterministic total tie-break / pluggable CRDT |
| Deletions | causal-stability GC (no resurrection) |
| Confidence | property tests + convergence fuzzing against an oracle |

**In one sentence:** the FingerprintTreeMap starts from the **right skeleton** — an RSOS, the design validated by
2026 research, with a real differentiator (value-based diff that removes the need for
history-independence). The remaining distance to a *true* SOTA structure is along the axes above; the
structural ones (secure/generic fingerprint, persistence/content-addressing, property-testing
foundation) belong to the structure itself, while conflicts, GC and robustness belong to the
surrounding system.

---

## 3. Glossary

> Lists **(a)** the competing structures and algorithms cited, **(b)** the acronyms and concepts of
> distributed systems, cryptography, networking and complexity, and **(c)** the Rust tooling — all
> **implementation-agnostic**. The repository's own identifiers, types and constants are intentionally
> not catalogued here (see [§3.1](#g91)). `Fxx` references denote the original audit findings, whose
> current status lives in [`PROGRESS.md`](./PROGRESS.md).

<a id="g91"></a>
### 3.1 — Repository identifiers

> **Implementation-agnostic by design.** This positioning document does not catalogue the
> repository's own types, methods and constants. For the code surface, see the crate's API
> documentation (`cargo doc`); for the module map and the target design, see
> [`ARCHITECTURE.md`](./ARCHITECTURE.md) (§2.1); for the audit findings and their current status,
> see [`PROGRESS.md`](./PROGRESS.md). The subsections below define the **field-agnostic** concepts.

<a id="g92"></a>
### 3.2 — Competing data structures and algorithms

| Term | Definition |
|---|---|
| **RBSR** (*Range-Based Set Reconciliation*) | Algorithm family (Meyer 2023): the peers maintain a family of pairwise disjoint **active ranges**, initially one **outer range**; each **protocol round**, a peer answers every active range it was asked about with **SKIP** (aggregates match → *resolved*), **IDLIST** (send the range's ordered contents outright) or **SPLIT** (replace it by a balanced family of **child ranges**). The result is the **symmetric difference** Δ(X, Y). Vocabulary and Algorithm 1 as formalized in arXiv:2603.19820 §4. What reconcile-rs implements — with a `√m` rather than fixed-`b` fan-out, which changes both cost columns (§1.3, §2.2). O(log n) RTT at fixed `b`. |
| **RSOS** (*Range-Summarizable Order-Statistics Store*) | Abstraction (arXiv:2603.19820, 2026): an ordered set offering **composable** range summaries + rank/select navigation. An augmented B+-tree realizes it → **the FingerprintTreeMap is an RSOS**. |
| **AELMDB** | **Persistent** RSOS implementation (LMDB extension, memory-mapped) from the 2026 paper, evaluated with Negentropy. The most direct competitor to the FingerprintTreeMap. Aggregates live in **branch pages** (`[child pgno \| aggregates \| separator key]`); the element summary is a byte slice *extracted* from the record, never hashed by the engine. Not content-addressed. |
| **LMDB** | Lightning Memory-Mapped Database: a copy-on-write, memory-mapped B+-tree with lock-free MVCC readers and a single writer. AELMDB's host engine, and the closest existing thing to what #271 proposes to build. |
| **Counted B-tree** | (Tatham, 2004) A B-tree carrying per-subtree element counts, giving O(log n) rank/select. The order-statistic half of RSOS, as a standalone classic. |
| **AB-tree** | (Zhao et al., VLDB 2022) A page-oriented tree maintaining aggregate metadata **under concurrent updates**; evidence that aggregate augmentation and concurrency compose, and the reference point for root-aggregate write contention. |
| **Embedded Merkle B-tree (EMB-tree)** | (Li et al., SIGMOD 2006) A B-tree caching digests in its nodes for **authenticated query answering** over outsourced databases. Prior art for "digests inside a B-tree", with a different goal (proofs, not range aggregation). |
| **PBS** (*Parity Bitmap Sketch*) | (Gong et al., VLDB 2020) A sketch-based reconciliation scheme targeting low computation together with near-optimal communication — another point on the RIBLT/minisketch Pareto front. |
| **MST** (*Merkle Search Tree*) | Auvolat & Taïani, SRDS 2019. A B-tree whose key level derives from the **hash of the key** ⇒ history-independent. Diffs **nodes**. Vulnerable to the leading-zeros attack. Usage: Bluesky/atproto. |
| **Prolly tree** (*probabilistic B-tree*) | Noms/Dolt. Content-addressed B-tree, boundaries by **rolling hash**. History-independent + **structural sharing** → versioning (Git-like). SOTA of versioned ordered stores. |
| **Merkle radix / Patricia trie** | A Merkle tree where position depends on the key's **prefix bits**. History-independent. The basis of Ethereum. |
| **SMT** (*Sparse Merkle Tree*) | Merkle tree over a huge, mostly-empty key space; compact inclusion/exclusion proofs. |
| **Merkle tree / Merkle root** | Hash tree where each node hashes its children; the root summarizes everything. Basis of classic anti-entropy. |
| **Merkle-DAG / Merkle-CRDT** | Content-addressed, hash-linked DAG (IPFS); the links encode causal history (Merkle-CRDT, arXiv:2004.00107). |
| **IBLT** (*Invertible Bloom Lookup Table*) | A structure encoding a set into cells (XOR of key/hash + counter); subtracting two IBLTs reveals the symmetric difference via "peeling". Comm. ∝ d, **needs d known**. |
| **Rateless IBLT (RIBLT)** | *Practical Rateless Set Reconciliation*, SIGCOMM 2024. An infinite stream of coded symbols (fountain code); decodes as soon as ~d symbols are received. **No need for d**, linear compute, adversarially robust. **Single-shot SOTA choice.** |
| **minisketch / PinSketch** | Bitcoin Core library implementing PinSketch (BCH formulation of reconciliation). Comm. **optimal ≈ b·d**, O(d²) decoding, capacity to predefine. |
| **CPI / CPISync** (*Characteristic Polynomial Interpolation*) | Encodes the set as the roots of a polynomial; the ratio of the polynomials yields the difference. Minsky-Trachtenberg-Zippel. O(d³) decoding. |
| **BCH codes / Berlekamp-Massey** | Error-correcting codes / decoding algorithm used by PinSketch to reconstruct the characteristic polynomial. |
| **Strata Estimator** | A stack of sampled IBLTs estimating the difference size *d* without a prior round (Eppstein et al. 2011). |
| **CertainSync** | arXiv:2504.08314 (SIGMETRICS 2025): rateless reconciliation with **deterministic success** (no estimator or parametrization). |
| **Bloom filter** | Probabilistic membership filter (false positives, no false negatives); a Graphene component. |
| **Erlay / Graphene / BIP 330** | Bitcoin deployments: Erlay (minisketch + flooding, specified in BIP 330), Graphene (Bloom + IBLT). |
| **Negentropy** | Production RBSR implementation (Nostr/NIP-77, strfry relay). **Abandoned the naive XOR combiner** for an incremental cryptographic hash — directly relevant to F6. |
| **Willow / Earthstar / iroh / iroh-docs** | Decentralized-sync ecosystem: Willow (3D RBSR, whose spec documents XOR-fingerprint insecurity), iroh (encrypted QUIC + `iroh-docs` = persistent CRDT KV — a direct Rust competitor). |
| **Dynamo / Cassandra / ScyllaDB / Riak / Voldemort** | Distributed databases with Merkle-tree anti-entropy. Cassandra: `gc_grace_seconds`, over-streaming. ScyllaDB: repair-based tombstone GC. Reference for F4. |
| **Noms / Dolt / DoltHub** | Prolly-tree ecosystem; Dolt = "the first version-controlled relational database". |
| **content-defined chunking (CDC) / rolling hash** | Placing node boundaries where a rolling hash over the content matches a target pattern (core of prolly-trees). |
| **structural sharing / CAS / CID** | Sharing unchanged substructures across versions; *Content-Addressed Storage*; *Content IDentifier* (hash used as an address). |

<a id="g93"></a>
### 3.3 — Consistency, replication and distributed systems

| Term | Definition |
|---|---|
| **LWW** (*Last-Write-Wins*) | Conflict resolution: the value with the largest timestamp wins. Wired here on a physical clock (F5). |
| **Thomas write rule** | The rule formalizing LWW: ignore a write older than an already-applied state. |
| **eventual consistency** | Weak guarantee: with no new writes, replicas converge *eventually*. |
| **SEC** (*Strong Eventual Consistency*) | *Strong* convergence: replicas that received the same updates have identical state, **regardless of order**. Requires a commutative/associative/idempotent merge. Not reached here (F5). |
| **CRDT** (*Conflict-free Replicated Data Type*) | A type whose merge guarantees SEC. **CvRDT** (state, merge = least upper bound of a lattice) vs **CmRDT** (commutative operations). Shapiro et al. 2011. |
| **LWW-Register / MV-Register / OR-Set** | Classic CRDTs: LWW register (lossy), multi-value register (keeps concurrent values), Observed-Remove Set (add-wins). |
| **join-semilattice** | A lattice where any pair has a least upper bound; the mathematical structure underlying CvRDTs. |
| **commutative / associative / idempotent / monotone** | Properties required of a CRDT merge. reconcile-rs's merge is **not commutative** on equal timestamps (F5). |
| **Lamport clock** | A scalar logical clock respecting *happens-before*; does not detect concurrency. |
| **vector clock / version vector** | A vector of one counter per node; **detects** concurrency (incomparable vectors). O(N) cost, delicate pruning. |
| **DVV** (*Dotted Version Vector*) | A refined version vector (Preguiça et al.): O(1) causality, metadata bounded by the replication degree. Adopted by Riak. |
| **HLC** (*Hybrid Logical Clock*) | Kulkarni 2014. 64-bit timestamp = physical + logical counter: monotonic, respects causality, bounded divergence. **Recommended minimal fix** for F5 (CockroachDB, MongoDB). |
| **TrueTime / commit-wait** | Spanner approach: bounded clock-uncertainty interval (GPS+atomic) + commit wait → external consistency/linearizability. |
| **happens-before / causality** | Partial order of events (Lamport 1978). A causally later write must not be overwritten by the one it derives from. |
| **causal consistency / causal+** | Consistency respecting *happens-before*; *causal+* (COPS) = causal + convergent conflict resolution. |
| **causal stability** | A safe-GC condition: an event is purgeable only when **no concurrent operation can still arrive** (all replicas have seen it). The basis of the F4 fix. |
| **session guarantees** | (Bayou) Read-Your-Writes, Monotonic Reads, Monotonic Writes, Writes-Follow-Reads. None provided by multi-master physical LWW. |
| **resurrection / zombie** | Reappearance of deleted data when a tombstone is purged before all have seen it (F4). |
| **`gc_grace_seconds`** | Cassandra window before purging a tombstone (default **10 days**), safe *only if* a complete repair covers it — a heuristic, not a guarantee. The pre-#109 design here used a **60 s** wall-clock purge (F4); GC is now gated on causal stability instead. |
| **CAP / PACELC** | CAP: under a Partition, choose Consistency or Availability. PACELC: *Else* (normal operation), choose Latency or Consistency. reconcile-rs is **PA/EL**. |
| **clock skew / NTP / PTP** | Drift between physical clocks; synchronization protocols (NTP ~sub-second, PTP more precise). Cause of LWW losses (F5). |
| **quorum / read repair / hinted handoff** | Dynamo-like mechanisms (absent here): majority of replicas, repair at read time, buffer for an unreachable peer. |
| **split-brain / partition** | A cluster split into sub-groups that no longer communicate; each diverges. |
| **anti-entropy (push / pull)** | Periodic pairwise reconciliation. Push = push hot updates; pull = query a peer. Demers et al. 1987. |
| **gossip / epidemic / rumor mongering** | Epidemic dissemination of updates to random peers. |
| **SWIM / HyParView / memberlist / Vivaldi** | **Membership** and failure-detection protocols (≠ data sync). SWIM/`memberlist` (HashiCorp): bounded fan-out, log N convergence — recommended for F10. |

<a id="g94"></a>
### 3.4 — Cryptography, hashing and networking

| Term | Definition |
|---|---|
| **XOR** | Exclusive-OR. Commutative, associative, **self-inverse**, GF(2)-linear. Convenient for range subtraction but weak as a fingerprint (F6). |
| **GF(2)-linear** | Linear over the two-element field → an attacker *solves* (Gaussian elimination) for collision elements instead of brute-forcing them (F6). |
| **collision / second-preimage / birthday bound** | Two inputs → same hash; finding a 2nd input colliding given data; probabilistic collision threshold (~2^(b/2), i.e. ~2³² for 64-bit). All relevant to F6. |
| **SipHash** | A fast keyed PRF, 64-bit output; the `DefaultHasher` algorithm. **Not** collision-resistant in the cryptographic sense. |
| **`DefaultHasher`** | The std hasher (`std::collections::hash_map`), **not stable** across Rust versions/platforms → cross-version non-convergence (F8). |
| **BLAKE3 / xxHash** | Fast and **stable** hashes recommended as replacements (F8). |
| **incremental / homomorphic hash** | A set hash updated incrementally and composable. **MSet-XOR-Hash** (weak, self-inverse and GF(2)-linear), **MSet-Mu-Hash** (finite field), **LtHash** (lattice/vector addition, closest in spirit to reconcile-rs's hash-then-add-mod-2²⁵⁶ combiner) — the F6 fix moved off MSet-XOR-Hash onto this family. |
| **transitive group** | The minimal algebraic structure required of an RBSR fingerprint (associativity, identity, inverses, transitivity) — XOR satisfies it, hence its convenience *and* its fragility. |
| **MAC / HMAC / AEAD** | Message Authentication Code; HMAC (hash-based); Authenticated Encryption with Associated Data. The F3 fix. |
| **TLS / DTLS / Noise / QUIC** | Secure transport layers (DTLS = TLS over datagrams; Noise = a handshake framework; QUIC = encrypted transport over UDP). Options for F3; cf. issue #96. |
| **spoofing / amplification / reflection / DRDoS** | Forging the source IP (trivial in UDP); a response larger than the request toward a victim; distributed reflection denial of service. The F9 surface. |
| **bincode allocation bomb** | Deserialization where an attacker-controlled length prefix forces a massive pre-allocation (F18). |
| **UDP / datagram / MTU** | Connectionless, unreliable protocol with a spoofable source; bounded datagram; *Maximum Transmission Unit*. |

<a id="g95"></a>
### 3.5 — Complexity, theory and notation

| Term | Definition |
|---|---|
| **B-tree / B+-tree** | A balanced multi-way search tree. B+-tree: values only in the leaves. |
| **order statistics (rank / select)** | "Rank of a key" / "key at rank i" operations in O(log n) thanks to subtree counters (`tree_size`). |
| **monoid** | A set with an associative operation and an identity element; the ideal structure of a generic composable summary (P1 of §2.4). |
| **fan-out** | Number of sub-ranges per recursion round; trades RTT vs message size. |
| **n / d / U / b** | SOTA notation: set size *n*, symmetric-difference size *d*, key universe *U*, element bit-width *b*. |
| **O(log n) / O(d log n)** | Target costs: hash-range query and per-mutation operations in O(log n); diff message volume in O(d log n). |

<a id="g96"></a>
### 3.6 — Rust tooling and ecosystem

| Term | Definition |
|---|---|
| **MSRV** (*Minimum Supported Rust Version*) | The minimum supported Rust version; absent from `Cargo.toml` (F17). |
| **clippy / `-Dwarnings`** | The Rust linter; CI treating warnings as errors. The `mismatched_lifetime_syntaxes` warning (`fingerprint_tree_map_iter.rs:177`) would break CI (F17). |
| **miri** | An interpreter detecting UB (*Undefined Behavior*); not applicable here — the crate is `#![forbid(unsafe_code)]` and all iterators are safe Rust (since `d030c15`). The CI gap for F17 is now the undeclared MSRV ([#189](https://github.com/Akvize/reconcile-rs/issues/189)). |
| **proptest / quickcheck / fuzzing** | Property-based / generative / random-input testing. **Entirely absent** (F11). |
| **`cargo audit` / `cargo deny`** | Vulnerability audit / dependency policies. Absent from CI (F19). |
| **bincode / serde / tokio / parking_lot / arrayvec / ipnet / range-cmp / chrono / rand / once_cell / tracing** | Dependencies: binary serialization; (de)serialization; async runtime; non-poisoning locks; `ArrayVec` (inline vector, B-tree nodes); network/CIDR types; key↔range comparison (`RangeOrdering`); `DateTime<Utc>` (LWW timestamps); randomness; lazy init; structured logs. |
| **`Arc` / `RwLock` / `unwrap` / `panic=abort` / `overflow-checks`** | Atomic shared pointer; reader-writer lock; panicking unwrap; panic strategy; arithmetic-overflow checking (disabled in release → F7). |
| **`ExactSizeIterator` / `FusedIterator` / `DoubleEndedIterator`** | Rust iterator traits targeted by issue #92 (full RSOS contract, §2.4). |

---

## 4. Bibliography

### 4.1 Cross-community vocabulary

> Two literatures work on the same recursive-partition skeleton under two names. This document
> searched one of them until 2026-08-14. The map is the instrument that catches the other; it is not
> a claim that the two are interchangeable — the rows marked **No** are where conflating them
> produces a wrong statement.

| Here (`cs.DC` / `cs.CR`) | There (`cs.IT` / `cs.NI`) | Same? |
|---|---|---|
| **RBSR** — range-based set reconciliation | **PSR** — partitioned set reconciliation | **Cousins.** Same skeleton; the per-partition primitive differs — next row |
| `Fingerprint` / `RangeAggregate` / comparison value `f_p` | **SR** — set representation data structure `Z` | **No.** `Z.recovery()` restores the differing *elements* (CPI / IBLT / BCH); a fingerprint only decides equality |
| enumeration threshold `t`, on `\|X ∩ [l,u)\|` — **range size** | `m̄`, on `δ` — **number of differences** (= sketch capacity) | **No.** Analogous role, different quantity |
| fan-out `b`, `FixedFanOut`, balanced `b`-partition (Def. 3.8) | partition arity; **`Q`-ary** / **`d`-ary** splitting | ≈ |
| difference size `d` | `δ` | Yes |
| store size `n` | — (PSR bounds are stated over `δ` alone) | No counterpart |
| refinement round / one-way message | communication round | ≈ |
| `T_loc` | time complexity | ≈ |

**The two lines share one ancestor and have not read each other since.** Verified from both reference
lists: Meyer (SRDS 2023) and arXiv:2603.19820 [16] both cite Minsky & Trachtenberg (Allerton 2002);
arXiv:2603.19820's 27 references carry **no** tree-algorithm, PSR-as-named or benchmarking-framework
work, and arXiv:2509.02373's 25 references carry **no** Meyer, Amparore, Negentropy or Willow.

```
                 Minsky & Trachtenberg, Allerton 2002
                        (divide-and-conquer root)
                    ┌──────────────┴──────────────┐
      fingerprint-based                      sketch-based
      RBSR  (cs.DC/cs.CR)                    PSR  (cs.IT/cs.NI)
      Meyer 2023 → Amparore 2026             Lázaro & Stefanović 2025 (EPSR)
      Negentropy, Willow                     CPI, IBLT, GenSync, tree algorithms
                    └──────── no citations either way ────────┘
```

*Bibliographic caveat:* that root is cited as **Scalable** set reconciliation (arXiv:2603.19820 [16],
and this page) and as **Practical** set reconciliation (arXiv:2509.02373 [10], same venue and year).
Unresolved — check the proceedings before citing either form.

| Community | Venues | Terms to search |
|---|---|---|
| Distributed systems / P2P | SRDS, ICDCS, EuroSys + PaPoC, arXiv preprints, protocol specs | range-based set reconciliation, anti-entropy, Merkle diff, range fingerprint, prolly/MST |
| Information theory / networking | IEEE Trans. Inf. Theory, **IEEE TNSM**, IEEE Trans. Commun., SIGCOMM, ISIT | partitioned set reconciliation, characteristic polynomial interpolation, PinSketch/BCH, IBLT, MET-IBLT, rateless |
| Random access / MAC — *EPSR's source* | IEEE Trans. Inf. Theory, IEEE Trans. Commun., ISIT, GLOBECOM | tree algorithms, collision resolution, splitting algorithms, **`Q`-ary** / **`d`-ary**, Capetanakis, Tsybakov–Mikhailov |

### 4.2 Entry format

```
- **Authors**, *Title*, `arXiv:NNNN.NNNNNvV` | `doi:10.xxxx/…` (venue, year) — <url>
  **Bears on:** <one sentence — the single claim that touches this repo>. → #NNN, §X.Y
```

1. **Version-pinned identifier.** `arXiv:2509.02373v1`, never bare — a claim can move between
   versions, and a silently retargeting citation is the drift §9 exists to stop.
2. **`Bears on:` is mandatory, one sentence.** No bearing, no entry; background belongs in §3.
3. **Forward pointer**, so the bibliography is navigable in both directions rather than write-only.

Entries predating this format are grandfathered; add the two fields when next touching one.

### 4.3 Search log

The coverage boundary is a fact, and it had no home — so a survey pass could not tell what an earlier
pass had already ruled out. Record negative results too.

| Date | Terms / dialect | Scope | Outcome |
|---|---|---|---|
| 2026-05-30 | "range-based set reconciliation" | `cs.DC` / `cs.CR` | Baseline survey. Held the Allerton-2002 root but **never searched the PSR dialect** |
| 2026-08-10 | arXiv:2603.19820 + its four repositories | targeted | §1.3/§2.1/§2.2/§2.3 revised, 8 references added |
| 2026-08-14 | "partitioned set reconciliation"; reference lists of arXiv:2509.02373 (25) and arXiv:2603.19820 (27), walked one level | `cs.IT` / `cs.NI` | **§4.4's `cs.IT` group** — EPSR, GenSync, the tree-algorithm arity lineage, multi-party, MET-IBLT. §2.2 revised |
| 2026-08-14 | `q`-ary trie / digital-tree space law, `q`/ln `q` | analysis of algorithms | **Not established.** The measured `b`/ln `b` law has no confirmed analytical counterpart — open |

### 4.4 Bibliography

**Set reconciliation — range-based (`cs.DC` / `cs.CR`)**
- A. Meyer, *Range-Based Set Reconciliation*, arXiv:2212.13567 (IEEE SRDS 2023) — https://arxiv.org/abs/2212.13567 ; primer: https://logperiodic.com/rbsr.html
- L. Yang, Y. Gilad, M. Alizadeh, *Practical Rateless Set Reconciliation*, SIGCOMM 2024, arXiv:2402.02668 — https://arxiv.org/abs/2402.02668 ; impl. https://github.com/yangl1996/riblt
- minisketch (Bitcoin Core) — https://github.com/bitcoin-core/minisketch ; BIP 330 — https://bips.dev/330/
- Erlay (Naumenko et al., CCS 2019) — https://arxiv.org/abs/1905.10518
- E. G. Amparore, *RBSR via Range-Summarizable Order-Statistics Stores* (RSOS / AELMDB), arXiv:2603.19820 (2026) — https://arxiv.org/html/2603.19820 ; software: AELMDB https://github.com/amparore/aelmdb , Negentropy integration https://github.com/amparore/negentropy-aelmdb , benchmark harness https://github.com/amparore/bench-aelmdb
- A. Meyer, K. Scherer, *Range-Based Set Reconciliation without Homomorphic Hashing*, preprint 2024 —
  https://aljoscha-meyer.de/assets/landing/rbsr_nonhomomorphic.pdf — RBSR over history-independent,
  clamping-invariant trees using conventional hashes. **The direct counter-argument to §2.3 #1**:
  the composable-monoid summary is one design point, not a requirement of the algorithm.
- L. Gong, Z. Liu, L. Liu, J. Xu, M. Ogihara, T. Yang, *Space- and computationally-efficient set
  reconciliation via Parity Bitmap Sketch (PBS)*, VLDB 14(4), 2020 — a further point on the
  communication/computation Pareto front, alongside RIBLT and minisketch (§2.2).
- Y. Minsky, A. Trachtenberg, *Scalable set reconciliation*, Allerton 2002 — the divide-and-conquer
  ancestry of range-based refinement, predating the RBSR framing. **Also the root of the PSR line**
  ([§4.1](#41-cross-community-vocabulary)): the one paper both dialects cite, and the reason this
  page held the ancestor for months without ever reaching its `cs.IT` continuation.
- *CertainSync: Rateless Set Reconciliation with Certainty*, arXiv:2504.08314 (SIGMETRICS 2025) — https://arxiv.org/abs/2504.08314
- *ConflictSync*, arXiv:2505.01144 (2025, Baquero group) — the first digest-driven synchronisation
  algorithm for state-based CRDTs, cutting transfer up to 18× — https://arxiv.org/abs/2505.01144
- *Rateless Bloom Filters*, arXiv:2510.27614 (2025, Baquero group) — https://arxiv.org/abs/2510.27614
  ; both validate §2.2's hybrid conclusion and show delta-CRDT sync converging toward digest-driven
  sync, i.e. toward what this crate already does.

**Set reconciliation — partitioned and sketch-based (`cs.IT` / `cs.NI`)** *(§4.1's other dialect,
opened 2026-08-14. arXiv:2509.02373 and arXiv:2603.19820 were read as PDFs; the rest of this group is
sourced from abstracts and search summaries — read before quoting a number from it.)*

- **F. Lázaro, Č. Stefanović**, *Tree algorithms for set reconciliation*, `arXiv:2509.02373v1`
  (submitted to IEEE, 2025) — https://arxiv.org/abs/2509.02373
  **Bears on:** EPSR transmits one child's SR per split and derives the sibling's by subtracting from
  the parent's, so a **group**-valued summary saves one transmission per split where a monoid or a
  conventional hash cannot — `Fingerprint` (add/sub mod 2²⁵⁶) qualifies. Their near-halving is
  specific to **binary** partitioning; at fan-out `b` the saving is `1/b`, ~6 % at `b` = 16.
  → [#298](https://github.com/Akvize/reconcile-rs/issues/298), [#185](https://github.com/Akvize/reconcile-rs/issues/185), §2.2
- **N. Boškov, A. Trachtenberg, D. Starobinski**, *GenSync: A New Framework for Benchmarking and
  Optimizing Reconciliation of Data*, `doi:10.1109/TNSM.2022.3164369` (IEEE TNSM 19(4), 2022) —
  https://github.com/nislab/gensync
  **Bears on:** an open-source testbed for set-reconciliation *families* with a cgroup-based
  latency/bandwidth/loss lane, reporting no universally dominant protocol; **carries no RBSR**, so a
  harness claim here scopes to refinement policies inside RBSR, and its injection lane is prior art
  to evaluate before building one. → [#280](https://github.com/Akvize/reconcile-rs/issues/280), [#174](https://github.com/Akvize/reconcile-rs/issues/174), §2.2
- **J. Capetanakis**, *Tree algorithms for packet broadcast channels*, `doi:10.1109/TCOM.1979.1094661`
  (IEEE Trans. Commun. 25(5), 1979) · **P. Mathys, P. Flajolet**, *Q-ary collision resolution
  algorithms in random-access systems with free or blocked channel access*,
  `doi:10.1109/TIT.1985.1057013` (IEEE Trans. Inf. Theory 31(2), 1985)
  **Bears on:** the founding and the `Q`-ary analyses of splitting when conflict locations are
  unknown — `Q` ∈ {2,3} preferred, degrading past 3, which is where this repo's measured `b`/ln `b`
  optimum also lands. Different objective (channel throughput), so a convergence to investigate, not
  a transferable bound. → [#257](https://github.com/Akvize/reconcile-rs/issues/257), §2.2
- **Q. Vogel, Y. Deshpande, Č. Stefanović, W. Kellerer**, *Analysis of d-ary tree algorithms with
  successive interference cancellation*, `doi:10.1017/jpr.2023.107` (J. Applied Prob. 61(3), 2024;
  preprint `arXiv:2302.08145`) — https://arxiv.org/abs/2302.08145
  **Bears on:** disproves binary-optimality — maximal throughput is reachable at any `d` ≥ 2 **given
  suitable splitting probabilities** — so the binding axis is plausibly the split *distribution*
  rather than the arity, which is what Def. 3.8's balanced equal-rank partition fixes and what
  [#318](https://github.com/Akvize/reconcile-rs/issues/318) proposes to vary. → [#318](https://github.com/Akvize/reconcile-rs/issues/318), §2.2
- **A. J. E. M. Janssen, M. J. de Jong**, *Analysis of contention tree algorithms*,
  `doi:10.1109/18.868486` (IEEE Trans. Inf. Theory 46(6), 2000)
  **Bears on:** levels-to-resolution statistics for arbitrary node degree — the analytical form of
  the round-count column `benches/protocol.rs` reports empirically. → [#257](https://github.com/Akvize/reconcile-rs/issues/257)
- **Y. Minsky, A. Trachtenberg, R. Zippel**, *Set reconciliation with nearly optimal communication
  complexity*, `doi:10.1109/TIT.2003.815784` (IEEE Trans. Inf. Theory 49(9), 2003)
  **Bears on:** CPI, the primitive PSR partitions down to and the `≈ b·d` optimum §2.2's table
  quotes through minisketch. → §2.2
- **M. Mitzenmacher, R. Pagh**, *Simple multi-party set reconciliation*,
  `doi:10.1007/s00446-017-0316-0` (Distributed Computing 31(6), 2018; preprint `arXiv:1311.2037`) —
  https://arxiv.org/abs/1311.2037
  **Bears on:** the only entry here that is not two-party. Every cost model on this page is stated
  for one pair while `ReplicatedMap` runs an N-node cluster at O(N) write amplification — an
  unexamined axis with an existing literature. → §1.2, §2.2, [#174](https://github.com/Akvize/reconcile-rs/issues/174)
- **F. Lázaro, B. Matuz**, *A rate-compatible solution to the set reconciliation problem*,
  `arXiv:2211.05472v2` (IEEE Trans. Commun. 71(10), 2023 — v2 is the accepted revision) —
  https://arxiv.org/abs/2211.05472
  **Bears on:** MET-IBLTs reconcile **without estimating `|d|`** and without worst-case oversizing —
  a third option against which #185 weighs a fixed-capacity IBLT (needs a capacity guess) and RIBLT
  (Ω(n) encoder per session). → [#185](https://github.com/Akvize/reconcile-rs/issues/185)
- **M. Goodrich, M. Mitzenmacher**, *Invertible Bloom lookup tables*, Allerton 2011 ·
  **D. Eppstein, M. Goodrich, F. Uyeda, G. Varghese**, *What's the difference? Efficient set
  reconciliation without prior context*, `doi:10.1145/2043164.2018462` (SIGCOMM 2011) ·
  **P. Ozisik et al.**, *Graphene*, `doi:10.1145/3341302.3342082` (SIGCOMM 2019)
  **Bears on:** the IBLT origin, the difference-digest framing §1.3's table row rests on, and the
  Bloom-prefilter-plus-IBLT hybrid that prefigures ConflictSync's two-stage design.
  → [#185](https://github.com/Akvize/reconcile-rs/issues/185), §1.3

**Merkle / anti-entropy structures**
- A. Auvolat, F. Taïani, *Merkle Search Trees*, SRDS 2019 — https://inria.hal.science/hal-02303490 ; crate https://github.com/domodwyer/merkle-search-tree ; Bluesky/atproto usage — https://atproto.com/specs/repository
- Prolly trees (Dolt/Noms) — https://docs.dolthub.com/architecture/storage-engine/prolly-tree ; https://www.dolthub.com/blog/2025-06-03-people-keep-inventing-prolly-trees/
- J. Gustafson, *Merklizing the key/value store* (Merkle radix / SMT) — https://joelgustafson.com/posts/2023-05-04/merklizing-the-key-value-store-for-fun-and-profit/
- Merkle-CRDTs, arXiv:2004.00107 — https://arxiv.org/abs/2004.00107
- Dynamo (DeCandia et al., SOSP 2007) — https://www.allthingsdistributed.com/files/amazon-dynamo-sosp2007.pdf
- Cassandra repair / over-streaming — https://www.pythian.com/blog/effective-anti-entropy-repair-cassandra
- Willow 3d-RBSR (fingerprint security) — https://willowprotocol.org/specs/3d-range-based-set-reconciliation/index.html ; Negentropy — https://github.com/hoytech/negentropy
- Demers et al., *Epidemic Algorithms*, PODC 1987 ; SWIM — https://www.cs.cornell.edu/projects/Quicksilver/public_pdfs/SWIM.pdf ; memberlist — https://github.com/hashicorp/memberlist

**Aggregate-augmented and page-oriented trees** *(the structural ancestry of `FingerprintTreeMap`,
surfaced by arXiv:2603.19820's related work — §2.4 P1/P2 and issues #257/#271 all land here)*
- S. Tatham, *Counted B-Trees* (2004) — https://www.chiark.greenend.org.uk/~sgtatham/algorithms/cbtree.html
  — the subtree-count augmentation giving O(log n) rank/select. Direct prior art for `tree_size`:
  the order-statistic half of RSOS is a documented classic, not a 2026 result.
- Z. Zhao, D. Xie, F. Li, *AB-tree: Index for concurrent random sampling and updates*, VLDB 15(9),
  2022 — maintaining aggregate metadata inside a page-oriented tree **under concurrent updates**.
  The reference for the contention this design has by construction (every insert rewrites the root
  aggregate, today hidden behind one global `RwLock`) and for epic
  [#271](https://github.com/Akvize/reconcile-rs/issues/271).
- F. Li, M. Hadjieleftheriou, G. Kollios, L. Reyzin, *Dynamic authenticated index structures for
  outsourced databases* (Embedded Merkle B-tree), SIGMOD 2006 — twenty years of prior art on
  caching digests inside a B-tree. Different goal (verifiable query answering, not range
  aggregation), but it bounds any novelty claim and supplies the vocabulary if inclusion proofs
  ever become a requirement.
- S. Roura, *A new method for balancing binary search trees*, ICALP 2001 — balancing by subtree
  weight; background for the untuned interaction between the tree order (6) and the protocol
  fan-out ([#257](https://github.com/Akvize/reconcile-rs/issues/257)).
- H. Chu et al., *LMDB* — https://github.com/LMDB — a copy-on-write, memory-mapped B+-tree with
  lock-free MVCC readers and a single writer. Named here because that *is* the property epic
  [#271](https://github.com/Akvize/reconcile-rs/issues/271) sets out to build in safe Rust: the
  build-vs-adopt comparison should be made against it explicitly rather than by default.

**Consistency & conflict resolution**
- Kingsbury (Jepsen), *The trouble with timestamps* — https://aphyr.com/posts/299-the-trouble-with-timestamps ; *Jepsen: Cassandra* — https://aphyr.com/posts/294-jepsen-cassandra
- S. Kulkarni et al., *Hybrid Logical Clocks*, 2014 — https://cse.buffalo.edu/tech-reports/2014-04.pdf
- Shapiro et al., *CRDTs*, INRIA RR-7506 / SSS 2011 — https://inria.hal.science/inria-00555588/en/
- Preguiça et al., *Dotted Version Vectors*, arXiv:1011.5808 — https://arxiv.org/abs/1011.5808
- Clarke et al., *Incremental Multiset Hash Functions*, ASIACRYPT 2003 — https://people.csail.mit.edu/devadas/pubs/mhashes.pdf
- Abadi, *PACELC* — https://en.wikipedia.org/wiki/PACELC_design_principle ; ScyllaDB repair-based tombstone GC — https://www.scylladb.com/2022/06/30/preventing-data-resurrection-with-repair-based-tombstone-garbage-collection/

**Product positioning**
- Pekko Distributed Data — https://pekko.apache.org/docs/pekko/current/typed/distributed-data.html
- Hazelcast Replicated Map — https://docs.hazelcast.com/hazelcast/5.6/data-structures/replicated-map
- iroh / iroh-docs — https://github.com/n0-computer/iroh ; automerge — https://github.com/automerge/automerge

---

## 5. Alphabetical index

> Index of the [glossary (§3)](#3-glossary) terms. Each entry links to the subsection where the term
> is defined: [3.1 repo → code](#g91) · [3.2 structures/algos](#g92) · [3.3 distributed](#g93) ·
> [3.4 crypto/network](#g94) · [3.5 complexity](#g95) · [3.6 Rust](#g96).

**A** — AB-tree [3.2](#g92) · AEAD [3.4](#g94) · AELMDB [3.2](#g92) · amplification [3.4](#g94) · anti-entropy [3.3](#g93) · `Arc` [3.6](#g96) · `ArrayVec` [3.6](#g96) · associative [3.3](#g93)

**B** — B-tree / B+-tree [3.5](#g95) · BCH codes [3.2](#g92) · Berlekamp-Massey [3.2](#g92) · bincode [3.6](#g96) · bincode allocation bomb [3.4](#g94) · BIP 330 [3.2](#g92) · birthday bound [3.4](#g94) · BLAKE3 [3.4](#g94) · Bloom filter [3.2](#g92)

**C** — CAP [3.3](#g93) · CAS [3.2](#g92) · Cassandra [3.2](#g92) · Counted B-tree [3.2](#g92) · causal consistency / causal+ [3.3](#g93) · causal stability [3.3](#g93) · CDC (content-defined chunking) [3.2](#g92) · CertainSync [3.2](#g92) · chrono [3.6](#g96) · CID [3.2](#g92) · clippy [3.6](#g96) · clock skew [3.3](#g93) · CmRDT [3.3](#g93) · collision [3.4](#g94) · commit-wait [3.3](#g93) · commutative [3.3](#g93) · content-addressing [3.2](#g92) · CPI / CPISync [3.2](#g92) · CRDT [3.3](#g93) · CvRDT [3.3](#g93)

**D** — datagram [3.4](#g94) · `DateTime<Utc>` [3.6](#g96) · `DefaultHasher` [3.4](#g94) · Dolt / DoltHub [3.2](#g92) · `DoubleEndedIterator` [3.6](#g96) · DRDoS [3.4](#g94) · DTLS [3.4](#g94) · DVV (Dotted Version Vector) [3.3](#g93) · Dynamo [3.2](#g92)

**E** — Earthstar [3.2](#g92) · EMB-tree (Embedded Merkle B-tree) [3.2](#g92) · epidemic [3.3](#g93) · Erlay [3.2](#g92) · eventual consistency [3.3](#g93) · `ExactSizeIterator` [3.6](#g96)

**F** — fan-out [3.5](#g95) · `FusedIterator` [3.6](#g96) · fuzzing [3.6](#g96)

**G** — `gc_grace_seconds` [3.3](#g93) · GF(2)-linear [3.4](#g94) · gossip [3.3](#g93) · Graphene [3.2](#g92)

**H** — happens-before [3.3](#g93) · Hazelcast [§2/product](#g92) · hinted handoff [3.3](#g93) · history-independence [3.2](#g92) · HLC (Hybrid Logical Clock) [3.3](#g93) · HMAC [3.4](#g94) · homomorphic hash [3.4](#g94) · HyParView [3.3](#g93)

**I** — IBLT [3.2](#g92) · idempotent [3.3](#g93) · incremental hash [3.4](#g94) · ipnet [3.6](#g96) · iroh / iroh-docs [3.2](#g92)

**J** — join-semilattice [3.3](#g93)

**L** — Lamport clock [3.3](#g93) · LMDB [3.2](#g92) · leading-zeros (attack) [3.2](#g92) · LtHash [3.4](#g94) · LWW (Last-Write-Wins) [3.3](#g93) · LWW-Register [3.3](#g93)

**M** — MAC [3.4](#g94) · memberlist [3.3](#g93) · Merkle-CRDT [3.2](#g92) · Merkle-DAG [3.2](#g92) · Merkle radix / Patricia [3.2](#g92) · Merkle tree / root [3.2](#g92) · minisketch [3.2](#g92) · miri [3.6](#g96) · monoid [3.5](#g95) · monotone [3.3](#g93) · MSet-Mu-Hash / MSet-XOR-Hash [3.4](#g94) · MSRV [3.6](#g96) · MST (Merkle Search Tree) [3.2](#g92) · MTU [3.4](#g94) · MV-Register [3.3](#g93)

**N** — *n / d / U / b* (notation) [3.5](#g95) · Negentropy [3.2](#g92) · Noise [3.4](#g94) · Noms [3.2](#g92) · NTP [3.3](#g93)

**O** — O(log n) / O(d log n) [3.5](#g95) · once_cell [3.6](#g96) · order statistics (rank/select) [3.5](#g95) · OR-Set [3.3](#g93) · over-streaming [3.2](#g92)

**P** — PACELC [3.3](#g93) · PBS (Parity Bitmap Sketch) [3.2](#g92) · `panic=abort` [3.6](#g96) · parking_lot [3.6](#g96) · partition [3.3](#g93) · Patricia trie [3.2](#g92) · Pekko Distributed Data [§2/product](#g92) · PinSketch [3.2](#g92) · prolly tree [3.2](#g92) · proptest [3.6](#g96) · PTP [3.3](#g93) · push / pull [3.3](#g93)

**Q** — QUIC [3.4](#g94) · quickcheck [3.6](#g96) · quorum [3.3](#g93)

**R** — rand [3.6](#g96) · range-cmp / `RangeOrdering` [3.6](#g96) · rank / select [3.5](#g95) · Rateless IBLT (RIBLT) [3.2](#g92) · RBSR [3.2](#g92) · read repair [3.3](#g93) · resurrection / zombie [3.3](#g93) · Riak [3.2](#g92) · rolling hash [3.2](#g92) · rumor mongering [3.3](#g93) · `RwLock` [3.6](#g96) · RSOS [3.2](#g92)

**S** — ScyllaDB [3.2](#g92) · second-preimage [3.4](#g94) · SEC (Strong Eventual Consistency) [3.3](#g93) · serde [3.6](#g96) · session guarantees [3.3](#g93) · SipHash [3.4](#g94) · SMT (Sparse Merkle Tree) [3.2](#g92) · spoofing [3.4](#g94) · split-brain [3.3](#g93) · Strata Estimator [3.2](#g92) · structural sharing [3.2](#g92) · SWIM [3.3](#g93)

**T** — Thomas write rule [3.3](#g93) · TLS [3.4](#g94) · tokio [3.6](#g96) · tracing [3.6](#g96) · transitive group [3.4](#g94) · TrueTime [3.3](#g93)

**U** — UDP [3.4](#g94) · `unwrap` [3.6](#g96) · `overflow-checks` [3.6](#g96)

**V** — vector clock / version vector [3.3](#g93) · Vivaldi [3.3](#g93) · Voldemort [3.2](#g92)

**W** — Willow [3.2](#g92) · Writes-Follow-Reads [3.3](#g93)

**X** — XOR [3.4](#g94) · xxHash [3.4](#g94)

---

*The state-of-the-art positioning was produced by a literature survey across four themes
(set-reconciliation algorithms, diffable/Merkle structures, consistency & conflict resolution, and
the Rust ecosystem), every claim backed by a cited source. The accompanying code-audit findings,
and their resolution status, live in [`PROGRESS.md`](./PROGRESS.md).*

# State of the Art — `reconcile-rs` positioning

> **Reference document.** Where `reconcile-rs` sits in the landscape of set reconciliation, diffable
> data structures, and replica consistency — plus a glossary and bibliography. This is **durable
> background**: the field positioning and the design taxonomy move slowly, unlike the code. It
> deliberately carries **no status or findings** — for the live correctness, security and maturity
> state see [`PROGRESS.md`](./PROGRESS.md); for the target design see [`ARCHITECTURE.md`](./ARCHITECTURE.md).
>
> - **Literature survey dated:** 2026-05-30 (sources cited inline and in the [bibliography (§4)](#4-bibliography)).
> - **Scope:** the HRTree as a *data structure* and RBSR as an *algorithm*, compared to the published
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
| **XOR RBSR (= reconcile-rs)** | O(d log n) | O(d log n) | **O(log n)** | No (self-adapting) | **Weak** (forgeable XOR) | Earthstar, Willow, Negentropy |
| Secure-fingerprint RBSR (≥256-bit) | O(d log n) | O(d log n) | O(log n) | No | Good | Negentropy (prod) |
| IBLT / Difference Digest | O(d·(b+log U)) | **O(d)** | 1 (+estim.) | **Yes** | Weak | blockchains |
| **Rateless IBLT (SIGCOMM 2024)** | **≈ d** (3-4× < non-rateless) | **linear** (2-2000× < minisketch) | **1 streaming** | **No** | **Designed for adversarial** | Ethereum state-sync |
| minisketch / PinSketch (CPI) | **optimal ≈ b·d** | O(d²) | 1 (+ext.) | **Yes (capacity)** | deterministic if capacity OK | Bitcoin Erlay (BIP 330) |
| Merkle-tree diffing | O(d log n) | O(d log n) | O(log n) | No | hash-dependent | Dynamo, Cassandra, Riak |

Sources: Meyer arXiv:2212.13567 & logperiodic.com/rbsr.html; *Practical Rateless Set
Reconciliation*, SIGCOMM 2024, arXiv:2402.02668; minisketch (bitcoin-core) & BIP 330; Erlay
(CCS 2019); arXiv:2603.19820 (RSOS, 2026).

**Key takeaway:** for the **large-n / small-d / latency-sensitive** profile, RBSR is the **worst
family on latency** (O(log n) sequential RTTs) whereas **Rateless IBLT** finds the diff in a single
streaming exchange, with no *d* estimation, and with adversarial robustness — it is the **current
SOTA choice** for this use case.

### 1.4 The SOTA of Merkle/anti-entropy structures

Important panel nuance: **HRTree does NOT belong to the Merkle Search Tree (MST) / prolly-tree
family**, and that is a point in its favor. MST (Auvolat & Taïani, SRDS 2019) and prolly-trees
(Dolt/Noms) *need* **insertion-order independence** because they diff by comparing the hashes of the
tree's **internal nodes**. HRTree, by contrast, diffs **value-defined ranges**: the cumulative XOR
over `[a,b)` is identical on two peers iff the *content* of the range is identical, **regardless of
each one's B-tree shape**. HRTree therefore obtains the convergence guarantee that MST/prolly pay
for with history-independence, **without paying for it** — and thereby escapes the MST
"leading-zeros" attack. The B-tree's order-dependence is therefore **not** a defect here.

### 1.5 The SOTA of consistency and conflict resolution

- **Physical-clock LWW**: a documented anti-pattern (Jepsen/Kingsbury "The trouble with
  timestamps"; real NTP incidents). The "winner" is the node with the most-advanced clock, not the
  causally latest write → silent lost update.
- **Minimal SOTA fix**: **Hybrid Logical Clocks (HLC, Kulkarni 2014)** — 64-bit drop-in, monotonic,
  respects causality, divergence bounded by ε; adopted by CockroachDB and MongoDB.
- **Tie-break**: must be a **deterministic total order** (e.g. `(HLC, node_id)`). The current `keep
  local on equal` is non-convergent.
- **Tombstone GC**: must be by **causal stability** (acknowledgment by all replicas) — Cassandra:
  `gc_grace_seconds` = **10 days** + a complete repair within the window; ScyllaDB: repair-based GC.
  **60 s** makes the safety precondition nearly impossible to honor.

---

## 2. Competitor audit and differentiators

> This section refocuses the analysis on the **HRTree as a data structure** (and its protocol),
> not on the full system. *(All structure/algo names below are defined in the
> [glossary §3.2](#g92).)* Methodological anchor: the HRTree **is not a [Merkle tree](#g92) in the
> [MST](#g92)/[prolly](#g92) sense**. It is a *[Range-Summarizable Order-Statistics Store](#g92)*
> (RSOS) — a B-tree augmented, per node, with a **composable subtree summary** (the XOR of hashes)
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
- **vs HRTree:** MST *pays* for history-independence; HRTree does not (value-based diff, §2.3) and
  **escapes the leading-zeros attack**. But MST gains structural sharing (versioning) that HRTree
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
- **vs HRTree:** prolly = SOTA if you want **versioning + persistence + branch/merge**. HRTree is
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
- **vs HRTree:** this is precisely the defect RBSR/HRTree fix (the recursion tightens onto the
  actually-differing elements). **Clear advantage to HRTree** on this axis.

#### RSOS / AELMDB (arXiv:2603.19820, 2026) — *the most direct competitor*
The paper formalizes "**B+-tree augmented with subtree counts + composable summaries**" as the RSOS
abstraction, proves RBSR's local-cost bounds on this backend, and ships **AELMDB**: a **persistent,
memory-mapped** LMDB extension, evaluated with Negentropy.
- **vs HRTree:** **it is the same design**, but (a) **persistent** (LMDB) and (b) with a **secure
  summary** (Negentropy). HRTree *is* an RSOS — but the in-memory, 64-bit-XOR, non-persistent
  version. **The structure's SOTA in this niche = "persistent RSOS + secure fingerprint", and the
  HRTree→SOTA delta reads directly as that gap.**

| Structure | Position/boundary | History-indep. | Diffs… | Structural sharing / versioning | Persistence | Resists leading-zeros | Maturity |
|---|---|---|---|---|---|---|---|
| **HRTree** | B-tree splits (insertion order) | **No** | **value ranges** | No | No (in-mem) | **Yes** (n/a) | pre-alpha |
| MST | level = hash(key) | Yes | nodes | partial | impl-dependent | **No** | mature (Bluesky) |
| Prolly tree | rolling-hash on content | Yes | chunks | **Yes** (CAS) | **Yes** | Yes | mature (Dolt) |
| Merkle radix/SMT | prefix bits | Yes | hash paths | partial | yes | Yes | mature (Ethereum) |
| Fixed-depth Merkle | token range | partial (rebuild) | nodes | no | yes | yes | mature (Cassandra) |
| **RSOS/AELMDB** | augmented B+-tree | not required | ranges | no | **Yes** (LMDB) | yes | research 2026 |

### 2.2 Competitors at the "reconciliation algorithm" level

The HRTree implements **RBSR**; its competitors are not tree structures.

| Family | Communication | Compute | RTT | Knows *d*? | Adversarial robustness | Maturity |
|---|---|---|---|---|---|---|
| **XOR RBSR (HRTree)** | O(d log n) | O(d log n) | **O(log n) sequential** | No (self-adapting) | **Weak** | Earthstar/Willow/Negentropy |
| **Rateless IBLT** (SIGCOMM 2024) | **≈ d** (3-4× < non-rateless) | **linear** (2-2000× < minisketch) | **1 streaming exchange** | **No** | **designed for adversarial** | Ethereum state-sync |
| minisketch/PinSketch (CPI) | **optimal ≈ b·d** | O(d²) | 1 (+ext.) | **Yes (capacity)** | deterministic if capacity | Bitcoin Erlay (BIP 330) |
| CertainSync (2025) | bound f(d,U) | linear | rateless | No | **deterministic success** | SIGMETRICS research |
| Classic IBLT | O(d·(b+log U)) | O(d) | 1 (+estim.) | **Yes** | weak | blockchains |

**Critical reading (stated profile: large n, small d, latency-sensitive, P2P):**
- RBSR is the **worst family on latency**: O(log n) **sequential RTTs** (≈3 for 1M, ≈4 for 1B). On a
  1 ms-RTT LAN that's several ms; on WAN far more — a cost the README's loopback benchmarks hide
  (cf. F16).
- **Rateless IBLT** finds the diff in a **single streaming exchange**, without estimating *d*, with
  explicit adversarial robustness and linear compute → **single-shot SOTA choice** for this use case.
- **But** RBSR keeps two assets that sketches lack: **self-adapting** (no *d* estimation, no failure
  if *d* is mis-guessed) and **ordered-range reconciliation** (partial sync by prefix/subspace —
  what Willow exploits in 3D). Sketches reconcile an *opaque* set.
- **Conclusion:** a **hybrid** SOTA design — RBSR to localize coarsely + a sketch (Rateless IBLT) to
  drain divergent leaves in one shot — would beat pure HRTree on latency without losing
  adaptiveness.

### 2.3 Real differentiators of the approach (structural strengths)

1. **Value-based range diff ⇒ history-independence is not needed** *(the deepest differentiator)*.
   MST/prolly *must* be history-independent because they compare **internal node hashes** (different
   tree shapes → false positives). HRTree never compares nodes: it computes the **cumulative XOR
   over `[a,b)`**, identical on two peers **iff the range content is identical**, regardless of each
   one's B-tree shape. → Convergence guaranteed **without paying** for history-independence, and
   **immunity to the MST leading-zeros attack**.
2. **It is a SOTA-2026-conformant RSOS**: the `tree_hash` cache (composable summary) + `tree_size`
   (order statistic) → range-summary and rank/select queries in **O(log n)** (the arXiv:2603.19820
   contract). Core *aligned* with the most recent theory.
3. **Cheap incremental maintenance**: `tree_hash ^= diff_hash` + `tree_size += 1` propagated along
   the single root→leaf path → O(log n) amortized. The 2-3× factor vs `BTreeMap` is the *expected*
   price of these two invariants, not an anomaly.
4. **A single structure stores AND reconciles**: no separate Merkle tree to maintain (contrast
   Cassandra which builds the tree at repair time). The store *is* the reconciliation index.
5. **Avoids Cassandra's over-streaming** (16-way recursion tightened onto real diffs).
6. **Rust-native, in-process, embeddable**: a real ecosystem niche (mature equivalents = JVM).

### 2.4 The design axes of a *true* SOTA RSOS

These are the axes along which an RSOS is judged against the state of the art — the **design
target** for a structure of this family ("persistent RSOS with a secure, generic fingerprint").
They are described here as durable design goals; which of them `reconcile-rs` has since addressed
is tracked in [`PROGRESS.md`](./PROGRESS.md) (the `Fxx` pointers below map to that table).

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
4. **Generic summary over a monoid**: `HRTree<XOR>` → `RSOS<M: Monoid>` (secure fingerprint, but
   also sum/min/max/count, sketches). Enables **embedding a sketch in the leaves** (hybrid RBSR +
   Rateless IBLT) to break the O(log n) RTT cost (§2.2).
5. **Fully expose the RSOS contract**: **lazy + double-ended** iterators (repo issues #90-92),
   public `rank`/`select`/`seek_lower_bound`/`seek_upper_bound` → a reusable generic building block.

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

**In one sentence:** the HRTree starts from the **right skeleton** — an RSOS, the design validated by
2026 research, with a real differentiator (value-based diff that removes the need for
history-independence). The remaining distance to a *true* SOTA structure is along the axes above; the
structural ones (secure/generic fingerprint, persistence/content-addressing, property-testing
foundation) belong to the structure itself, while conflicts, GC and robustness belong to the
surrounding system.

---

## 3. Glossary

> Lists **(a)** the identifiers and constants introduced by the repository, **(b)** the competing
> structures and algorithms cited, **(c)** the acronyms and concepts of distributed systems,
> cryptography, networking and complexity, **(d)** the Rust tooling. `Fxx` references denote the original audit findings (status tracked in
> [`PROGRESS.md`](./PROGRESS.md)); `file:line` references point to the code.

<a id="g91"></a>
### 3.1 — Repository-specific terms and identifiers

| Term | Definition |
|---|---|
| **HRTree** (*Hash-Range Tree*) | The central structure (`hrtree.rs`): a hand-written B-tree where each node caches `tree_hash` (XOR of the subtree hashes) and `tree_size`. Enables a cumulative-hash range query in O(log n). It is an RSOS (§3.2). |
| **`tree_hash`** | Node field: cumulative XOR of the `hash(key,value)` of all subtree elements. The composable summary that drives the diff. Maintained incrementally (`tree_hash ^= diff_hash`). |
| **`tree_size`** | Node field: number of subtree elements (order statistic). Gives `len()` in O(1) and rank/select navigation. |
| **`hash(key, value)`** | Function (`hrtree.rs:35-40`) computing the 64-bit hash of a pair via `DefaultHasher`. The fingerprint building block (F6, F8). |
| **`B` / `MIN_CAPACITY` / `MAX_CAPACITY`** | B-tree parameters: `B = 6`, node capacity 5 to 11 keys (`hrtree.rs:42-44`). |
| **`refresh_hash_size`** | Recomputes a node's `tree_hash` and `tree_size` from its elements and children (`hrtree.rs:70-84`). |
| **`check_invariants`** | Test function (`hrtree.rs:495-560`) that re-verifies ordering, min node size, height balance, and the correctness of the `tree_hash`/`tree_size` caches. An excellent tool, but only run on fixed sequences (F11). |
| **`insertion_position` / `key_at` / `position`** | Order-statistic navigation: insertion index of a key, key at a given index, index of a key. O(log n). |
| **`get_range`** | Lazy iterator over a key range (O(log n + k)). Materialized with clones in the network handler (F16/perf). |
| **`with_mut` / `get_mut`** | In-place mutation of a value with restoration of the hash invariant. Contains the stray `println!` (F12, `hrtree.rs:315`). |
| **`rebalance_after_deletion`** | B-tree rebalancing after deletion (steal-left/steal-right/merge). Has a `TODO` (`hrtree.rs:97`) on a degenerate split case. |
| **`HashRangeQueryable`** | Trait (`diff.rs`) exposing the cumulative-hash range query; implemented by HRTree. |
| **`Diffable`** | Trait (`diff.rs`) carrying `start_diff` and `diff_round`: the RBSR machinery. |
| **`start_diff`** | Emits the root segment `{(−∞,+∞), global hash, size}` that bootstraps a reconciliation. O(1). |
| **`diff_round`** | Protocol core (`diff.rs:85-169`): compares a received segment to the local one, then either concludes or subdivides the range into ~16 sub-segments. Contains the `hash==0` sentinel (F1) and the `unimplemented!()` (F7). |
| **`HashSegment`** | Serialized comparison message: `{ range: (Bound,Bound), hash: u64, size: usize }`. Deserialized from the network without validation (F7). |
| **`DiffRange`** | A range identified as divergent, to be reconciled by element exchange. |
| **`Message`** | Wire-protocol enum: `ComparisonItem(HashSegment)` or `Update((K,V))`. Serialized via bincode. |
| **`ComparisonItem` / `Update`** | The two `Message` variants: a hash segment to compare, vs a key-value pair to apply. |
| **`fingerprint`** | (1) Generally: the range summary used for the diff (here 64-bit XOR). (2) The public method `ReconcileStore::fingerprint(range)`. |
| **`Reconcilable` / `reconcile()`** | Conflict-merge trait (`reconcilable.rs`). The only impl provided: LWW over `(DateTime<Utc>, V)` (F5). |
| **`ReconcileStore`** | The store's public API (`reconcile_store.rs`): wraps the `HRTree`, manages timestamps and tombstones. Its `clone()` is cheap (shares an `Arc`). |
| **`ReconcileEngine`** | Transport/reconciliation layer (`reconcile_engine.rs`): UDP socket, locks, `run()` loop, peer discovery. |
| **`Config`** | Configuration: `port`, `listen_addr`, `peer_net` (`reconcile_store.rs:240-269`). Loopback-safe default but `port=0` unusable as-is. |
| **`with_seed` / `with_port` / `with_listen_addr` / `with_peer_net` / `with_tombstone_timeout`** | Configuration builders. `with_seed` provides a known peer (mitigates scan discovery, F10). |
| **`insert` / `just_insert` / `*_bulk`** | `insert` = local insertion **+** UDP broadcast; `just_insert` = local only (naming footgun, F-API). `_bulk` variants clone the entire input (F16/perf). |
| **`remove` / `just_remove` / `remove_bulk`** | Deletions; write a tombstone `(now, None)`. |
| **`pre_insert` / `add_pre_insert`** | User hook called before insertion. Supposed to run outside the write-lock, but executed **under** the lock on the network path (F14, contradicts `reconcile_store.rs:98-101`). |
| **`tombstone`** | Deletion marker `(timestamp, None)` kept in the tree to propagate the deletion, then purged (F4). |
| **`TimeoutWheel`** | Tombstone-expiry structure (`timeout_wheel.rs`), BTreeMap + HashMap, `std::sync::RwLock` (inconsistent with `parking_lot` elsewhere). |
| **`pop_expired` / `clear_expired_tombstones`** | Purge of wall-clock-expired tombstones (`reconcile_store.rs:208-215`) → resurrection (F4). |
| **`gen_ip`** | Draws a random IP within a CIDR (`gen_ip.rs`) for peer discovery (F10). |
| **`peers` / `peer_net`** | Map of known peers (key = `IpAddr`, 60 s expiry); probed CIDR. Unbounded growth under spoofed IPs (F18). |
| **Timing constants** | `DEFAULT_TIMEOUT` = 60 s (tombstones), `TOMBSTONE_CLEARING` = 1 s, `PEER_EXPIRATION` = 60 s, `ACTIVITY_TIMEOUT` = 1 s (triggers the periodic diff), `BUFFER_SIZE` = 65507 (max UDP datagram), `MAX_SENDTO_RETRIES` (send retries). |

<a id="g92"></a>
### 3.2 — Competing data structures and algorithms

| Term | Definition |
|---|---|
| **RBSR** (*Range-Based Set Reconciliation*) | Algorithm family (Meyer 2023): recursive partition of an ordered set, exchange of range fingerprints, descent into divergent ranges. What reconcile-rs implements. O(log n) RTT. |
| **RSOS** (*Range-Summarizable Order-Statistics Store*) | Abstraction (arXiv:2603.19820, 2026): an ordered set offering **composable** range summaries + rank/select navigation. An augmented B+-tree realizes it → **the HRTree is an RSOS**. |
| **AELMDB** | **Persistent** RSOS implementation (LMDB extension, memory-mapped) from the 2026 paper, evaluated with Negentropy. The most direct competitor to the HRTree. |
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
| **`gc_grace_seconds`** | Cassandra window before purging a tombstone (default **10 days**), safe *if* a complete repair covers it. Compare to the **60 s** here (F4). |
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
| **incremental / homomorphic hash** | A set hash updated incrementally and composable. **MSet-XOR-Hash** (weak, = the current approach), **MSet-Mu-Hash** (finite field), **LtHash** (lattice/vector addition) — secure alternatives for F6. |
| **transitive group** | The minimal algebraic structure required of an RBSR fingerprint (associativity, identity, inverses, transitivity) — XOR satisfies it, hence its convenience *and* its fragility. |
| **MAC / HMAC / AEAD** | Message Authentication Code; HMAC (hash-based); Authenticated Encryption with Associated Data. The F3 fix. |
| **TLS / DTLS / Noise / QUIC** | Secure transport layers (DTLS = TLS over datagrams; Noise = a handshake framework; QUIC = encrypted transport over UDP). Options for F3; cf. issue #96. |
| **spoofing / amplification / reflection / DRDoS** | Forging the source IP (trivial in UDP); a response larger than the request toward a victim; distributed reflection denial of service. The F9 surface. |
| **bincode allocation bomb** | Deserialization where an attacker-controlled length prefix forces a massive pre-allocation (F18). |
| **UDP / datagram / MTU** | Connectionless, unreliable protocol with a spoofable source; bounded datagram (here 65507 bytes); *Maximum Transmission Unit*. |

<a id="g95"></a>
### 3.5 — Complexity, theory and notation

| Term | Definition |
|---|---|
| **B-tree / B+-tree** | A balanced multi-way search tree. B+-tree: values only in the leaves. |
| **order statistics (rank / select)** | "Rank of a key" / "key at rank i" operations in O(log n) thanks to subtree counters (`tree_size`). |
| **monoid** | A set with an associative operation and an identity element; the ideal structure of a generic composable summary (P1 of §2.4). |
| **fan-out** | Number of sub-ranges per recursion round (here ~16, `diff.rs:141`); trades RTT vs message size. |
| **n / d / U / b** | SOTA notation: set size *n*, symmetric-difference size *d*, key universe *U*, element bit-width *b*. |
| **O(log n) / O(d log n)** | Target costs: hash-range query and per-mutation operations in O(log n); diff message volume in O(d log n). |

<a id="g96"></a>
### 3.6 — Rust tooling and ecosystem

| Term | Definition |
|---|---|
| **MSRV** (*Minimum Supported Rust Version*) | The minimum supported Rust version; absent from `Cargo.toml` (F17). |
| **clippy / `-Dwarnings`** | The Rust linter; CI treating warnings as errors. The `mismatched_lifetime_syntaxes` warning (`hrtree_iter.rs:177`) would break CI (F17). |
| **miri** | An interpreter detecting UB (*Undefined Behavior*); relevant given the `unsafe` iterators; absent from CI (F17). |
| **proptest / quickcheck / fuzzing** | Property-based / generative / random-input testing. **Entirely absent** (F11). |
| **`cargo audit` / `cargo deny`** | Vulnerability audit / dependency policies. Absent from CI (F19). |
| **bincode / serde / tokio / parking_lot / arrayvec / ipnet / range-cmp / chrono / rand / once_cell / tracing** | Dependencies: binary serialization; (de)serialization; async runtime; non-poisoning locks; `ArrayVec` (inline vector, B-tree nodes); network/CIDR types; key↔range comparison (`RangeOrdering`); `DateTime<Utc>` (LWW timestamps); randomness; lazy init; structured logs. |
| **`Arc` / `RwLock` / `unwrap` / `panic=abort` / `overflow-checks`** | Atomic shared pointer; reader-writer lock; panicking unwrap; panic strategy; arithmetic-overflow checking (disabled in release → F7). |
| **`ExactSizeIterator` / `FusedIterator` / `DoubleEndedIterator`** | Rust iterator traits targeted by issues #90-92 (full RSOS contract, §2.4). |

---

## 4. Bibliography

**Set reconciliation**
- A. Meyer, *Range-Based Set Reconciliation*, arXiv:2212.13567 (IEEE SRDS 2023) — https://arxiv.org/abs/2212.13567 ; primer: https://logperiodic.com/rbsr.html
- L. Yang, Y. Gilad, M. Alizadeh, *Practical Rateless Set Reconciliation*, SIGCOMM 2024, arXiv:2402.02668 — https://arxiv.org/abs/2402.02668 ; impl. https://github.com/yangl1996/riblt
- minisketch (Bitcoin Core) — https://github.com/bitcoin-core/minisketch ; BIP 330 — https://bips.dev/330/
- Erlay (Naumenko et al., CCS 2019) — https://arxiv.org/abs/1905.10518
- E. G. Amparore, *RBSR via Range-Summarizable Order-Statistics Stores* (RSOS / AELMDB), arXiv:2603.19820 (2026) — https://arxiv.org/html/2603.19820
- *CertainSync: Rateless Set Reconciliation with Certainty*, arXiv:2504.08314 (SIGMETRICS 2025) — https://arxiv.org/abs/2504.08314

**Merkle / anti-entropy structures**
- A. Auvolat, F. Taïani, *Merkle Search Trees*, SRDS 2019 — https://inria.hal.science/hal-02303490 ; crate https://github.com/domodwyer/merkle-search-tree ; Bluesky/atproto usage — https://atproto.com/specs/repository
- Prolly trees (Dolt/Noms) — https://docs.dolthub.com/architecture/storage-engine/prolly-tree ; https://www.dolthub.com/blog/2025-06-03-people-keep-inventing-prolly-trees/
- J. Gustafson, *Merklizing the key/value store* (Merkle radix / SMT) — https://joelgustafson.com/posts/2023-05-04/merklizing-the-key-value-store-for-fun-and-profit/
- Merkle-CRDTs, arXiv:2004.00107 — https://arxiv.org/abs/2004.00107
- Dynamo (DeCandia et al., SOSP 2007) — https://www.allthingsdistributed.com/files/amazon-dynamo-sosp2007.pdf
- Cassandra repair / over-streaming — https://www.pythian.com/blog/effective-anti-entropy-repair-cassandra
- Willow 3d-RBSR (fingerprint security) — https://willowprotocol.org/specs/3d-range-based-set-reconciliation/index.html ; Negentropy — https://github.com/hoytech/negentropy
- Demers et al., *Epidemic Algorithms*, PODC 1987 ; SWIM — https://www.cs.cornell.edu/projects/Quicksilver/public_pdfs/SWIM.pdf ; memberlist — https://github.com/hashicorp/memberlist

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
> is defined: [3.1 repo](#g91) · [3.2 structures/algos](#g92) · [3.3 distributed](#g93) ·
> [3.4 crypto/network](#g94) · [3.5 complexity](#g95) · [3.6 Rust](#g96).

**A** — AEAD [3.4](#g94) · AELMDB [3.2](#g92) · `ACTIVITY_TIMEOUT` [3.1](#g91) · `add_pre_insert` [3.1](#g91) · amplification [3.4](#g94) · anti-entropy [3.3](#g93) · `Arc` [3.6](#g96) · `ArrayVec` [3.6](#g96) · associative [3.3](#g93)

**B** — B-tree / B+-tree [3.5](#g95) · `B` (constant) [3.1](#g91) · BCH codes [3.2](#g92) · Berlekamp-Massey [3.2](#g92) · bincode [3.6](#g96) · bincode allocation bomb [3.4](#g94) · BIP 330 [3.2](#g92) · birthday bound [3.4](#g94) · BLAKE3 [3.4](#g94) · Bloom filter [3.2](#g92) · `BUFFER_SIZE` [3.1](#g91)

**C** — CAP [3.3](#g93) · CAS [3.2](#g92) · Cassandra [3.2](#g92) · causal consistency / causal+ [3.3](#g93) · causal stability [3.3](#g93) · CDC (content-defined chunking) [3.2](#g92) · CertainSync [3.2](#g92) · `check_invariants` [3.1](#g91) · chrono [3.6](#g96) · CID [3.2](#g92) · clippy [3.6](#g96) · clock skew [3.3](#g93) · CmRDT [3.3](#g93) · collision [3.4](#g94) · commit-wait [3.3](#g93) · commutative [3.3](#g93) · `ComparisonItem` [3.1](#g91) · `Config` [3.1](#g91) · content-addressing [3.2](#g92) · CPI / CPISync [3.2](#g92) · CRDT [3.3](#g93) · CvRDT [3.3](#g93)

**D** — datagram [3.4](#g94) · `DateTime<Utc>` [3.6](#g96) · `DEFAULT_TIMEOUT` [3.1](#g91) · `DefaultHasher` [3.4](#g94) · `Diffable` [3.1](#g91) · `diff_round` [3.1](#g91) · `DiffRange` [3.1](#g91) · Dolt / DoltHub [3.2](#g92) · `DoubleEndedIterator` [3.6](#g96) · DRDoS [3.4](#g94) · DTLS [3.4](#g94) · DVV (Dotted Version Vector) [3.3](#g93) · Dynamo [3.2](#g92)

**E** — Earthstar [3.2](#g92) · epidemic [3.3](#g93) · Erlay [3.2](#g92) · eventual consistency [3.3](#g93) · `ExactSizeIterator` [3.6](#g96)

**F** — fan-out [3.5](#g95) · fingerprint [3.1](#g91) · `FusedIterator` [3.6](#g96) · fuzzing [3.6](#g96)

**G** — `gc_grace_seconds` [3.3](#g93) · `gen_ip` [3.1](#g91) · `get_mut` [3.1](#g91) · `get_range` [3.1](#g91) · GF(2)-linear [3.4](#g94) · gossip [3.3](#g93) · Graphene [3.2](#g92)

**H** — `hash(key,value)` [3.1](#g91) · `HashRangeQueryable` [3.1](#g91) · `HashSegment` [3.1](#g91) · happens-before [3.3](#g93) · Hazelcast [§2/product](#g92) · hinted handoff [3.3](#g93) · history-independence [3.2](#g92) · HLC (Hybrid Logical Clock) [3.3](#g93) · HMAC [3.4](#g94) · homomorphic hash [3.4](#g94) · HRTree [3.1](#g91) · HyParView [3.3](#g93)

**I** — IBLT [3.2](#g92) · idempotent [3.3](#g93) · incremental hash [3.4](#g94) · `insert` / `insert_bulk` [3.1](#g91) · `insertion_position` [3.1](#g91) · ipnet [3.6](#g96) · iroh / iroh-docs [3.2](#g92)

**J** — join-semilattice [3.3](#g93) · `just_insert` / `just_remove` [3.1](#g91)

**K** — `key_at` [3.1](#g91)

**L** — Lamport clock [3.3](#g93) · leading-zeros (attack) [3.2](#g92) · LtHash [3.4](#g94) · LWW (Last-Write-Wins) [3.3](#g93) · LWW-Register [3.3](#g93)

**M** — MAC [3.4](#g94) · `MAX_CAPACITY` / `MIN_CAPACITY` [3.1](#g91) · `MAX_SENDTO_RETRIES` [3.1](#g91) · memberlist [3.3](#g93) · Merkle-CRDT [3.2](#g92) · Merkle-DAG [3.2](#g92) · Merkle radix / Patricia [3.2](#g92) · Merkle tree / root [3.2](#g92) · `Message` [3.1](#g91) · minisketch [3.2](#g92) · miri [3.6](#g96) · monoid [3.5](#g95) · monotone [3.3](#g93) · MSet-Mu-Hash / MSet-XOR-Hash [3.4](#g94) · MSRV [3.6](#g96) · MST (Merkle Search Tree) [3.2](#g92) · MTU [3.4](#g94) · MV-Register [3.3](#g93)

**N** — *n / d / U / b* (notation) [3.5](#g95) · Negentropy [3.2](#g92) · Noise [3.4](#g94) · Noms [3.2](#g92) · NTP [3.3](#g93)

**O** — O(log n) / O(d log n) [3.5](#g95) · once_cell [3.6](#g96) · order statistics (rank/select) [3.5](#g95) · OR-Set [3.3](#g93) · over-streaming [3.2](#g92)

**P** — PACELC [3.3](#g93) · `panic=abort` [3.6](#g96) · parking_lot [3.6](#g96) · partition [3.3](#g93) · Patricia trie [3.2](#g92) · `peer_net` / `peers` [3.1](#g91) · `PEER_EXPIRATION` [3.1](#g91) · Pekko Distributed Data [§2/product](#g92) · PinSketch [3.2](#g92) · `pop_expired` [3.1](#g91) · `position` [3.1](#g91) · `pre_insert` [3.1](#g91) · prolly tree [3.2](#g92) · proptest [3.6](#g96) · PTP [3.3](#g93) · push / pull [3.3](#g93)

**Q** — QUIC [3.4](#g94) · quickcheck [3.6](#g96) · quorum [3.3](#g93)

**R** — rand [3.6](#g96) · range-cmp / `RangeOrdering` [3.6](#g96) · rank / select [3.5](#g95) · Rateless IBLT (RIBLT) [3.2](#g92) · RBSR [3.2](#g92) · read repair [3.3](#g93) · `rebalance_after_deletion` [3.1](#g91) · `Reconcilable` / `reconcile()` [3.1](#g91) · `ReconcileEngine` [3.1](#g91) · `ReconcileStore` [3.1](#g91) · `refresh_hash_size` [3.1](#g91) · `remove` / `remove_bulk` [3.1](#g91) · resurrection / zombie [3.3](#g93) · Riak [3.2](#g92) · rolling hash [3.2](#g92) · rumor mongering [3.3](#g93) · `RwLock` [3.6](#g96) · RSOS [3.2](#g92)

**S** — ScyllaDB [3.2](#g92) · second-preimage [3.4](#g94) · SEC (Strong Eventual Consistency) [3.3](#g93) · serde [3.6](#g96) · session guarantees [3.3](#g93) · SipHash [3.4](#g94) · SMT (Sparse Merkle Tree) [3.2](#g92) · spoofing [3.4](#g94) · split-brain [3.3](#g93) · `start_diff` [3.1](#g91) · Strata Estimator [3.2](#g92) · structural sharing [3.2](#g92) · SWIM [3.3](#g93)

**T** — Thomas write rule [3.3](#g93) · `TimeoutWheel` [3.1](#g91) · TLS [3.4](#g94) · tokio [3.6](#g96) · tombstone [3.1](#g91) · `TOMBSTONE_CLEARING` [3.1](#g91) · tracing [3.6](#g96) · transitive group [3.4](#g94) · `tree_hash` [3.1](#g91) · `tree_size` [3.1](#g91) · TrueTime [3.3](#g93)

**U** — UDP [3.4](#g94) · `unwrap` [3.6](#g96) · `Update` [3.1](#g91) · `overflow-checks` [3.6](#g96)

**V** — vector clock / version vector [3.3](#g93) · Vivaldi [3.3](#g93) · Voldemort [3.2](#g92)

**W** — Willow [3.2](#g92) · `with_mut` [3.1](#g91) · `with_seed` / `with_port` / `with_peer_net` / `with_tombstone_timeout` [3.1](#g91) · Writes-Follow-Reads [3.3](#g93)

**X** — XOR [3.4](#g94) · xxHash [3.4](#g94)

---

*The state-of-the-art positioning was produced by a literature survey across four themes
(set-reconciliation algorithms, diffable/Merkle structures, consistency & conflict resolution, and
the Rust ecosystem), every claim backed by a cited source. The accompanying code-audit findings,
and their resolution status, live in [`PROGRESS.md`](./PROGRESS.md).*

# reconcile-rs — Architecture

## Status & scope

`reconcile-rs` is a reconciliation service that keeps a key-value map synchronised across several
instances. This document describes the **current architecture** and the **target architecture**
(hexagonal — ports & adapters). Correctness and security properties are tracked in
[`PROGRESS.md`](./PROGRESS.md) and are assumed here as the baseline; the state-of-the-art
positioning is in [`SOTA.md`](./SOTA.md).

The crate is published on crates.io as `reconcile` **0.2.1**. The public API and the on-wire /
on-disk formats are not yet stable — breaking changes ride a minor-version bump (next: 0.3.0,
see `CHANGELOG.md`). Code locations are given as `file:line` against the current tree.

---

## 1. System overview

A node holds an ordered key-value map and gossips changes to its peers so that all replicas
converge. The design rests on five mechanisms:

- **Storage** — a Hash-Range Tree (`HRTree`): an ordered map that also maintains, for every subtree,
  a **range fingerprint** so the hash of any key interval is available in `O(log n)`.
- **Anti-entropy protocol** — two peers compare fingerprints over shrinking key ranges (`diff`) and
  exchange only the divergent entries. Equality and emptiness are decided by interval **size**, not
  by hash, to stay collision-safe.
- **Causality & conflict resolution** — each value is stamped with a Hybrid Logical Clock timestamp (`Timestamp`);
  conflicts resolve by **last-write-wins** over the HLC total order `(wall_ms, counter, node_id)`.
- **Deletion** — removals are **tombstones**; they are garbage-collected only once causally stable
  (every monotonic cluster member has acknowledged the exact version), which prevents resurrection.
- **Transport & security** — messages travel as authenticated UDP datagrams (per-datagram MAC,
  verified before deserialisation). Persistence to disk is optional.

---

## 2. Current architecture

### 2.1 Modules

| Module | Responsibility |
|---|---|
| `hrtree.rs`, `hrtree_iter.rs` | Ordered map + range-fingerprint data structure and its iterators. |
| `fingerprint.rs` | 256-bit additive fingerprint (`[u64; 4]`, per-element BLAKE3, add/sub mod 2²⁵⁶). |
| `diff.rs` | Anti-entropy algorithm (`start_diff`, `diff_round`) and its wire types. |
| `reconcilable.rs` | Value/tombstone semantics and the conflict-resolution policy. |
| `clock.rs` | Hybrid Logical Clock: timestamp type, ordering, and the clock that mints/observes stamps. |
| `auth.rs` | Per-datagram message authentication (MAC). |
| `persistence.rs` | Durability boundary: load/save a snapshot of the dated map. |
| `reconcile_engine.rs` | Network orchestration: UDP socket, (de)serialisation, peer discovery, gossip. |
| `reconcile_store.rs` | Public facade tying the engine, the map, and timeouts together. |
| `timeout_wheel.rs` | Acknowledgement timeout tracking. |

### 2.2 The domain mechanism is already infrastructure-free

The data structure and the protocol algorithm carry **no infrastructure dependency** — they import
no async runtime, no socket, no codec, no wall clock (outside `#[cfg(test)]`):

| Module | Infrastructure imported |
|---|---|
| `hrtree.rs`, `hrtree_iter.rs`, `fingerprint.rs`, `diff.rs`, `reconcilable.rs` | none |

This is, in effect, the interior of the hexagon and it exists today.

### 2.3 Infrastructure coupling

The infrastructure dependencies are concentrated in two places:

| Module | Infrastructure imported directly | `file:line` |
|---|---|---|
| `clock.rs` | `chrono::Utc` — physical time read inside the domain | `clock.rs:35` |
| `reconcile_engine.rs` | `tokio::net::UdpSocket`, `tokio::time`, `bincode`, `rand::StdRng`, `ipnet` | `reconcile_engine.rs:21-28,67` |
| `reconcile_store.rs` | `chrono`, `ipnet` | `reconcile_store.rs:19-20` |

The reconciliation engine therefore mixes three concerns — transport (UDP), encoding (bincode), and
the gossip orchestration — in a single module, with the data structure and protocol reached
underneath. There is no abstraction boundary between the domain and the transport, the codec, or the
time source: the domain cannot be exercised without real sockets and real wall-clock time, and none
of the three can be substituted.

### 2.4 Trait landscape

Seven traits exist. They fall into three groups:

- **Boundary abstraction (1).** `Persistence` (`persistence.rs:77`) is a genuine port: it has two
  implementations (in-memory and file snapshot) and abstracts durability behind a small,
  intention-revealing contract.
- **Internal mechanism, currently public (2).** `HashRangeQueryable` (`diff.rs:30`) and `Diffable`
  (`diff.rs:58`) describe *how* the diff is computed over the tree. `Diffable` is a blanket impl
  whose associated types are always the same concrete types; `HashRangeQueryable` has a single
  real implementation (`HRTree`). They are exposed through `pub mod diff` (`lib.rs:30`), placing the
  protocol mechanism on the crate's public surface.
- **Value-shape helpers (4).** `MaybeTombstone` (`reconcilable.rs:43`), `Reconcilable`
  (`reconcilable.rs:27`) and `Timestamped` (`clock.rs:171`) each carry a single implementation over a
  tuple — the stored cell is represented as `(Timestamp, Option<V>)`. `Mac` (`auth.rs:92`) selects a MAC
  backend chosen at compile time.

Two consequences of the value-shape representation:

- The internal tuple `(Timestamp, Option<V>)` leaks onto the public surface — into the
  `add_pre_insert` hook (`reconcile_store.rs:171`) and into `PersistedState` (`persistence.rs:51`).
- The generic constraints are spelled out in full at every implementation site (a nine-bound key
  constraint and an eleven-bound value constraint, e.g. `reconcile_engine.rs:122-135`,
  `reconcile_store.rs:75`), with the entry-semantics bounds attached to the *value* parameter rather
  than to the entry.

---

## 3. Target architecture: ports & adapters

### 3.1 Principle

The target is a **hexagonal** architecture. The domain — storage, protocol, causality, conflict
resolution, tombstone lifecycle — depends only on a small set of **ports** (traits) that it defines
itself. **Adapters** implement those ports against concrete infrastructure. All dependency arrows
point **inward**: adapters depend on the domain, never the reverse.

Two rules follow:

1. **Ports reveal intent and are public.** A port names a capability the domain needs from the
   outside world (a clock, a transport, a codec, durability). It is part of the crate's contract.
2. **Mechanism stays internal.** How a diff round is computed, or how a range hash is queried, is an
   implementation detail of the domain and is not exposed.

### 3.2 The hexagon

```
                         ┌─────────────────────── adapters (infrastructure) ──────────────┐
   driving side          │                                                                │
   (application)         │   HlcClock        UdpTransport      BincodeCodec    FileSnapshot│
        │                │  (system time)   (tokio / UDP)      (bincode)     / InMemory     │
        ▼                │       │               │                 │              │         │
  ┌───────────┐  impl    └───────┼───────────────┼─────────────────┼──────────────┼─────────┘
  │  Store    │           ports: │ Clock         │ Transport       │ Codec        │ Persistence
  │ (facade)  │◀──────────────── ▼ ───────────── ▼ ─────────────── ▼ ──────────── ▼ ────────
  └───────────┘                 ┌──────────────────────────────────────────────────────────┐
        ▲                       │                    DOMAIN  (hexagon interior)              │
        │  driving port         │  anti-entropy algorithm · conflict policy (LWW)            │
        └───────────────────────│  tombstone lifecycle · HRTree + Fingerprint (mechanism)    │
                                │  Timestamp · Entry / State (value types)  —  no tokio / bincode  │
                                └──────────────────────────────────────────────────────────┘
```

### 3.3 Domain core

The interior contains, with no infrastructure dependency:

- the anti-entropy algorithm (`start_diff` / `diff_round`),
- the conflict-resolution policy (last-write-wins over the HLC order),
- the tombstone lifecycle and the causal-stability garbage-collection rule,
- the `HRTree` and `Fingerprint` (the storage and range-hash mechanism),
- the value types `Timestamp`, `Entry`, `State`.

### 3.4 Ports

Four outbound ports, each removing one concrete infrastructure dependency from the domain:

```rust
// Clock — abstracts the time source (replaces the direct chrono::Utc read).
// The HLC algorithm stays in the domain; only physical time crosses the boundary.
// The port returns the concrete `Timestamp` rather than a generic associated type: it is the
// only stamp in use, alternate causality schemes (vector clocks / CRDT) are out of scope
// (see clock.rs), and the tombstone wheel, version_hash and the serde format are already
// coupled to its shape — a generic timestamp would leak that shape while adding a type
// parameter to the engine, store and Config. Only the physical-time read crosses the boundary.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Timestamp;           // mint a strictly-monotonic local stamp
    fn observe(&self, remote: Timestamp);  // advance past a peer's stamp (causality)
}

// Transport — abstracts datagram I/O (replaces tokio::net::UdpSocket).
#[async_trait::async_trait]
pub trait Transport: Send + Sync + 'static {
    type Addr: Clone + Eq + Hash + Send + Sync;
    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, Self::Addr)>;
    async fn send_to(&self, buf: &[u8], dst: &Self::Addr) -> io::Result<usize>;
    fn local_addr(&self) -> io::Result<Self::Addr>;
}

// Codec — abstracts wire encoding (replaces bincode). Authentication wraps it externally.
// decode_stream carries a max_items cap so a single datagram cannot be expanded into an
// unbounded number of messages, and the BincodeCodec adapter is built with `.with_limit`
// so a crafted length prefix cannot pre-allocate a huge buffer (closes the allocation-bomb
// hazard, issue #151).
pub trait Codec: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    fn encode<T: Serialize>(&self, value: &T, out: &mut Vec<u8>) -> Result<(), Self::Error>;
    fn decode_stream<T: DeserializeOwned>(
        &self,
        bytes: &[u8],
        max_items: usize,
    ) -> Result<Vec<T>, Self::Error>;
}

// Persistence — durability boundary (already present, the model the others follow).
pub trait Persistence<K, V>: Send + Sync + 'static {
    fn load(&self) -> io::Result<Option<PersistedState<K, V>>>;
    fn save(&self, state: &PersistedState<K, V>) -> io::Result<()>;
}

// Discovery — the single source of peer addresses. The default `RandomProbe` adapter (speculative,
// one random address per declared network each round) and the `DnsDiscovery` adapter (authoritative,
// k8s headless-Service DNS) both implement it. `is_authoritative` distinguishes the two: a speculative
// result only steers the current round's targets, an authoritative one is the current truth (seeded
// into the known-peer set, an absence decommissions after a grace period). Either way it never grants
// causal-stability membership (which a peer must earn via an authenticated dated datagram), so it
// cannot affect tombstone GC correctness. Boxed-future rather than async_trait to keep the dependency
// footprint unchanged (it is always behind Arc<dyn ..>).
pub trait Discovery: Send + Sync + 'static {
    fn discover(&self) -> DiscoverFuture<'_>;       // io::Result<Vec<IpAddr>>
    fn is_authoritative(&self) -> bool { true }     // RandomProbe overrides to false
}
```

### 3.5 Adapters

| Port | Default adapter | Backed by |
|---|---|---|
| `Clock` | `HlcClock` (its `now`/`observe`, `clock.rs:121,146`, already match the port) | system time |
| `Transport` | `UdpTransport(Arc<UdpSocket>)` | tokio / UDP |
| `Codec` | `BincodeCodec(DefaultOptions)` | bincode |
| `Persistence` | `FileSnapshot`, `InMemory` | file / memory |
| `Discovery` | `RandomProbe` (default, speculative per-net probing); `DnsDiscovery` (authoritative, headless-Service DNS) | `gen_ip` / `tokio::net::lookup_host` |

Message authentication (`Authenticator` / MAC) sits **ahead of** the codec: the MAC is verified on
raw bytes before any decoding occurs. It is never folded into the `Codec`.

The ports make the domain testable in isolation: an in-memory `Transport` (optionally lossy and
reordering) and a fixed `Clock` make convergence and HLC behaviour deterministic without real
sockets or wall-clock time.

### 3.6 Domain types and conflict policy

A single, intention-revealing type represents a stored cell, replacing the `(Timestamp, Option<V>)` tuple
and the three value-shape helper traits:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Entry<T, V> { pub stamp: T, pub state: State<V> }

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum State<V> { Present(V), Tombstone }

impl<T: Ord + Copy, V: Clone> Entry<T, V> {
    pub fn is_tombstone(&self) -> bool { matches!(self.state, State::Tombstone) }
    pub fn value(&self) -> Option<&V> { /* … */ }
    pub fn merge(&self, other: &Self) -> Self {        // last-write-wins (strict >)
        if other.stamp > self.stamp { other.clone() } else { self.clone() }
    }
}
```

`Entry` carries the tombstone, timestamp, and merge semantics as a concrete domain type;
`add_pre_insert` and `PersistedState` take `Entry<…>` rather than the bare tuple. Conflict
resolution is **domain policy**, not an infrastructure port: last-write-wins is the concrete
default. A pluggable `Resolve` seam is warranted only if a second policy (e.g. a CRDT) becomes a
real requirement.

> **Superseded in part by §7 D6.** That open-ended condition is now three named triggers, none of
> which has fired, and the shape to build if one does is a concrete facade rather than a public type
> parameter. §7 D5 also records why `Value: PartialEq` — which existed only to support the merge
> call site — is dropped, and what would require reinstating it.

The same step also absorbs the **value-only projection** that powers the dateless `ReconcileMirror`.
Today the engine keeps a second tree `HRTree<K, V::Projected>` fed through the `Projectable` trait
into a `ValueOnly<V>(Option<V>)` cell whose `Hash` is timestamp-less by construction. `State<V>` is
isomorphic to that cell (`Present(v) ↔ Some(v)`, `Tombstone ↔ None`), so the projection becomes
`Entry::project(&self) -> State<V>` (= `self.state.clone()`) and the projection tree becomes
`HRTree<K, State<V>>`; the `Projectable` trait and `ValueOnly` type are dissolved. The two hashes
must stay distinct — the dated `Entry` hashes **with** its stamp (for `version_hash`), the projected
`State<V>` hashes the value **alone** — which is invariant 8 in §5. Because a `ValueOnly(Some(v))`
and a `State::Present(v)` do not encode to the same bytes, this step also breaks the value-only wire
format, alongside the dated wire and on-disk formats (acceptable while the formats are unstable).

### 3.7 Internal mechanism

The anti-entropy mechanism is not a port. `HashRangeQueryable` and `Diffable` are removed as traits:
range-hash querying becomes inherent methods on the concrete `HRTree`, and `start_diff` / `diff_round`
become `pub(crate)` functions over it. The wire types `HashSegment` / `DiffRange` are `pub(crate)`.
None of this appears in the public surface.

> **Amended by §7 D1.** This continues to hold for the **anti-entropy protocol** — `start_diff`,
> `diff_round`, `HashSegment` and `DiffRange` stay `pub(crate)`. It no longer holds for the **tree**,
> which graduates to a standalone crate (an *RSOS*: an order-statistic B-tree with a group-valued
> range measure) with its own published contract, consumed here as an external dependency. This also
> resolves the apparent conflict with SOTA.md §2.4 P1-4 / P1-5: a generic measure and public
> `rank`/`select` are right *for the tree crate* and wrong *for the wire-visible summary here*.

### 3.8 Generic bounds

The repeated multi-bound constraints are expressed once, as supertrait bundles with blanket impls,
so implementation sites read `impl<K: Key, V: Value>`:

```rust
pub trait Key:   Clone + Debug + Hash + Ord + Send + Sync + Serialize + DeserializeOwned + 'static {}
impl<T> Key for T where T: /* same */ {}

pub trait Value: Clone + Debug + Hash + PartialEq + Send + Sync + Serialize + DeserializeOwned + 'static {}
impl<T> Value for T where T: /* same */ {}
```

Entry semantics travel with `Entry`, not with the `V` bound.

### 3.9 Crate structure

The layers are separated into a workspace so that the inward dependency direction is enforced by the
compiler:

```
reconcile-core   // DOMAIN + PORTS: Clock, Persistence (traits);
                 //   Entry / State, Timestamp, Fingerprint, HRTree, the diff algorithm, LWW.
                 //   no infrastructure deps (no tokio, bincode, chrono-IO, ipnet, runtime rand);
                 //   serde + blake3, plus the dependency-light tracing / metrics facades.
reconcile-net    // ADAPTERS + I/O PORTS: Transport, Codec (traits); UdpTransport, BincodeCodec,
                 //   Authenticator, peer discovery (gen_ip / ipnet / rand).
reconcile-store  // ADAPTERS: FileSnapshot / InMemory persistence.
reconcile        // WIRING + driving API: the Store facade, configuration, the HlcClock adapter
                 //   (which holds the chrono read), the tombstone wheel.
```

The `Clock` and `Persistence` ports are defined in `reconcile-core` (the domain-adjacent logic
injects them). The **`Transport` and `Codec` ports are defined in `reconcile-net`**, not the core:
they are consumed by the UDP driver, which lives in `reconcile-net`, and the diff/merge domain does
no I/O — this also keeps `async_trait` out of the core. The chrono-reading `HlcClock` adapter lives
in `reconcile` (the `Timestamp` *type* and the pure HLC advance stay in the core, so the core carries no
`chrono`). Adapters depend on `reconcile-core`; infrastructure cannot be imported into the core
without a compile error — the guarantee a single crate cannot provide.

---

## 4. Current → target mapping

| Current | Target |
|---|---|
| `(Timestamp, Option<V>)` tuple | `Entry<Timestamp, V>` + `State<V>` domain type |
| `MaybeTombstone`, `Timestamped` | inherent `Entry` methods / field access |
| `Reconcilable` (LWW over tuple) | `Entry::merge` (LWW), optional `Resolve` policy seam |
| `Projectable` / `ValueOnly<V>` (dateless mirror) | `Entry::project` → `State<V>` (the projection cell *is* `State`) |
| `HashRangeQueryable`, `Diffable` (public) | inherent `HRTree` methods + `pub(crate)` diff functions |
| `pub mod diff` exposing wire types | `pub(crate)` `HashSegment` / `DiffRange` |
| `chrono::Utc` read in `clock.rs` | `Clock` port; `HlcClock` adapter holds the time read |
| `UdpSocket` in `reconcile_engine.rs` | `Transport` port; `UdpTransport` adapter |
| `bincode` in `reconcile_engine.rs` | `Codec` port; `BincodeCodec` adapter |
| `Persistence` | unchanged (the model port) |
| `gen_ip` random IP-scan inline in the engine | `Discovery` port; `RandomProbe` (speculative, the default) + `DnsDiscovery` (authoritative, k8s-native) adapters |
| Multi-bound `where` blocks | `Key` / `Value` supertrait bundles |
| Single crate | `reconcile-core` / `-net` / `-store` / `reconcile` workspace |

---

## 5. Invariants

The following load-bearing properties are preserved across any restructuring; they encode the
correctness and security guarantees tracked in [`PROGRESS.md`](./PROGRESS.md).

1. **Fingerprint format & arithmetic** — `[u64; 4]`, per-element BLAKE3, add/sub mod 2²⁵⁶; the
   golden vectors in `fingerprint.rs` hold.
2. **HLC total order** `(wall_ms, counter, node_id)` (`clock.rs:44-54`) — the derived ordering *is* the
   conflict order; the `Clock` port mints `Timestamp` directly, preserving it, and merge uses strict `>`.
3. **Size-not-hash emptiness/equality** in `diff_round` (`diff.rs:135-141`).
4. **Malformed-bound / inverted-range hardening** in `diff_round` (`diff.rs:100-134`).
5. **Authenticate before deserialise** — the MAC is verified on raw bytes before the codec runs; the
   `Codec` port never absorbs authentication.
6. **Causal-stability tombstone gate** — a tombstone is garbage-collected only after every monotonic
   cluster member has acknowledged the exact version hash. Dynamic discovery (e.g. `DnsDiscovery`)
   only ever feeds the gossip-target `peers` set, **never** the `members` set: membership stays
   earned by an authenticated dated datagram, so a discovered (unverified) address can neither block
   GC nor be the subject of a GC release. Decommissioning a member that has vanished from discovery
   uses the same `decommission_peer` escape hatch as `forget_peer`.
7. **`version_hash` determinism** (`reconcile_engine.rs:55`) — preserved as `Entry` derives `Hash`.
8. **Value-only projection hash is timestamp-less** — the dated `Entry` hashes with its `stamp`
   (feeding `version_hash`), while its `State<V>` projection hashes the value alone, so a dated
   store and a dateless `ReconcileMirror` compute identical per-element fingerprints. Guarded by
   `mirror.rs::value_fingerprint_is_timestamp_independent`.
9. **A single protocol message must fit one datagram** — the send path packs messages into
   datagrams but never *fragments* one. An `Update` whose encoding exceeds
   `BUFFER_SIZE - authenticator.overhead()` is therefore never delivered, and the key never
   converges. Until chunking exists this is a hard ceiling on value size, not a soft one; it must be
   documented and must fail loudly rather than silently (see §7 D3, D6).

---

## 6. Migration sequence

The path from the current structure to the target is ordered by dependency. Each step is
behaviour-preserving and verified by the existing test suite, except the `Entry` step, which changes
the wire and on-disk formats (acceptable while the formats are unstable).

1. **Bound bundles & encapsulation** — introduce `Key` / `Value`; demote the protocol mechanism and
   other internals from `pub` to `pub(crate)`.
2. **Dissolve the diff traits** — remove `HashRangeQueryable` / `Diffable`; move `start_diff` /
   `diff_round` verbatim to `pub(crate)` functions over `HRTree`.
3. **`Clock` port** — extract `Clock`; `HlcClock` becomes its adapter and holds the physical-time
   read; the domain becomes time-source-free.
4. **`Entry` / `State` domain type** — replace `(Timestamp, Option<V>)`; dissolve `MaybeTombstone` /
   `Timestamped`; fold `Reconcilable` into `Entry::merge`; update the public hook and
   `PersistedState`. *Changes the wire and on-disk formats.*
5. **`Transport` & `Codec` ports** — extract both; `UdpTransport` / `BincodeCodec` adapters; the
   engine becomes a thin driver over the ports, with authentication ahead of the codec.
6. **Workspace split** — promote the layers to `reconcile-core` / `reconcile-net` /
   `reconcile-store` / `reconcile`; ports defined in the core; the core carries no infrastructure
   dependency.

Steps 1–3 are independent of the format change; step 4 is the single format-breaking step; steps 5–6
complete the boundary extraction and enforce it at the crate level.

---

## 7. Decision ledger

Product and contract decisions taken deliberately, with the reasoning that produced them, so they are
not relitigated. Each entry states the decision, why, and what would overturn it. Where a decision
supersedes text earlier in this document, the earlier section points here.

**Recorded:** 2026-08-03. Decisions D1-D5 concern the public contract, D6-D9 scope, D10-D12 release.

### D1 — `HRTree` becomes its own product, correctly named

**Decision.** The tree graduates to a standalone crate with its own public contract, rather than
staying an internal mechanism. It is renamed away from "Hash-Range Tree".

**What it actually is.** Not a Merkle tree (see §2.3 differentiator 1). Precisely: a **B-tree**,
augmented with **order statistics** (per-subtree size ⇒ `rank`/`select` in O(log n)), augmented with
a **composable range measure** (per-subtree summary ⇒ `Aggregate(l,u)` in O(log n)). The general
technique is the *monoidally-annotated* or **measured** tree (Hinze & Paterson, finger trees, JFP
2006, where the annotation is literally a *measure*); here the measure is the product
`(Fingerprint, +) × (usize, +)`. The summary itself is an **incremental multiset hash** (Clarke et
al., ASIACRYPT 2003) and is an abelian **group**, not merely a monoid — it has inverses, which is
what makes range subtraction and O(log n) incremental maintenance possible. The most specific
literature name for this exact contract is **RSOS — Range-Summarizable Order-Statistics Store**
(Amparore, arXiv:2603.19820), whose operations are `size / Aggregate(l,u) / Rank(z) / Select(r) /
Enumerate(l,u) / Insert / Delete`. The tree satisfies all of them today.

> In one sentence: **an order-statistic B-tree with a group-valued range measure — an RSOS whose
> measure is an incremental multiset hash.**

**Why rename.** `HRTree` / "Hash-Range Tree" reads as "hash tree" ≡ Merkle tree, which is precisely
the association the design does *not* want: the crate's deepest structural advantage is that it
diffs *value-defined ranges* rather than node identity, and therefore needs no history-independence
(§2.3). A descriptive name with the literature term in the documentation is preferred over the bare
acronym, since "RSOS" is not yet established vocabulary.

**Consequence — this resolves the §2.4 monoid tension.** SOTA.md §2.4 P1-4 (`HRTree<XOR>` →
`RSOS<M: Monoid>`) and P1-5 (expose `rank`/`select`) are **correct for the tree crate** and **wrong
for `reconcile-rs`**:

- *In the tree crate:* a generic measure is the whole point — fingerprints, but also sum/min/max/
  count and sketches over ranges. The RSOS contract is the product's public API.
- *In `reconcile-rs`:* the summary is **wire-visible** (`HashSegment.hash`). A generic measure would
  propagate to `Message` and `ReconcileStore` as a type parameter, and two nodes configured with
  different measures would never converge **and never notice** — they would exchange structurally
  valid segments whose fingerprints never match, refining forever. That is strictly worse than a hard
  error. `reconcile-rs` therefore instantiates exactly one concrete measure and keeps invariant 1.

**Consequence — §3.7 is amended, not overruled.** "Mechanism stays internal" continues to hold for
the **anti-entropy protocol** (`start_diff` / `diff_round` / `HashSegment` stay `pub(crate)`). It no
longer holds for the **tree**, which becomes an external dependency with a published contract.

**Sequencing.** Lands with step 6 (the workspace split), as a peer crate.

### D2 — `Transport` is consumer-wireable; `Codec` is not

**Decision.** Add a `Transport` injection point to the public API and un-gate the in-memory
transport. Demote `Codec` and `BincodeCodec` to `pub(crate)`. Leave `Clock` test-gated.

**Why the two ports split.** `Transport` is object-safe and already held as
`Arc<dyn Transport<Addr = SocketAddr>>`, so a `Persistence`-style injection point costs **zero type
parameters**. `Codec` has generic methods (`encode<T>` / `decode_stream<T>`), is not object-safe, and
is carried as a type parameter — exposing it would require a type-changing builder
(`-> ReconcileStore<K, V, C2>`).

**Why `Transport` earns it.** Two concrete uses: (a) a QUIC datagram transport (RFC 9221 unreliable
datagrams fit this trait's shape) is one of the options for larger-than-datagram support; (b) lossy /
reordering / delaying transports for convergence testing under adversity, which §2.4 P3-8 names as
the category standard and which is otherwise impossible. Users also need the in-memory transport to
test their own applications deterministically, which is why it is un-gated.

**Why `Codec` does not.** Its plausible uses — compression and cross-language interop — are not
satisfied by swapping the trait alone. Compression interacts with the authenticate-before-decode
ordering (invariant 5) and with the datagram size accounting; cross-language interop needs a
published wire specification, not a Rust trait. The internal type parameter is retained, so the seam
survives at no public cost.

**Why `Clock` stays test-gated, deliberately.** A transport that loses, duplicates or reorders
datagrams is *already assumed* by the protocol. A clock that is not monotonic silently breaks
invariant 2 and the causal ordering that tombstone GC depends on. The asymmetry is intentional and
should be documented as such rather than left looking like an oversight.

**Note.** Before this decision only two of the five ports (`Persistence`, `Discovery`) were actually
consumer-wireable; the other three were ports in name only.

### D3 — The write API stays infallible

**Decision.** `insert` / `insert_bulk` / `remove` keep their current signatures. The value-size
ceiling is documented, and exceeding it produces a distinct, alertable signal rather than an
`io::Result` on every write.

**Why.** Making the whole write surface fallible is a broad public break to surface one failure mode
that is better fixed at its root (chunking). See invariant 9 below and the tracking issue.

### D4 — `ReconcileStore::node_id()` is added

**Decision.** Expose the node identity that `Config::with_node_id` sets.

**Why.** It is currently settable but not readable. It is needed for diagnostics, and it is a
prerequisite for any per-replica value state should D6 ever be revisited.

### D5 — `Value: PartialEq` is dropped

**Decision.** Remove `PartialEq` from the `Value` bound bundle and rewrite the single site that used
it to compare stamps.

**Why it is redundant.** Its only production use was post-merge change detection,
`if merged_v != *local_v`. Because `Entry::merge` is `if other.stamp > self.stamp { other } else
{ self }`, that expression is *provably equivalent* to `remote_v.stamp > local_v.stamp`: merge
returns `other` exactly when the stamps differ (so the entries differ), and returns `self`
otherwise (so they are equal). The stamp comparison needs only `Timestamp: Ord`, which is already
available, and no bound on `V` at all.

**Secondary benefit.** The old formulation cloned the whole value inside `merge`, compared it (a
value-sized comparison on the receive hot path), then discarded the clone when nothing changed. The
stamp comparison clones only when actually applying.

**What would overturn it.** The equivalence holds *only* under last-write-wins. Under a CRDT resolver
the merged stamp must be `max(local, remote)`, so a join can change the payload while the stamp
equals `local.stamp` — stamp comparison would silently miss the change. If D6 is ever revisited,
`PartialEq` (or a hash comparison) must be reinstated. This is recorded on the D6 tracking issue.

### D6 — Conflict resolution stays hardcoded last-write-wins

**Decision.** §3.6's deferral of a pluggable `Resolve` seam stands, with the open-ended condition
replaced by three named triggers. As of this decision a counter is **hypothetical**, so none has
fired.

- **T1 — a converging counter becomes a real requirement.** The only genuinely inexpressible gap:
  concurrent `+1` and `+1` under LWW yields `+1`, and no key encoding recovers it.
- **T2 — an opaque third-party CRDT document as the value** (Automerge / Loro / Yjs bytes). The
  strongest case for a merge seam, because such documents carry their causal context internally, so
  a bare value-level join genuinely suffices. Gate on documents staying well inside one datagram.
- **T3 — multi-value register.** Not a policy seam at all: `Timestamp` is a *total order*, so for any
  two stamps one is greater and concurrency cannot be represented. The information is destroyed at
  stamp-mint time. This would require replacing `Timestamp` with a version vector, detonating
  invariants 2 and 7 and the `Clock` port's explicit out-of-scope note. Treat as a separate project.

**Why the deferral is strengthened, not merely maintained.** Three repository-specific facts:

1. **Stable Rust offers no cheap opt-in.** `Value` is a blanket impl, so a `merge` with a default
   body that specific types override requires specialization (nightly). The only alternatives are
   breaking every downstream `V`, or a third public type parameter.
2. **The datagram ceiling is a correctness cliff.** A single `Update` above the payload budget is
   never fragmented and never delivered (invariant 9). CRDT state grows with cardinality or history,
   so a set-as-value or document-as-value crosses that cliff *as it succeeds*.
3. **The most-requested CRDT is already free.** An add-wins set is expressible today as
   `(set_id, element) -> ()` keys, reusing the existing tombstone GC and obtaining *per-element*
   diffing that a set-as-value cannot. Strictly this is last-op-wins per element, differing from
   add-wins only under a genuinely concurrent add/remove of the same element.

**If a trigger fires, the shape is a concrete facade, not a public type parameter.** Seven of eight
comparable systems ship a fixed menu of concrete types rather than a pluggable hole; the lone
exception needs three traits to do it and ships the menu anyway. A public merge trait maximises the
number of ways a user can silently break convergence — non-canonical `Hash`, non-idempotent merge,
oversize state — while this crate's own test suite stays green.

**Related constraint.** Only *state-based* (join-idempotent) payloads are safe on this transport:
`Update` rides unreliable UDP with retransmission, so a duplicated `increment(+1)` is not idempotent.
Operation-based deltas are ruled out independently of any seam.

### D7 — Larger-than-RAM datasets are permanently out of scope

**Decision.** The crate does not pursue datasets exceeding one node's RAM. A pluggable storage
backend is therefore not built; the question is closed rather than deferred.

**Why.** Larger-than-RAM and full replication are in direct tension. If the dataset exceeds RAM and
every node holds everything, then every node hits disk on reads — destroying the one unambiguous
advantage (§1.6: reads are local, in-process, no network hop, no deserialization). The requirement
splits into two different products: a fully-replicated on-disk store, whose market is thin and
already occupied by persistent range-sync CRDT stores in the same language; or sharding, which is a
different system entirely and is tracked separately as partial replication.

**What people usually mean.** "Can it persist?" is nearly always "survive a restart", not "exceed
RAM" — already served by `Persistence` / `FileSnapshot`. The genuinely differentiated persistence
direction is content-addressed structural sharing, whose value is versioning, snapshot diff and
incremental cold start — not capacity.

**The cheap capacity win instead.** Reducing per-entry memory raises the effective ceiling by
roughly 2-3x at zero contract cost, which is the honest way to serve larger datasets without
changing the architecture.

**What would overturn it.** Nothing short of adopting partial replication, at which point this
decision is superseded by that design rather than amended.

### D8 — Reconciliation strategy is automatic, never a user-facing choice

**Decision.** If a sketch-based fast path is added, it self-selects and falls back to range-based
reconciliation. There is no user-visible strategy knob.

**Why, and the guard that matters.** Range-based reconciliation must remain the **sole authority for
the "in sync" decision**. An all-zero *difference* sketch does not prove two sets are equal — cells
cancel on both the key sum and the counter, so two distinct same-size sets can cancel. That is
exactly the hazard invariant 3 exists to prevent. A sketch may only ever *propose* differences; a
stalled or empty peel falls through to the range diff untouched.

**Note on the choice of sketch.** A *fixed-capacity* sketch is preferred over a rateless one for the
steady state, because a fixed sketch is a monoid and can be maintained incrementally (O(1) per
in-sync round), whereas a rateless encoder must touch every element per session — untenable at a
one-second gossip cadence. Detail lives on the tracking issue.

### D9 — `ReconcileMirror` is documented as last-write-wins only

**Decision.** State the constraint explicitly now.

**Why.** `integrate` performs a plain overwrite rather than a join, which is correct *only* because
the authoritative dated peer already resolved the conflict. Under any non-LWW resolution,
last-writer-by-arrival is wrong, and the mirror would need redesign. Cheap to document now; expensive
to discover later.

### D10 — 0.3.0 is one coordinated break

**Decision.** Wire and on-disk breaks are batched into 0.3.0 rather than dribbled across releases.
The `Entry` / `State` domain type and the versioned snapshot header are already in. The fingerprint
wire encoding fix is folded in, since it is a pure win that never becomes cheaper. Additive,
capability-gated protocol extensions do not need to ride it.

### D11 — Phase 1 lands before Phase 2 is retargeted

**Decision.** The Phase-1 stack merges to `main` first; the Phase-2 pull request is then retargeted
from the Phase-1 tip to `main`.

**Why.** Phase 2 is stacked on the Phase-1 tip so its diff shows only its own commits. Retargeting to
`main` before Phase 1 lands would present the whole train as one review.

### D12 — Continuous integration is re-enabled

**Decision.** Turn GitHub Actions back on.

**Why.** The repository is public and the workflows use only standard hosted runners, for which
Actions is free — so the dormancy is a settings or organisation-policy toggle, not a billing
constraint. Note that an organisation-level restriction overrides the repository setting, so both
need checking. Until it is on, the full matrix must be reproduced locally; note that
`cargo doc` under `-D warnings` catches failures that `cargo build` and `cargo clippy` do not.

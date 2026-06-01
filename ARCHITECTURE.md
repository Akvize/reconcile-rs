# Architecture review & refonte foundations — `reconcile-rs` (crate `reconcile`)

> Forward-looking **architecture** document: a critical review of the current layering and
> contracts, a concrete target architecture (layers, module tree, trait signatures), and a
> phased, test-green migration path.
>
> - **Date:** 2026-06-01
> - **Manifest version:** `0.0.0-git` (unpublished under this shape — no backward-compatibility
>   contract yet, so the API and the on-wire/on-disk formats may be broken freely).
> - **Scope:** architecture, contracts and abstraction. This is **not** a defect review — for the
>   correctness/security peer-review see [`REVIEW.md`](./REVIEW.md). Most of its critical findings
>   (F1–F8: `hash==0` sentinel, packet-panic DoS, missing auth, tombstone resurrection,
>   physical-clock LWW, weak XOR fingerprint, reachable `unimplemented!()`, unstable
>   `DefaultHasher`) are **already fixed** in the current code (256-bit BLAKE3 fingerprint, HLC,
>   per-datagram MAC, causal-stability gate, `checked_sub` hardening). This document assumes that
>   corrected baseline and asks a different question: *is the architecture clean?*
> - **Method:** static reading of `src/`, `tests/`, `Cargo.toml`. No source file modified by this
>   document. Every claim carries a `file:line` proof against the audited tree.

---

## 1. Diagnosis in one sentence

The code is **correct and well-tested but layer-collapsed**: a data structure, a wire protocol, a
replication-semantics policy (HLC + LWW + tombstone GC), a transport (UDP/bincode/MAC) and a user
facade all live in one flat module set **with no contract boundaries** — and the one place where
abstraction was attempted (the associated types of `Diffable`) adds ceremony without flexibility.

The fix is to introduce **named seams** — traits at the layer boundaries — and to make the
value/tombstone/clock/transport models **pluggable**, *without touching the handful of things that
are correctness- or compatibility-load-bearing* (§5).

---

## 2. Critical review of the current architecture

### 2.1 The five layers, and where they leak

The crate naturally decomposes into five layers, but the modules do not reflect them:

| Layer | Responsibility | Today's files |
|---|---|---|
| **L1 — Data structure** | Ordered map with O(log n) range fingerprints | `hrtree.rs`, `hrtree_iter.rs`, `fingerprint.rs` |
| **L2 — Reconciliation protocol** | Range-subdivision set-difference algorithm over fingerprints; pure, transport-agnostic, no I/O | `diff.rs` |
| **L3 — Replication semantics** | What a *version* is (clock), how conflicts *merge*, what a *tombstone* is, when a tombstone is GC-safe (causal stability) | `hlc.rs`, `reconcilable.rs`, `timeout_wheel.rs` + ack logic **buried inside** `reconcile_engine.rs` |
| **L4 — Transport + codec + membership** | UDP socket I/O, bincode framing, MAC auth, peer discovery/sampling, the async protocol *driver* | `reconcile_engine.rs`, `auth.rs`, `gen_ip.rs` |
| **L5 — Service facade** | The single public type users hold; wiring, persistence, builder config | `reconcile_store.rs`, `persistence.rs` |

**Two clean layering violations** stand out:

1. **L3 logic is embedded in L4.** Clock advancement (`clock.observe`), per-value tombstone
   version-hashing (`version_hash`, `reconcile_engine.rs:55`) and ack tracking
   (`tombstone_acks`, `reconcile_engine.rs:83`) live inside the engine's message loop rather than
   in a semantics layer. The *policy* (when is a deletion safe to forget?) is tangled with the
   *mechanism* (UDP datagrams).
2. **L2 is reached by a blanket impl that hardwires L1's key type.** `diff.rs:78`,
   `impl<K: Clone, T: HashRangeQueryable<Key = K>> Diffable for T`, means nobody ever implements
   `Diffable` directly; the protocol is welded onto the data-structure trait.

### 2.2 Seven hard-coded couplings that block abstraction

| # | Coupling | Proof | Why it hurts |
|---|---|---|---|
| C1 | **Clock hardwired to HLC** | `reconcile_engine.rs:86` (`clock: Arc<HlcClock>`), `hlc.rs` | No `Clock`/`Timestamp` seam; a different versioning scheme (vector clocks, externally-supplied stamps) cannot be plugged in. |
| C2 | **Conflict resolution hardwired to LWW-on-Hlc** | `reconcilable.rs:27` (`impl Reconcilable for (Hlc, V)`) | Merge policy is fixed; no way to override per value type without re-implementing the engine. |
| C3 | **Value model hardwired to `(Hlc, Option<V>)`** | `reconcile_store.rs:52`, `persistence.rs:51` (`DatedEntries = Vec<(K,(Hlc,Option<V>))>`) | The timestamp+tombstone tuple is structural, not a named type. |
| C4 | **Tombstone leaks into the public API** | `reconcile_store.rs:171` — `add_pre_insert<F: … Fn(&K, &(Hlc, Option<V>)) …>` | Users of the *public* hook must know the internal wrapping `(Hlc, Option<V>)`; the abstraction boundary is broken at the surface. |
| C5 | **Tombstone logic fragmented** | `reconcilable.rs:43` (`MaybeTombstone`), `Option<V>`, `timeout_wheel.rs`, `tombstone_acks` (`reconcile_engine.rs:83`), store GC loop | One concept (a deletion and its safe lifetime) is smeared across five places. |
| C6 | **Transport=UDP & codec=bincode hardwired** | `reconcile_engine.rs:67` (`socket: Arc<UdpSocket>`), `:21` (`use bincode::…`) | No `Transport`/`Codec` seam; cannot swap UDP→QUIC or bincode→protobuf, and cannot unit-test the driver without real sockets. |
| C7 | **Public surface leaks internals** | `lib.rs:30-37` exposes `pub mod diff, fingerprint, hrtree_iter, hlc, gen_ip` next to `reconcile_store` | A user cannot tell the stable external contract (`ReconcileStore`) from low-level plumbing (`HashSegment`, `Fingerprint` internals, iterators, `HlcClock`). |

### 2.3 The verbosity: bounds and the vacuous two-trait split

The owner's "verbose, unreadable traits" complaint has two concrete causes.

**(a) Giant `where` blocks, repeated.** The same nine-bound key constraint and eleven-bound value
constraint are spelled out at every impl site:

```rust
// reconcile_engine.rs:122-135
impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Send + Serialize + Sync + 'static,
        V: Clone + DeserializeOwned + Hash + MaybeTombstone + PartialEq + Reconcilable
            + Send + Serialize + Sync + Timestamped + 'static,
    > ReconcileEngine<K, V>
```

The identical key bound reappears at `reconcile_store.rs:75` and in the `get_mut` impl. There is no
trait-alias/bundle, so every reader re-parses the same wall of bounds. Worse, `MaybeTombstone +
Reconcilable + Timestamped` are mixed into a *value* bound — but those are **semantics of the entry
type**, not properties any plain value has. They belong to the entry model (§3.4), not to `V`.

**(b) A two-trait split whose associated types are vacuous.** `HashRangeQueryable` (`diff.rs:30`)
plus `Diffable` (`diff.rs:58`), where `Diffable::ComparisonItem` is *always* `HashSegment<K>` and
`Diffable::DifferenceItem` is *always* `DiffRange<K>` (`diff.rs:78-80`). The associated types
suggest flexibility that does not exist; the blanket impl means `Diffable` is never implemented by
hand. Two traits and two associated types pay for zero generality.

---

## 3. Target architecture

### 3.1 Structural decision: single crate now, **workspace-ready** module tree

The owner's stated target is a **multi-crate workspace**. The recommendation here is to **reach it
in two moves, not one**: first reorganize into a layered *module* tree inside the single crate, with
explicit `pub` / `pub(crate)` contracts and module names chosen so that "promote each `mod` to a
crate" is mechanical; then split into a workspace once the layers have stabilized (and ideally once
a sub-crate has an external consumer).

**Why module-first, crate-second:**

- *Pre-1.0, unpublished.* A workspace's main payoff — independent versioning and compile-time
  isolation of sub-crates — has near-zero value before any external consumer exists.
- *Risk isolation.* The hard part of this refonte is the **trait design** (§3.3) and the
  **god-object split** (§3.5). Doing that *and* fighting cross-crate visibility, `Cargo.toml`
  plumbing and feature-flag propagation simultaneously multiplies risk for no behavioural gain.
- *Tests stay honest.* `tests/service.rs` and `tests/proptest_hrtree.rs` reach across all layers;
  one crate lets them stay green through every phase without being rewritten against several crates.

**The single downside, and its mitigation.** Module-level `pub(crate)` is a softer wall than a crate
boundary: a lazy cross-layer reach still compiles. Mitigation: the explicit re-export facade
(§3.2), code review, and — later — a `clippy.toml`/`cargo-deny`-style disallowed-path lint in CI.

**Bascule criteria (when to actually split into the workspace):** when `hrtree/`+`proto/` gains an
external consumer, or when `no_std`/alternative-transport feature isolation is wanted. The module
tree below names each layer after its eventual crate so the split is a move, not a redesign.

```
hrtree/    → reconcile-hrtree   (L1)
proto/     → reconcile-proto    (L2)
semantics/ → reconcile-core     (L3)
net/       → reconcile-net      (L4)
store/     → reconcile          (L5, the umbrella crate)
```

### 3.2 Target module tree (single crate, workspace-ready)

```
src/
  lib.rs                 // THE public facade: re-exports L5 + the few L1/L3 extension points
  prelude.rs             // optional `pub use` bundle for downstream users
  bounds.rs              // pub: Key, Value supertrait bundles (used everywhere)

  hrtree/                // L1  (future crate: reconcile-hrtree)
    mod.rs               //   pub: HRTree, Fingerprint, RangeHashMap (the merged L1/L2 contract)
    node.rs
    iter.rs
    fingerprint.rs

  proto/                 // L2  (future crate: reconcile-proto)  — pub(crate)
    mod.rs               //   start_diff / diff_round as free fns over RangeHashMap
    segment.rs           //   HashSegment, DiffRange  (wire types)

  semantics/             // L3  (future crate: reconcile-core)
    mod.rs
    clock.rs             //   pub: Clock, Timestamp, Versioned
    merge.rs             //   pub: Merge (was Reconcilable)
    entry.rs             //   pub: Entry<T,V>, State<V>  (replaces (Hlc, Option<V>))
    hlc.rs               //   pub: Hlc (impl Timestamp) + pub(crate) HlcClock (impl Clock)
    stability.rs         //   pub(crate): TombstoneTracker (was tombstone_acks + timeout_wheel)

  net/                   // L4  (future crate: reconcile-net)
    mod.rs
    transport.rs         //   pub: Transport trait; pub(crate) UdpTransport
    codec.rs             //   pub: Codec trait;     pub(crate) BincodeCodec
    auth.rs              //   pub(crate): Mac, Authenticator, ClusterKey, Payload
    membership.rs        //   pub(crate): PeerTable, discovery (gen_ip)
    driver.rs            //   pub(crate): ProtocolDriver (the slimmed-down engine)

  store/                 // L5  (this crate's reason to exist)
    mod.rs               //   pub: ReconcileStore, Config
    persistence.rs       //   pub: Persistence, FileSnapshot, InMemoryPersistence, PersistedState
```

### 3.3 External vs internal contract, per layer

- **L1 (`hrtree/`) — external:** `HRTree`, `Fingerprint`, and the merged range-hash trait
  `RangeHashMap` (§3.6). Legitimately reusable on their own. Everything else (`node`, cached-hash
  internals) is `pub(crate)`.
- **L2 (`proto/`) — internal only.** The protocol is an implementation detail of replication.
  `HashSegment`/`DiffRange` are **wire types**: `pub(crate)`, never in the public API. (Today
  `diff.rs` is `pub mod` at `lib.rs:30` — coupling **C7**; demote it.)
- **L3 (`semantics/`) — external:** `Clock`, `Timestamp`, `Versioned`, `Merge`, `Entry`, `State`,
  `Hlc` — the **extension points** users plug into to change clock or conflict policy. `HlcClock`,
  `TombstoneTracker` are `pub(crate)`.
- **L4 (`net/`) — external:** the `Transport` and `Codec` *traits* (so an embedder can swap UDP for
  QUIC, or bincode for protobuf). Concrete `UdpTransport`/`BincodeCodec`, `Authenticator`,
  `PeerTable`, `ProtocolDriver` are `pub(crate)`.
- **L5 (`store/`) — external:** `ReconcileStore`, `Config`, the `Persistence` family. ~90% of the
  surface users actually touch.

`lib.rs` shrinks from eight `pub mod` (`lib.rs:30-37`) to a tight facade:

```rust
pub mod store;        // ReconcileStore, Config
pub mod persistence;  // Persistence family (or re-exported from store)
pub mod semantics;    // Clock / Merge / Entry / Hlc — extension points
pub mod hrtree;       // L1 reusable data structure
pub mod net;          // Transport / Codec traits (concretes stay pub(crate))

pub(crate) mod proto;
pub(crate) mod bounds;

pub use store::{Config, ReconcileStore};
pub use hrtree::{Fingerprint, HRTree};
pub use semantics::{Clock, Entry, Hlc, Merge, State, Timestamp, Versioned};
pub use persistence::{FileSnapshot, InMemoryPersistence, PersistedState, Persistence};
```

The key change: `diff`, `fingerprint` internals, `hrtree_iter`, `gen_ip`, `hlc::HlcClock` and
`auth` stop being top-level `pub`. Users can no longer mistake plumbing for the stable contract.

### 3.4 Bound bundles — kill the repeated `where` clauses

`#![feature(trait_alias)]` is unstable. Use the **stable supertrait + blanket-impl** pattern: a
marker supertrait bundles the bounds, a blanket impl makes every qualifying type satisfy it
automatically, and downstream users never implement it by hand.

```rust
// src/bounds.rs
use std::fmt::Debug;
use std::hash::Hash;
use serde::{de::DeserializeOwned, Serialize};

/// Everything a key must satisfy across the stack. Implemented automatically.
pub trait Key:
    Clone + Debug + Hash + Ord + Send + Sync + Serialize + DeserializeOwned + 'static {}
impl<T> Key for T where
    T: Clone + Debug + Hash + Ord + Send + Sync + Serialize + DeserializeOwned + 'static {}

/// Everything a *stored* value must satisfy: serializable, hashable (for the
/// fingerprint), comparable (to detect a no-op merge), thread-safe.
pub trait Value:
    Clone + Debug + Hash + PartialEq + Send + Sync + Serialize + DeserializeOwned + 'static {}
impl<T> Value for T where
    T: Clone + Debug + Hash + PartialEq + Send + Sync + Serialize + DeserializeOwned + 'static {}
```

The twelve-line bound block at `reconcile_engine.rs:122-135` collapses to `impl<K: Key, V: Value>`
(plus the entry-model bounds, which now travel with `Entry`, §3.5). Note the *conceptual* cleanup,
not just cosmetics: `MaybeTombstone + Reconcilable + Timestamped` move **out** of the value bound and
**into** the entry model where they belong.

### 3.5 Clock / Version abstraction (generalizes HLC)

Split HLC's two roles: the *timestamp* (a value type with a total order) and the *clock* (a stateful
minter/observer).

```rust
// src/semantics/clock.rs

/// A version stamp with a globally-deterministic total order. `Ord` MUST be a genuine
/// total order shared by all replicas (the conflict survivor is `max` over it), and it
/// MUST be part of the value's `Hash` so the fingerprint reflects version changes.
pub trait Timestamp:
    Clone + Copy + Ord + std::hash::Hash + Send + Sync
    + serde::Serialize + serde::de::DeserializeOwned + 'static {}

/// A per-node clock that mints monotonic timestamps and advances on observation.
pub trait Clock: Send + Sync + 'static {
    type Timestamp: Timestamp;
    /// Mint a fresh timestamp, strictly greater than every timestamp previously produced
    /// or observed by this clock (local monotonicity).
    fn now(&self) -> Self::Timestamp;
    /// Advance to account for a timestamp received from a peer, so a subsequent `now()` is
    /// ordered after `remote` (causality; prevents lost updates under skew).
    fn observe(&self, remote: Self::Timestamp);
}

/// A value carrying a version stamp: lets the engine read a stored value's timestamp
/// (to advance the clock) without knowing the concrete value type. Replaces `Timestamped`.
pub trait Versioned {
    type Timestamp: Timestamp;
    fn timestamp(&self) -> Self::Timestamp;
}
```

`Hlc: Timestamp` — the derived `Ord` already *is* the `(wall_ms, counter, node_id)` total order
(`hlc.rs:44-54`); **do not change it** (§5.2). `HlcClock: Clock<Timestamp = Hlc>` — its `now`/
`observe` (`hlc.rs:121`, `:146`) already match the trait exactly, so this is pure renaming.
`Timestamped` (`hlc.rs:171`) becomes `Versioned`.

### 3.6 First-class tombstones — replace `(Hlc, Option<V>)`

The deepest fix, addressing **C3/C4/C5**. The value model is currently the tuple `(Hlc, Option<V>)`
with tombstone = `None`, and that tuple leaks into the public `add_pre_insert` (`reconcile_store.rs:171`)
and into `PersistedState` (`persistence.rs:51`). Make it an explicit, named type with one trait.

```rust
// src/semantics/entry.rs

/// One stored, replicated cell: a version stamp plus either a live value or a tombstone.
/// Replaces the leaky `(Hlc, Option<V>)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Entry<T, V> { pub stamp: T, pub state: State<V> }

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum State<V> { Present(V), Tombstone }

impl<T: Timestamp, V> Versioned for Entry<T, V> {
    type Timestamp = T;
    fn timestamp(&self) -> T { self.stamp }
}

impl<T, V> Entry<T, V> {
    pub fn is_tombstone(&self) -> bool { matches!(self.state, State::Tombstone) }
    pub fn value(&self) -> Option<&V> {
        match &self.state { State::Present(v) => Some(v), State::Tombstone => None }
    }
}
```

This dissolves the fragmented `MaybeTombstone` trait (`reconcilable.rs:43`) into an inherent method,
removes the `Option` wrapper from the public hook signature, and gives `PersistedState` a named field
type. `version_hash` (`reconcile_engine.rs:55`) keeps working because `Entry` derives `Hash`.

> **Wire/disk format note.** The bincode encoding of `Entry<Hlc, V>` differs from
> `(Hlc, Option<V>)`. This is a **breaking on-disk/on-wire change** — acceptable because the crate
> is unpublished (`0.0.0`) and has no compatibility contract. Sequence it (Phase 4, §4) so the
> proptest oracle re-runs against the new shape.

### 3.7 Merge abstraction (generalizes LWW)

```rust
// src/semantics/merge.rs

/// Conflict resolution. MUST be commutative, associative and idempotent so all replicas
/// converge (Strong Eventual Consistency). Returns the winner of two concurrently-held
/// values for the same key.
pub trait Merge { fn merge(&self, other: &Self) -> Self; }

/// Last-write-wins over any total-order timestamp. Generalizes `Reconcilable for (Hlc, V)`.
impl<T: Timestamp, V: Clone> Merge for Entry<T, V> {
    fn merge(&self, other: &Self) -> Self {
        if other.stamp > self.stamp { other.clone() } else { self.clone() }
    }
}
```

**Recommendation: keep `Merge` as a trait on the entry type** (mirroring today's
`Reconcilable for (Hlc, V)`, `reconcilable.rs:27`) with LWW supplied by the blanket impl above —
**do not** introduce a separate `Lww<C>` strategy object. The engine already calls
`local.reconcile(&remote)` with no policy parameter; a free-standing strategy would force a policy
generic through the entire stack (driver, store, config) for a generalization nobody has requested.
The trait keeps the call site identical and still lets a user override merge by newtyping their value.

Do **not** weaken the strict `>` (§5.2): "equal stamp ⇒ keep self" is what makes merge commutative,
pinned by `reconcile_is_commutative_on_equal_wall_and_counter` (`reconcilable.rs:59`).

### 3.8 Merge the `HashRangeQueryable` + `Diffable` pair into one trait

Kill the vacuous two-trait split (§2.3b). Keep **one** trait `RangeHashMap` (the renamed
`HashRangeQueryable`), **delete** the `Diffable` trait, and turn `start_diff`/`diff_round` into
**free functions** in `proto/` parameterized by `RangeHashMap`. This removes the redundant trait
*and* its vacuous associated types in one move, and relocates the protocol (L2) out of the
data-structure contract (L1).

```rust
// src/hrtree/mod.rs  — the L1 contract, the only thing the protocol needs
pub trait RangeHashMap {
    type Key: Ord + Clone;
    fn range_hash<R: RangeBounds<Self::Key>>(&self, range: &R) -> Fingerprint;
    fn insertion_position(&self, key: &Self::Key) -> usize;
    fn key_at(&self, index: usize) -> &Self::Key;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool { self.len() == 0 }
}

// src/proto/mod.rs  — L2, pure, no I/O, pub(crate)
pub(crate) fn start_diff<M: RangeHashMap>(map: &M) -> Vec<HashSegment<M::Key>> { /* moved verbatim */ }
pub(crate) fn diff_round<M: RangeHashMap>(
    map: &M,
    in_comparison: Vec<HashSegment<M::Key>>,
    out_comparison: &mut Vec<HashSegment<M::Key>>,
    differences: &mut Vec<DiffRange<M::Key>>,
) { /* moved verbatim, including the #106/#111/#112 hardening */ }
```

> **Move `diff_round` verbatim.** Its body (`diff.rs:90-200`) contains the size-not-hash
> emptiness/equality decision (issues #106/#111, `diff.rs:135-141`) and the malformed-bound /
> inverted-range hardening (issue #112, `diff.rs:100-134`). It is correctness-critical; this is a
> **relocation, not a rewrite** (§5.3, §5.4).

### 3.9 Transport and Codec abstractions

Decouple UDP and bincode (**C6**). Keep the async surface minimal so `UdpTransport` is a thin
wrapper and `tests/fuzz_packets.rs` can still drive raw bytes.

```rust
// src/net/transport.rs
#[async_trait::async_trait]   // hand-rolled `impl Future` also fine; async-trait is simplest here
pub trait Transport: Send + Sync + 'static {
    type Addr: Clone + Send + Sync;
    async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, Self::Addr)>;
    async fn send_to(&self, buf: &[u8], dst: &Self::Addr) -> std::io::Result<usize>;
    fn local_addr(&self) -> std::io::Result<Self::Addr>;
}

// src/net/codec.rs
/// Frames protocol messages to/from bytes. The MAC/auth layer wraps the codec output; the
/// codec itself is auth-agnostic. Streaming decode (multiple messages per datagram) is
/// preserved by decoding from a cursor.
pub trait Codec: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    fn encode<T: serde::Serialize>(&self, value: &T, out: &mut Vec<u8>) -> Result<(), Self::Error>;
    fn decode_stream<T: serde::de::DeserializeOwned>(&self, bytes: &[u8]) -> Result<Vec<T>, Self::Error>;
}
```

`UdpTransport(Arc<UdpSocket>)` and `BincodeCodec(DefaultOptions)` are the `pub(crate)` defaults
wired by `Config`. **Keep `BincodeCodec` on the exact same `DefaultOptions` and the same `Message`
enum tag order** (`reconcile_engine.rs:108-120`) — the on-wire representation of
`ComparisonItem/Update/Ack`, `HashSegment` and `Fingerprint` must not change beyond the `Entry`
re-encoding of §3.6 (§5.1). The MAC/`Authenticator` stays a **separate `pub(crate)` wrapper outside
and ahead of** the codec — auth-before-deserialize is a security property (issue #108, §5.5), never
folded into the codec.

### 3.10 Decomposing the god object `ReconcileEngine`

`ReconcileEngine` (`reconcile_engine.rs:64-87`, 789 lines) currently fuses: map ownership, the UDP
socket, bincode ser/de, MAC framing, peer expiry, member tracking, the clock, tombstone-ack state,
the `Message` enum, the recv/dispatch loop, broadcast-on-write, and the pre-insert hook. Split into
single-responsibility components owned by a thin driver:

| New component | Owns / does | Carved from |
|---|---|---|
| `Transport` + `UdpTransport` (`net/`) | socket bind, `recv_from`/`send_to`, retries | `socket`, `send_to_retry`, `MAX_SENDTO_RETRIES` |
| `Codec` + `BincodeCodec` (`net/`) | `Message` encode / decode-stream | `Serializer`/`Deserializer`, `BUFFER_SIZE` framing |
| `Authenticator` (`net/auth.rs`) | MAC open/seal, `Payload` gate | `auth.rs` (already isolated) |
| `PeerTable` (`net/membership.rs`) | `peers` (expiring) + `members` (monotonic) + `gen_ip` sampling | `peers`, `members`, `get_peers`, `PEER_EXPIRATION`, `gen_ip` |
| `TombstoneTracker` (`semantics/stability.rs`) | `tombstone_acks` + reciprocal-ack + `TimeoutWheel` + `is_gc_ready` predicate | `tombstone_acks`, ack logic in `handle_messages`, `timeout_wheel.rs` |
| `Arc<dyn Clock>` (or `Arc<HlcClock>`) | mint/observe | `clock` field, `clock_now`, the `observe` call |
| `ProtocolDriver` (`net/driver.rs`) | the slim coordinator: `run()` loop, dispatch, broadcast-on-write | what remains after the extractions |

**Wiring.** `ProtocolDriver` holds `Arc<RwLock<HRTree>>`, a `Transport`, a `Codec`, an
`Authenticator`, a `PeerTable`, a `TombstoneTracker`, and a `Clock`. Its `run()` loop becomes:
`transport.recv_from` → `authenticator.open` → `codec.decode_stream` → classify into
comparisons/updates/acks → delegate (comparisons → `proto::diff_round`; updates → `apply_updates`,
which calls `clock.observe`, `entry.merge`, `tombstone_tracker.record`; acks → `tombstone_tracker`)
→ each delegate returns outbound messages, which the driver encodes + auths + sends. This turns
`handle_messages` (today ~150 lines mixing five concerns) into a ~30-line dispatcher.

**Causal-stability invariant survives the split intact** (§5.6): a tombstone is GC-eligible only
when *every* member in `PeerTable.members` has acked the *exact* `version_hash` currently held.
Concentrate that logic in `TombstoneTracker::is_gc_ready(key, version, &members)`, byte-for-byte.

---

## 4. Phased migration (ordered, each independently committable, test-green)

Every phase ends with `cargo test` green against the **existing** suite (the behavioural oracle).
No big-bang.

| Phase | What | Files | Risk | Oracle |
|---|---|---|---|---|
| **1 — Bound bundles + facade** *(cheapest, highest visible value)* | Add `bounds.rs` (`Key`/`Value`), replace the repeated `where` blocks; demote `diff`, `fingerprint` internals, `hrtree_iter`, `gen_ip`, `hlc::HlcClock` to `pub(crate)` in `lib.rs`. (`MaybeTombstone+Reconcilable+Timestamped` stay explicit until Phase 4.) | `lib.rs`, `bounds.rs` (new), `reconcile_engine.rs`, `reconcile_store.rs` | very low | whole suite compiles+passes unchanged; a successful compile *proves* the bundles are equivalent. Check `tests/diff.rs` for direct imports of demoted paths and keep them reachable (`#[cfg(test)] pub use`). |
| **2 — Merge L1/L2, relocate protocol** | Rename `HashRangeQueryable`→`RangeHashMap` into `hrtree/`; delete `Diffable`; move `start_diff`/`diff_round` **verbatim** into `proto/`; update the two engine call sites. | `diff.rs`→`hrtree/`+`proto/`, `hrtree.rs`, engine, `tests/diff.rs` (import-only) | low | `tests/diff.rs` (#106/#111/#112) + `proptest_hrtree.rs` convergence catch any accidental edit to moved code. |
| **3 — Clock/Timestamp/Versioned** | Introduce the three traits; `impl Timestamp for Hlc`, `impl Clock for HlcClock`; rename `Timestamped`→`Versioned`. Engine still holds `Arc<HlcClock>`, called through the trait. | `hlc.rs`→`semantics/{clock,hlc}.rs`, `reconcilable.rs`, engine | low | `hlc.rs` unit tests (monotonicity, observe-advances, total-order tie-break) move with the type, stay green. |
| **4 — First-class `Entry`** *(highest risk: changes encoding)* | `Entry`/`State` replace `(Hlc,Option<V>)`; `Reconcilable`→`Merge`; delete `MaybeTombstone`; fix the public `add_pre_insert` signature; update `PersistedState`/`DatedEntries`. | `entry.rs`+`merge.rs` (new), engine, `reconcile_store.rs:171`, `persistence.rs:51` | **highest** | `tests/service.rs` (resurrection, conflicts, two-node convergence), `proptest_hrtree.rs` (lossy-transport convergence = strongest round-trip check), `FileSnapshot` round-trips (`reconcile_store.rs` persistence tests). |
| **5 — Extract `TombstoneTracker` + `PeerTable`** | Pull acks+reciprocal+`TimeoutWheel` into `semantics/stability.rs` with `is_gc_ready`; pull `peers`/`members`/`gen_ip` into `net/membership.rs`. | `timeout_wheel.rs`→`stability.rs`, `gen_ip.rs`→`membership.rs`, engine, store GC loop | medium | resurrection + authenticated-node tests in `tests/service.rs`; `fuzz_packets.rs` for ack robustness. |
| **6 — Extract `Transport`/`Codec`, slim the driver** | Introduce the traits + `UdpTransport`/`BincodeCodec`; rewrite `handle_messages`/`run` as `ProtocolDriver` delegating to Phase-5 components; keep `Authenticator` ahead of the codec; freeze `DefaultOptions`/`Message` tag order. | `net/{transport,codec,driver}.rs`, `reconcile_engine.rs`→`driver.rs`, `auth.rs`→`net/auth.rs` | medium | `tests/service.rs` two-node convergence + `fuzz_packets.rs` (malformed datagram never panics) + `proptest_hrtree.rs` lossy convergence. |
| **7 — Final facade + module move + docs** | Physically move files into `hrtree/ proto/ semantics/ net/ store/`; finalize `lib.rs` + optional `prelude`; update README/module docs to name the layers and extension points; materialize crate boundaries for the eventual workspace split. | tree move, `lib.rs`, README | low | everything compiles, full suite green, `cargo doc` builds. |

**Ordering rationale.** Each phase depends only on its predecessors. Phase 1 delivers visible value
immediately. The highest-risk encoding change (Phase 4) sits in the middle, where the test oracle is
richest, not at the end where regressions are expensive to localize.

---

## 5. What must NOT change (correctness / compatibility load-bearing)

1. **`Fingerprint` wire format and arithmetic** — the `[u64;4]` 256-bit abelian group (add/sub mod
   2²⁵⁶, per-element BLAKE3), its `combine`/`remove`, and the golden vectors in `fingerprint.rs`.
   The whole protocol's correctness rests on `combine` being a commutative group so range
   fingerprints compose; the golden vectors pin the exact bytes. Relocate the file only.
2. **HLC total order `(wall_ms, counter, node_id)`** (`hlc.rs:44-54`) — the derived `Ord` field
   order *is* the conflict-resolution order. Generalizing to a `Timestamp` trait must preserve
   `Hlc`'s exact `Ord`/`Hash`. Do not reorder fields; do not weaken `>` to `>=` in merge.
3. **The size-not-hash emptiness/equality decision in `diff_round`** (issues #106/#111,
   `diff.rs:135-141`) — decisions on `size`/`local_size`, never on `hash == ZERO`, because a
   non-empty range can legitimately fingerprint to zero. Move verbatim.
4. **Malformed-bound / inverted-range hardening in `diff_round`** (issue #112, `diff.rs:100-134`) —
   dropping unsupported bound shapes and `checked_sub` instead of `unimplemented!()`/underflow. A
   remote-DoS fix; preserve every branch. Pinned by `tests/diff.rs`.
5. **Auth-before-deserialize ordering** (issue #108) — `Authenticator::open` runs on raw bytes
   *before* any `Codec::decode`, and only accepted senders are recorded as peers/members. The
   `Codec`/`Transport` split keeps the MAC gate outside and ahead of the codec, never folded in.
6. **Causal-stability tombstone gate** — GC only after every `members` entry has acked the exact
   `version_hash` held; `members` is monotonic (never auto-expired) precisely so a long-partitioned
   peer cannot resurrect a deletion. `TombstoneTracker` must reproduce this exactly. Pinned by the
   resurrection test in `tests/service.rs`.
7. **`version_hash` determinism** (`reconcile_engine.rs:55`) — `DefaultHasher`'s fixed keys make the
   token identical across nodes. Do **not** swap to a randomized/seeded hasher when `Entry` gains
   its derived `Hash`.

---

## 6. Summary

The refonte is not a rewrite. It is the introduction of five **named seams** —
`RangeHashMap` (L1/L2), `Clock`/`Timestamp`/`Versioned` + `Merge` + `Entry` (L3), `Transport`/`Codec`
(L4) — plus a `Key`/`Value` bound bundle to kill the verbosity, plus the decomposition of one
god-object into single-responsibility components. The seven load-bearing invariants of §5 are held
fixed throughout, and the existing test suite (`service`, `proptest_hrtree`, `diff`, `fuzz_packets`)
is the oracle that keeps every one of the seven migration phases honest. The module tree is named so
that the owner's ultimate target — a multi-crate workspace — becomes a mechanical move once the
layers have proven stable.

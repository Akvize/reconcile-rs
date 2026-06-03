# reconcile-rs — Architecture

## Status & scope

`reconcile-rs` is a reconciliation service that keeps a key-value map synchronised across several
instances. This document describes the **current architecture** and the **target architecture**
(hexagonal — ports & adapters). Correctness and security properties are tracked in
[`PROGRESS.md`](./PROGRESS.md) and are assumed here as the baseline; the state-of-the-art
positioning is in [`SOTA.md`](./SOTA.md).

The crate is unpublished (`0.0.0-git`); the public API and the on-wire / on-disk formats are not yet
stable and may change. Code locations are given as `file:line` against the current tree.

---

## 1. System overview

A node holds an ordered key-value map and gossips changes to its peers so that all replicas
converge. The design rests on five mechanisms:

- **Storage** — a Hash-Range Tree (`HRTree`): an ordered map that also maintains, for every subtree,
  a **range fingerprint** so the hash of any key interval is available in `O(log n)`.
- **Anti-entropy protocol** — two peers compare fingerprints over shrinking key ranges (`diff`) and
  exchange only the divergent entries. Equality and emptiness are decided by interval **size**, not
  by hash, to stay collision-safe.
- **Causality & conflict resolution** — each value is stamped with a Hybrid Logical Clock (`Hlc`);
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
| `hlc.rs` | Hybrid Logical Clock: timestamp type, ordering, and the clock that mints/observes stamps. |
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
| `hlc.rs` | `chrono::Utc` — physical time read inside the domain | `hlc.rs:35` |
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
  (`reconcilable.rs:27`) and `Timestamped` (`hlc.rs:171`) each carry a single implementation over a
  tuple — the stored cell is represented as `(Hlc, Option<V>)`. `Mac` (`auth.rs:92`) selects a MAC
  backend chosen at compile time.

Two consequences of the value-shape representation:

- The internal tuple `(Hlc, Option<V>)` leaks onto the public surface — into the
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
                                │  Hlc · Entry / State (value types)  —  no tokio / bincode  │
                                └──────────────────────────────────────────────────────────┘
```

### 3.3 Domain core

The interior contains, with no infrastructure dependency:

- the anti-entropy algorithm (`start_diff` / `diff_round`),
- the conflict-resolution policy (last-write-wins over the HLC order),
- the tombstone lifecycle and the causal-stability garbage-collection rule,
- the `HRTree` and `Fingerprint` (the storage and range-hash mechanism),
- the value types `Hlc`, `Entry`, `State`.

### 3.4 Ports

Four outbound ports, each removing one concrete infrastructure dependency from the domain:

```rust
// Clock — abstracts the time source (replaces the direct chrono::Utc read).
// The HLC algorithm stays in the domain; only physical time crosses the boundary.
pub trait Clock: Send + Sync + 'static {
    type Timestamp: Copy + Ord + Hash + Serialize + DeserializeOwned + Send + Sync + 'static;
    fn now(&self) -> Self::Timestamp;           // mint a strictly-monotonic local stamp
    fn observe(&self, remote: Self::Timestamp);  // advance past a peer's stamp (causality)
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
pub trait Codec: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    fn encode<T: Serialize>(&self, value: &T, out: &mut Vec<u8>) -> Result<(), Self::Error>;
    fn decode_stream<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<Vec<T>, Self::Error>;
}

// Persistence — durability boundary (already present, the model the others follow).
pub trait Persistence<K, V>: Send + Sync + 'static {
    fn load(&self) -> io::Result<Option<PersistedState<K, V>>>;
    fn save(&self, state: &PersistedState<K, V>) -> io::Result<()>;
}
```

### 3.5 Adapters

| Port | Default adapter | Backed by |
|---|---|---|
| `Clock` | `HlcClock` (its `now`/`observe`, `hlc.rs:121,146`, already match the port) | system time |
| `Transport` | `UdpTransport(Arc<UdpSocket>)` | tokio / UDP |
| `Codec` | `BincodeCodec(DefaultOptions)` | bincode |
| `Persistence` | `FileSnapshot`, `InMemory` | file / memory |

Message authentication (`Authenticator` / MAC) sits **ahead of** the codec: the MAC is verified on
raw bytes before any decoding occurs. It is never folded into the `Codec`.

The ports make the domain testable in isolation: an in-memory `Transport` (optionally lossy and
reordering) and a fixed `Clock` make convergence and HLC behaviour deterministic without real
sockets or wall-clock time.

### 3.6 Domain types and conflict policy

A single, intention-revealing type represents a stored cell, replacing the `(Hlc, Option<V>)` tuple
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

### 3.7 Internal mechanism

The anti-entropy mechanism is not a port. `HashRangeQueryable` and `Diffable` are removed as traits:
range-hash querying becomes inherent methods on the concrete `HRTree`, and `start_diff` / `diff_round`
become `pub(crate)` functions over it. The wire types `HashSegment` / `DiffRange` are `pub(crate)`.
None of this appears in the public surface.

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
reconcile-core   // DOMAIN + PORTS: Clock, Transport, Codec, Persistence (traits);
                 //   Entry / State, Hlc, Fingerprint, HRTree, the diff algorithm, LWW.
                 //   deps: serde, blake3.  (no tokio, bincode, chrono-IO, ipnet, runtime rand)
reconcile-net    // ADAPTERS: UdpTransport, BincodeCodec, Authenticator, peer discovery.
reconcile-store  // ADAPTERS: FileSnapshot / InMemory persistence.
reconcile        // WIRING + driving API: the Store facade, configuration.
```

Ports are defined in `reconcile-core`; adapters depend on `reconcile-core`. Infrastructure cannot be
imported into the core without a compile error — the guarantee a single crate cannot provide.

---

## 4. Current → target mapping

| Current | Target |
|---|---|
| `(Hlc, Option<V>)` tuple | `Entry<Hlc, V>` + `State<V>` domain type |
| `MaybeTombstone`, `Timestamped` | inherent `Entry` methods / field access |
| `Reconcilable` (LWW over tuple) | `Entry::merge` (LWW), optional `Resolve` policy seam |
| `HashRangeQueryable`, `Diffable` (public) | inherent `HRTree` methods + `pub(crate)` diff functions |
| `pub mod diff` exposing wire types | `pub(crate)` `HashSegment` / `DiffRange` |
| `chrono::Utc` read in `hlc.rs` | `Clock` port; `HlcClock` adapter holds the time read |
| `UdpSocket` in `reconcile_engine.rs` | `Transport` port; `UdpTransport` adapter |
| `bincode` in `reconcile_engine.rs` | `Codec` port; `BincodeCodec` adapter |
| `Persistence` | unchanged (the model port) |
| Multi-bound `where` blocks | `Key` / `Value` supertrait bundles |
| Single crate | `reconcile-core` / `-net` / `-store` / `reconcile` workspace |

---

## 5. Invariants

The following load-bearing properties are preserved across any restructuring; they encode the
correctness and security guarantees tracked in [`PROGRESS.md`](./PROGRESS.md).

1. **Fingerprint format & arithmetic** — `[u64; 4]`, per-element BLAKE3, add/sub mod 2²⁵⁶; the
   golden vectors in `fingerprint.rs` hold.
2. **HLC total order** `(wall_ms, counter, node_id)` (`hlc.rs:44-54`) — the derived ordering *is* the
   conflict order; `Clock::Timestamp = Hlc` preserves it, and merge uses strict `>`.
3. **Size-not-hash emptiness/equality** in `diff_round` (`diff.rs:135-141`).
4. **Malformed-bound / inverted-range hardening** in `diff_round` (`diff.rs:100-134`).
5. **Authenticate before deserialise** — the MAC is verified on raw bytes before the codec runs; the
   `Codec` port never absorbs authentication.
6. **Causal-stability tombstone gate** — a tombstone is garbage-collected only after every monotonic
   cluster member has acknowledged the exact version hash.
7. **`version_hash` determinism** (`reconcile_engine.rs:55`) — preserved as `Entry` derives `Hash`.

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
4. **`Entry` / `State` domain type** — replace `(Hlc, Option<V>)`; dissolve `MaybeTombstone` /
   `Timestamped`; fold `Reconcilable` into `Entry::merge`; update the public hook and
   `PersistedState`. *Changes the wire and on-disk formats.*
5. **`Transport` & `Codec` ports** — extract both; `UdpTransport` / `BincodeCodec` adapters; the
   engine becomes a thin driver over the ports, with authentication ahead of the codec.
6. **Workspace split** — promote the layers to `reconcile-core` / `reconcile-net` /
   `reconcile-store` / `reconcile`; ports defined in the core; the core carries no infrastructure
   dependency.

Steps 1–3 are independent of the format change; step 4 is the single format-breaking step; steps 5–6
complete the boundary extraction and enforce it at the crate level.

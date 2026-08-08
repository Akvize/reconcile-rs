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

- **Storage** — `FingerprintTreeMap`: an ordered map that also maintains, for every subtree,
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
| `rsos/src/fingerprint_tree_map.rs`, `rsos/src/fingerprint_tree_map_iter.rs` (standalone `rsos` crate, migration step 6 Step A, done) | `FingerprintTreeMap`: ordered map + range-fingerprint data structure and its iterators, plus the `Rsos<K, V>` trait (Def. 3.9) it implements. |
| `rsos/src/fingerprint.rs` (standalone `rsos` crate) | 256-bit additive fingerprint (`[u64; 4]`, per-element BLAKE3, add/sub mod 2²⁵⁶). |
| `diff.rs` | Anti-entropy algorithm (`start_diff`, `diff_round`) and its wire types. |
| `entry.rs` | The `Entry`/`State` domain type: value/tombstone semantics and the conflict-resolution policy (migration step 4, done). |
| `clock.rs` | Hybrid Logical Clock: timestamp type, ordering, and the clock that mints/observes stamps. |
| `auth.rs` | Per-datagram message authentication (MAC). |
| `persistence.rs` | Durability boundary: load/save a snapshot of the dated map. |
| `transport.rs` | The `Transport` port (datagram I/O) and its `UdpTransport` / `InMemoryTransport` adapters (migration step 5, done). |
| `reconcile_engine.rs` | Network orchestration: peer discovery, gossip, driving the `Transport` port and the `bincode` wire-encoding functions. |
| `bincode.rs` | The crate's wire-encoding functions (`pub(crate)`, no adapter type — migration step 5, done; see §2.4). |
| `reconcile_store.rs` | Public facade tying the engine, the map, and timeouts together. |
| `timeout_wheel.rs` | Acknowledgement timeout tracking. |

### 2.2 The domain mechanism is already infrastructure-free

The data structure and the protocol algorithm carry **no infrastructure dependency** — they import
no async runtime, no socket, no codec, no wall clock (outside `#[cfg(test)]`):

| Module | Infrastructure imported |
|---|---|
| `rsos/src/fingerprint_tree_map.rs`, `rsos/src/fingerprint_tree_map_iter.rs`, `rsos/src/fingerprint.rs` (standalone `rsos` crate) | none — enforced today by the crate's own minimal `Cargo.toml` dependency list, not just by grep. |
| `diff.rs`, `entry.rs` | none |

This is, in effect, the interior of the hexagon and it exists today.

### 2.3 Infrastructure coupling

Migration step 5 (§6) extracted the `Transport` port and the `bincode.rs` wire-encoding functions, so
`reconcile_engine.rs` no longer imports `tokio::net` or `bincode` directly — it drives the gossip
protocol purely over `Transport` (default adapter `UdpTransport`) and `crate::bincode::{encode,
decode_stream}`. The remaining infrastructure dependencies:

| Module | Infrastructure imported directly | Notes |
|---|---|---|
| `clock.rs` | `chrono::Utc` — physical time read inside the domain | Behind the `Clock` port (`HlcClock` adapter); not yet extracted to a separate adapter module (§3.9 target, not yet split into a workspace). |
| `transport.rs` | `tokio::net::UdpSocket`, `socket2` | The `Transport` port's adapter — this is the *intended* location for this dependency, not residual coupling. |
| `bincode.rs` | `bincode` | The crate's wire-encoding functions — likewise intended; not a port (§2.4). |
| `reconcile_engine.rs` | `tokio::time`, `rand::StdRng`, `ipnet` | Timer/backoff, discovery randomness, and CIDR geography — not yet ported (no port is warranted: these are not swappable infrastructure boundaries the way I/O is). |
| `reconcile_store.rs` | `chrono`, `ipnet` | Tombstone-expiry wall-clock reads and CIDR geography. |
| `mirror.rs` | `tokio::net::UdpSocket`, `bincode` | **Residual coupling** (tracked under issue #138): `ReconcileMirror` binds and owns its own UDP socket and calls `bincode`'s `Serializer`/`Deserializer` directly for its own receive loop. Its *send* path was rewired onto the shared `send_messages_to`/`send_to_retry` helpers (so it does construct a `UdpTransport` and go through `crate::bincode::encode` per send), but the mirror was never given its own injectable `Transport` — out of scope for step 5, left for a future pass. |

There is no longer an abstraction boundary missing between the dated engine and the transport: it is
a substitutable adapter, and the in-memory `Transport` (`InMemoryNetwork`/`InMemoryTransport`) paired
with the `bincode.rs` wire-encoding functions makes the engine's gossip protocol exercisable with no
real sockets. Wire encoding itself has a single implementation and is not behind a port (§2.4). The
wall-clock time source (`clock.rs`) and the mirror's socket stack remain the open items.

### 2.4 Trait landscape

Seven traits exist. They fall into three groups:

- **Boundary abstraction (1).** `Persistence` (`persistence.rs:77`) is a genuine port: it has two
  implementations (in-memory and file snapshot) and abstracts durability behind a small,
  intention-revealing contract.
- **Internal mechanism, currently public (2).** `HashRangeQueryable` (`diff.rs:30`) and `Diffable`
  (`diff.rs:58`) describe *how* the diff is computed over the tree. `Diffable` is a blanket impl
  whose associated types are always the same concrete types; `HashRangeQueryable` has a single
  real implementation (`FingerprintTreeMap`). They are exposed through `pub mod diff` (`lib.rs:30`), placing the
  protocol mechanism on the crate's public surface.
- **Value-shape helpers — dissolved (migration step 4, done).** `MaybeTombstone`, `Reconcilable` and
  `Timestamped` used to each carry a single implementation over the untyped `(Timestamp, Option<V>)`
  tuple that represented the stored cell. They have been replaced by the `Entry<Timestamp, V>` /
  `State<V>` domain type (`entry.rs`, §3.6): `Entry::is_tombstone` / `Entry::value` replace
  `MaybeTombstone`, `Entry::merge` replaces `Reconcilable`, and direct `.stamp` field access replaces
  `Timestamped`. `Mac` (`auth.rs:92`) selects a MAC backend chosen at compile time.

`Codec` followed the same path and was dissolved after step 5 landed (this cleanup, still tracked
under issue #144): it had exactly one implementation (`BincodeCodec`), its methods are generic so it
was never object-safe (always carried as a type parameter, unlike the object-safe `Transport`), and
neither plausible alternate use (compression, cross-language interop) is served by swapping a trait
— compression interacts with authenticate-before-decode and datagram-size accounting, and
cross-language interop needs a published wire spec, not a Rust trait. A second review pass then
dissolved `BincodeCodec` itself (still issue #144): zero-sized and stateless, with no injection point
(`pub(crate)`, never swapped), it was pure ceremony threaded through `Inner`, `SendPorts`, and every
constructor just to call what were effectively two static functions. Wire encoding is now the plain
`pub(crate) fn encode` / `fn decode_stream` in `bincode.rs` (renamed from `codec.rs`, since there is
no abstraction left to name — it is just the code that talks to the `bincode` crate); the engine
(`reconcile_engine.rs`) and `mirror.rs` call `crate::bincode::encode` / `crate::bincode::decode_stream`
directly. `Transport`, by contrast, remains a genuine port (§3.4/§3.5): object-safe, `pub`, and with
two real implementations (`UdpTransport`, `InMemoryTransport`).

Consequence of the (now-superseded) tuple representation, addressed by the same step:

- The internal tuple used to leak onto the public surface — into the `add_pre_insert` hook and into
  `PersistedState`. Both now carry `Entry<Timestamp, V>` directly (`reconcile_store.rs`,
  `persistence.rs`).

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
   (application)         │   HlcClock        UdpTransport    bincode::encode   FileSnapshot│
        │                │  (system time)   (tokio / UDP)   /decode_stream   / InMemory     │
        ▼                │       │               │                 │              │         │
  ┌───────────┐  impl    └───────┼───────────────┼─────────────────┼──────────────┼─────────┘
  │  Store    │           ports: │ Clock         │ Transport       │ (mechanism,  │ Persistence
  │ (facade)  │◀──────────────── ▼ ───────────── ▼ ─────────────── ▼ no port)──── ▼ ────────
  └───────────┘                 ┌──────────────────────────────────────────────────────────┐
        ▲                       │                    DOMAIN  (hexagon interior)              │
        │  driving port         │  anti-entropy algorithm · conflict policy (LWW)            │
        └───────────────────────│  tombstone lifecycle · FingerprintTreeMap + Fingerprint (mechanism)    │
                                │  Timestamp · Entry / State (value types)  —  no tokio / bincode  │
                                └──────────────────────────────────────────────────────────┘
```

### 3.3 Domain core

The interior contains, with no infrastructure dependency:

- the anti-entropy algorithm (`start_diff` / `diff_round`),
- the conflict-resolution policy (last-write-wins over the HLC order),
- the tombstone lifecycle and the causal-stability garbage-collection rule,
- the `FingerprintTreeMap` and `Fingerprint` (the storage and range-hash mechanism),
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
    fn node_id(&self) -> u64;             // identity stamped onto every minted Timestamp
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

// Wire encoding is NOT a port — see §2.4. `bincode.rs` holds plain functions of this shape;
// decode_stream carries a max_items cap so a single datagram cannot be expanded into an
// unbounded number of messages (closes the allocation-bomb hazard, issue #151). Authentication
// wraps it externally; never folded in (invariant 5).
//
// pub(crate) fn encode<T: Serialize>(value: &T, out: &mut Vec<u8>) -> bincode::Result<()>;
// pub(crate) fn decode_stream<T: DeserializeOwned>(bytes: &[u8], max_items: usize) -> bincode::Result<Vec<T>>;

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
| `Persistence` | `FileSnapshot`, `InMemory` | file / memory |
| `Discovery` | `RandomProbe` (default, speculative per-net probing); `DnsDiscovery` (authoritative, headless-Service DNS) | `gen_ip` / `tokio::net::lookup_host` |

Wire encoding is not a port (`bincode::{DefaultOptions, ...}`, backed by `bincode` — see §2.4): the
engine and `mirror.rs` call the plain `crate::bincode::encode` / `crate::bincode::decode_stream`
functions directly rather than naming a `Codec` trait or a codec value.

Message authentication (`Authenticator` / MAC) sits **ahead of** the codec: the MAC is verified on
raw bytes before any decoding occurs. It is never folded into `bincode.rs`.

The ports make the domain testable in isolation: an in-memory `Transport` (`InMemoryNetwork` /
`InMemoryTransport`, `transport.rs`) and a fixed `Clock` make convergence and HLC behaviour
deterministic without real sockets or wall-clock time.

**Visibility asymmetry (`Transport` public, `bincode.rs` `pub(crate)`).** `Transport` is
consumer-wireable: `ReconcileStore::new_with_transport` takes any
`Arc<dyn Transport<Addr = SocketAddr>>`, and `InMemoryNetwork`/`InMemoryTransport` are public (not
test-gated) so a downstream crate can drive a deterministic in-process cluster in its own tests —
that, plus a future non-UDP datagram transport (e.g. QUIC unreliable datagrams), is what earns
`Transport` a public injection point. `bincode.rs`'s `encode`/`decode_stream` stay `pub(crate)` and,
since there is no type — trait or struct — to name at all (§2.4), carry no visibility-vs-object-safety
tradeoff to explain — they are simply an internal mechanism, the same as `FingerprintTreeMap`. `Clock` stays
test-only for a different reason: the protocol already tolerates an unreliable transport, but a
non-monotonic clock silently breaks the causal ordering tombstone collection depends on, so it is not
a seam offered to callers.

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

The same step also absorbs the **value-only projection** that powers the dateless `ReconcileMirror`.
Today the engine keeps a second tree `FingerprintTreeMap<K, V::Projected>` fed through the `Projectable` trait
into a `ValueOnly<V>(Option<V>)` cell whose `Hash` is timestamp-less by construction. `State<V>` is
isomorphic to that cell (`Present(v) ↔ Some(v)`, `Tombstone ↔ None`), so the projection becomes
`Entry::project(&self) -> State<V>` (= `self.state.clone()`) and the projection tree becomes
`FingerprintTreeMap<K, State<V>>`; the `Projectable` trait and `ValueOnly` type are dissolved. The two hashes
must stay distinct — the dated `Entry` hashes **with** its stamp (for `version_hash`), the projected
`State<V>` hashes the value **alone** — which is invariant 8 in §5. Because a `ValueOnly(Some(v))`
and a `State::Present(v)` do not encode to the same bytes, this step also breaks the value-only wire
format, alongside the dated wire and on-disk formats (acceptable while the formats are unstable).

### 3.7 Internal mechanism

The anti-entropy mechanism is not a port. `HashRangeQueryable` and `Diffable` are removed as traits:
range-hash querying becomes inherent methods on the concrete `FingerprintTreeMap`, and `start_diff` / `diff_round`
become `pub(crate)` functions over it. The wire types `RangeAggregate` / `DiffRange` are `pub(crate)`.
None of this appears in the public surface.

### 3.8 Generic bounds

The repeated multi-bound constraints are expressed once, as supertrait bundles with blanket impls,
so implementation sites read `impl<K: Key, V: Value>`:

```rust
pub trait Key:   Clone + Debug + Hash + Ord + Send + Sync + Serialize + DeserializeOwned + 'static {}
impl<T> Key for T where T: /* same */ {}

pub trait Value: Clone + Debug + Hash + Send + Sync + Serialize + DeserializeOwned + 'static {}
impl<T> Value for T where T: /* same */ {}
```

Entry semantics travel with `Entry`, not with the `V` bound. `Value` carries no `PartialEq`: the
receive path's only "did this change?" question is a stamp comparison (`Entry::merge` returns
`other` exactly when the remote stamp is strictly greater), so `Timestamp: Ord` answers it without
ever needing to compare — or clone — the value itself.

### 3.9 Crate structure

The layers are separated into a workspace so that the inward dependency direction is enforced by the
compiler:

```
reconcile-core   // DOMAIN + PORTS: Clock, Persistence (traits);
                 //   Entry / State, Timestamp, Fingerprint, FingerprintTreeMap, the diff algorithm, LWW.
                 //   no infrastructure deps (no tokio, bincode, chrono-IO, ipnet, runtime rand);
                 //   serde + blake3, plus the dependency-light tracing / metrics facades.
reconcile-net    // ADAPTERS + I/O PORT: Transport (trait); UdpTransport, bincode.rs's encode/
                 //   decode_stream functions (no adapter type — §2.4), Authenticator, peer
                 //   discovery (gen_ip / ipnet / rand).
reconcile-store  // ADAPTERS: FileSnapshot / InMemory persistence.
reconcile        // WIRING + driving API: the Store facade, configuration, the HlcClock adapter
                 //   (which holds the chrono read), the tombstone wheel.
```

The `Clock` and `Persistence` ports are defined in `reconcile-core` (the domain-adjacent logic
injects them). The **`Transport` port is defined in `reconcile-net`**, not the core: it is consumed
by the UDP driver, which lives in `reconcile-net`, and the diff/merge domain does no I/O — this also
keeps `async_trait` out of the core. `bincode.rs`'s wire-encoding functions live in `reconcile-net`
too, rather than behind a port (§2.4). The chrono-reading `HlcClock` adapter lives
in `reconcile` (the `Timestamp` *type* and the pure HLC advance stay in the core, so the core carries no
`chrono`). Adapters depend on `reconcile-core`; infrastructure cannot be imported into the core
without a compile error — the guarantee a single crate cannot provide.

---

## 4. Current → target mapping

| Current | Target |
|---|---|
| `(Timestamp, Option<V>)` tuple | ✅ `Entry<Timestamp, V>` + `State<V>` domain type (`entry.rs`) |
| `MaybeTombstone`, `Timestamped` | ✅ inherent `Entry` methods / field access |
| `Reconcilable` (LWW over tuple) | ✅ `Entry::merge` (LWW); no `Resolve` seam needed yet |
| `Projectable` / `ValueOnly<V>` (dateless mirror) | ✅ `Entry::project` → `State<V>` (the projection cell *is* `State`) |
| `HashRangeQueryable`, `Diffable` (public) | inherent `FingerprintTreeMap` methods + `pub(crate)` diff functions |
| `pub mod diff` exposing wire types | `pub(crate)` `RangeAggregate` / `DiffRange` |
| `chrono::Utc` read in `clock.rs` | `Clock` port; `HlcClock` adapter holds the time read |
| `UdpSocket` in `reconcile_engine.rs` | ✅ `Transport` port (`transport.rs`); `UdpTransport` adapter (public, `ReconcileStore::new_with_transport` — D2) |
| `bincode` in `reconcile_engine.rs` | ✅ `encode`/`decode_stream` (`bincode.rs`), plain `pub(crate)` wire-encoding functions — no `Codec` trait and no `BincodeCodec` type (both dissolved, §2.4, same as `Diffable`/`Reconcilable`); `decode_stream`'s `max_items` cap closes the datagram-expansion DoS (#151) |
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
5. **Authenticate before deserialise** — the MAC is verified on raw bytes before the codec runs;
   `bincode.rs`'s `decode_stream` never absorbs authentication.
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

---

## 6. Migration sequence

The path from the current structure to the target is ordered by dependency. Each step is
behaviour-preserving and verified by the existing test suite, except the `Entry` step, which changes
the wire and on-disk formats (acceptable while the formats are unstable).

1. ✅ **Bound bundles & encapsulation** — introduce `Key` / `Value`; demote the protocol mechanism and
   other internals from `pub` to `pub(crate)`.
2. ✅ **Dissolve the diff traits** — remove `HashRangeQueryable` / `Diffable`; move `start_diff` /
   `diff_round` verbatim to `pub(crate)` functions over `FingerprintTreeMap`.
3. ✅ **`Clock` port** — extract `Clock`; `HlcClock` becomes its adapter and holds the physical-time
   read; the domain becomes time-source-free.
4. ✅ **`Entry` / `State` domain type** (done) — replace `(Timestamp, Option<V>)` with
   `Entry<Timestamp, V>` / `State<V>` (`entry.rs`); dissolve `MaybeTombstone` / `Timestamped` /
   `Reconcilable` / `Projectable` / `ValueOnly<V>`; `reconcile_engine.rs` and `reconcile_store.rs`
   now parameterize the engine over the plain `V` and construct `Entry<Timestamp, V>` /
   `State<V>` internally; `add_pre_insert`, `ReconcileMirror::add_on_update` and `PersistedState`
   carry `Entry<Timestamp, V>` / `State<V>` directly. *Changed the wire and on-disk formats*, as
   anticipated — acceptable pre-1.0.
5. ✅ **`Transport` port + `bincode.rs` wire-encoding functions** (done, issue #144) — extract the
   `Transport` port and the crate's wire-encoding functions; `UdpTransport` / `encode` /
   `decode_stream` (`transport.rs` / `bincode.rs`); the engine (`reconcile_engine.rs`) becomes a thin
   driver over them — it imports neither `tokio::net` nor `bincode` directly — with authentication
   ahead of the codec (invariant 5) and `decode_stream`'s `max_items` cap closing the
   datagram-expansion DoS (#151). `ReconcileEngine` holds the transport as
   `Arc<dyn Transport<Addr = SocketAddr>>`; wire encoding needs no field at all. The wire-encoding
   logic started life generic-over-a-`Codec`-trait like `Transport`, but the trait was dissolved as a
   follow-up (same PR-review pass that landed §2.4's writeup): with a single implementation, generic
   (non-object-safe) methods always carried as a type parameter, and no plausible alternate use, it
   did not earn its keep as a port — same call as `HashRangeQueryable`/`Diffable` (step 2) and
   `Reconcilable`/`MaybeTombstone` (step 4). The concrete replacement, a zero-sized `BincodeCodec`
   struct with inherent `encode`/`decode_stream` methods, was itself dissolved in a second
   review pass (still issue #144): stateless and never swapped, it was pure ceremony threaded
   through `Inner`, `SendPorts`, and every constructor to call what were effectively two static
   functions — so it became the plain `pub(crate) fn encode` / `fn decode_stream` free functions in
   `bincode.rs` (renamed from `codec.rs`, since there is no abstraction left to name).
   `ReconcileEngine`/`SendPorts` therefore take no codec type parameter and hold no codec field;
   call sites use `crate::bincode::encode`/`crate::bincode::decode_stream` directly.
   `ReconcileStore::new_with_transport` makes `Transport` consumer-wireable (infallible: the only
   fallible step in `new` is the socket bind, already done by a caller supplying their own
   transport) and `InMemoryNetwork`/`InMemoryTransport` are public, not test-gated, so downstream
   crates can drive a deterministic in-process cluster in their own tests; `bincode.rs`'s functions
   stay `pub(crate)` (internal mechanism, not a port — see §3.5). `Clock::node_id` was added so
   `ReconcileStore::node_id()` can read the identity back from the same adapter that stamps it,
   rather than caching it separately. `Value` dropped its `PartialEq` bound: the receive path's
   post-merge change detection is now a stamp comparison (`remote.stamp > local.stamp`, equivalent
   to `Entry::merge` under LWW) rather than an `Entry`/value equality check, which needed no bound on
   `V` and avoids a clone on the hot path. **Residual coupling**: `ReconcileMirror` (`mirror.rs`)
   still binds and owns its own UDP socket and calls `bincode` directly for its own receive loop —
   only its *send* calls were rewired onto the shared `Transport`-generic, `crate::bincode::encode`-using
   helpers. Routing the mirror's own socket onto an injectable `Transport` is deferred (tracked
   under issue #138, same as the workspace split below).
6. **Workspace split** — promote the layers to `reconcile-core` / `reconcile-net` /
   `reconcile-store` / `reconcile`; ports defined in the core; the core carries no infrastructure
   dependency.

Steps 1–3 are independent of the format change; step 4 is the single format-breaking step (now
done); step 5 is done; step 6 completes the boundary extraction and enforces it at the crate level.

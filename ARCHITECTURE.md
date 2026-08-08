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
| `rsos/src/fingerprint_tree_map.rs`, `rsos/src/fingerprint_tree_map_iter.rs`, `rsos/src/aggregate.rs` (standalone `rsos` crate, migration step 6 Step A, done) | `FingerprintTreeMap`: ordered map + range-fingerprint data structure and its iterators, the bundled `Aggregate` (Def. 3.5), plus the `Rsos<K>` trait (Def. 3.9, `Value` associated type) it implements. |
| `rsos/src/fingerprint.rs` (standalone `rsos` crate) | 256-bit additive fingerprint (`[u64; 4]`, per-element BLAKE3, add/sub mod 2²⁵⁶). |
| `rbsr/src/diff.rs`, `rbsr/src/rsos_view.rs` (standalone `rbsr` crate, migration step 6 Step B, done) | Anti-entropy algorithm (`start_diff`, `diff_round`) and its wire types, generic over the `RsosView<K>` read-only backend trait (four of Def. 3.9's seven operations), blanket-implemented for every `rsos::Rsos` implementor. |
| `lww-register/src/entry.rs` (standalone `lww-register` crate, migration step 6 Step C, done) | The `Entry`/`State` domain type: value/tombstone semantics and the conflict-resolution policy (migration step 4, done). |
| `lww-register/src/bounds.rs` | The `Key`/`Value` data-bound bundles (§3.8). |
| `lww-register/src/clock.rs` | Hybrid Logical Clock, ordering half: the `Timestamp` type, the `Clock` port, and the `advance`/`advance_past` HLC arithmetic. Reads no clock of its own. |
| `lww-register/src/persistence.rs` | Durability boundary: the `Persistence` port, the `PersistedState` snapshot type, and the non-durable `InMemoryPersistence` default. |
| `gossip/src/auth.rs` (standalone `gossip` crate, Step C, done) | Per-datagram message authentication (MAC) and optional AEAD encryption. |
| `gossip/src/replay.rs` | Per-sender sequence/freshness state: the anti-replay half of the same envelope. |
| `gossip/src/transport.rs` | The `Transport` port (datagram I/O) and its `UdpTransport` / `InMemoryTransport` adapters (migration step 5, done). |
| `gossip/src/bincode.rs` | The wire-encoding functions (no adapter type — migration step 5, done; see §2.4). |
| `gossip/src/discovery.rs`, `gossip/src/gen_ip.rs` | The `Discovery` port with its `RandomProbe`/`DnsDiscovery` adapters, and the address generation the former probes over. |
| `src/snapshot.rs` | `FileSnapshot`: the durable, atomically-replaced file adapter for the `Persistence` port, with its versioned on-disk header. Briefly a standalone `snapshot` crate (Step C); folded back into `reconcile` afterwards — see §3.9. |
| `src/clock.rs` | Hybrid Logical Clock, adapter half: `HlcClock`, the sole physical-time read, feeding `lww-register`'s ordering arithmetic. |
| `src/replica.rs` | Network orchestration: peer discovery, gossip, driving the `Transport` port and the `bincode` wire-encoding functions. |
| `src/replicated_map.rs` | Public facade tying the engine, the map, and timeouts together. |
| `src/read_replica_map.rs` | The dateless read-only `ReadReplicaMap`. |
| `src/timeout_wheel.rs` | Acknowledgement timeout tracking. |
| `src/observability.rs`, `src/prometheus.rs` | Metric call sites (no-ops without the `metrics` feature) and the optional Prometheus recorder/endpoint. |
| `src/persistence.rs`, `src/lib.rs` | Re-export shims keeping `reconcile::entry::Entry`, `reconcile::transport::UdpTransport`, `reconcile::FileSnapshot` etc. resolving after the split. |

### 2.2 The domain mechanism is already infrastructure-free

The data structure and the protocol algorithm carry **no infrastructure dependency** — they import
no async runtime, no socket, no codec, no wall clock (outside `#[cfg(test)]`):

| Module | Infrastructure imported |
|---|---|
| `rsos/src/fingerprint_tree_map.rs`, `rsos/src/fingerprint_tree_map_iter.rs`, `rsos/src/fingerprint.rs`, `rsos/src/aggregate.rs` (standalone `rsos` crate) | none — the crate's own minimal `Cargo.toml` dependency list (`arrayvec` + `blake3` + `range-cmp` + `serde` + `tracing`) makes an infrastructure import a compile error; the manifest itself is gated, see below. |
| `rbsr/src/diff.rs`, `rbsr/src/rsos_view.rs` (standalone `rbsr` crate) | none — likewise, from `Cargo.toml` (`rsos` + `serde` + `tracing`). |
| `lww-register/src/*.rs` (standalone `lww-register` crate) | none — its `Cargo.toml` names exactly one dependency, `serde` (derive only, for data shapes *other* crates encode), so `use tokio::…` there does not compile. |

This is, in effect, the interior of the hexagon and it exists today.

`./scripts/check-domain-purity.sh` gates both of those claims, in two parts that answer two different
questions. The division of labour is the point:

| Part | Question it answers | Scope |
|---|---|---|
| **Manifest gate** | *Can the forbidden thing be reached at all?* — is the dependency even declared | the `[dependencies]` / `[dev-dependencies]` / `[build-dependencies]` tables of `rsos/Cargo.toml`, `rbsr/Cargo.toml`, `lww-register/Cargo.toml`, which must name none of `tokio`, `bincode`, `chrono`, `ipnet`, `mio`, `reqwest`, `hyper`, `socket2`, `async-trait` (renamed `package = "…"` targets included) |
| **Source grep** | *Is an **allowed** dependency being used to reach something forbidden?* | every `lww-register/src/*.rs` file, for `tokio`/`bincode`/`chrono`/`ipnet`/`mio`/`reqwest`/`hyper`/`std::net` imports |

Neither subsumes the other. The manifest gate exists because that is the level at which the invariant
is actually breakable for these three crates: with the dependency undeclared, `use tokio::…` there
does not compile, so a source-level grep *for those crates* could never fire — it would read as
protection that is not there. The step that would make such an import compile is someone adding the
dependency, and that is what gets caught. Conversely a manifest cannot see an infrastructure type
reached through a *re-export* of an allowed dependency, which compiles fine and silently breaches the
boundary; the source grep covers that intra-crate sub-boundary, and lists every file in the crate
rather than a hand-picked subset — a new module in `lww-register` is domain by construction.
`gossip` and the root `reconcile` package (which holds the file-persistence adapter, `src/snapshot.rs`)
are deliberately outside the manifest gate: they are the adapters, and carrying infrastructure
dependencies is their job.

The source grep carries one documented carve-out: `std::net`'s
plain address *value* types (`IpAddr`, `SocketAddr`, …) are allowed, because `PersistedState`'s
causal-stability membership set is literally a set of peer identities and a peer's identity is its
address; the socket types (`UdpSocket`, `TcpStream`, `ToSocketAddrs`) are not, and a mixed import
such as `use std::net::{IpAddr, UdpSocket};` is still rejected. Making peer identity an opaque domain
newtype would remove the need for the carve-out — a separate change, not part of the split.

Widening or narrowing either set means updating `DOMAIN_FILES` / `STANDALONE_MANIFESTS` in the script
**and** this section together (AGENTS.md §9.2).

### 2.3 Infrastructure coupling

Migration step 5 (§6) extracted the `Transport` port and the `bincode.rs` wire-encoding functions, so
`replica.rs` no longer imports `tokio::net` or `bincode` directly — it drives the gossip
protocol purely over `Transport` (default adapter `UdpTransport`) and `gossip::bincode::{encode,
decode_stream}`. The remaining infrastructure dependencies:

| Module | Infrastructure imported directly | Notes |
|---|---|---|
| `src/clock.rs` | `chrono::Utc` — the sole physical-time read | The `Clock` port's adapter (`HlcClock`). Step C split the module: the `Timestamp` type, the port and the HLC ordering arithmetic went to `lww-register`, this adapter stayed — so the wall-clock read is now *outside* the domain, which is the intended location, not residual coupling. |
| `gossip/src/transport.rs` | `tokio::net::UdpSocket`, `socket2` | The `Transport` port's adapter — this is the *intended* location for this dependency, not residual coupling. |
| `gossip/src/bincode.rs` | `bincode` | The wire-encoding functions — likewise intended; not a port (§2.4). |
| `gossip/src/auth.rs`, `gossip/src/replay.rs` | `chrono::Utc`, MAC/AEAD backends | Datagram sealing and per-sender freshness stamps — intended, in the adapter layer. |
| `snapshot/src/lib.rs` | `std::fs`, `bincode` | The `Persistence` port's durable adapter — intended. |
| `src/replica.rs` | `tokio::time`, `rand::StdRng`, `ipnet` | Timer/backoff, discovery randomness, and CIDR geography — not yet ported (no port is warranted: these are not swappable infrastructure boundaries the way I/O is). |
| `src/replicated_map.rs` | `chrono`, `ipnet` | Tombstone-expiry wall-clock reads and CIDR geography. |
| `src/read_replica_map.rs` | `tokio::time`, `rand::StdRng`, `ipnet` | Same three as `replica.rs`, for the same reasons. The read replica's socket and codec coupling is **closed**: it holds an `Arc<dyn Transport<Addr = SocketAddr>>` like the engine, drives both directions over that port and over `gossip::bincode::{encode, decode_stream}`, and names `tokio::net` only through `UdpTransport::bind` in its default constructor. `ReadReplicaMap::new_with_transport` mirrors `ReplicatedMap::new_with_transport`, so a read replica converges against a dated peer with no real sockets (`tests/read_replica_map.rs::read_replica_converges_with_dated_store_over_in_memory_transport`). |

There is no longer an abstraction boundary missing between the dated engine and the transport: it is
a substitutable adapter, and the in-memory `Transport` (`InMemoryNetwork`/`InMemoryTransport`) paired
with the wire-encoding functions makes the engine's gossip protocol exercisable with no real sockets.
Wire encoding itself has a single implementation and is not behind a port (§2.4). Step C closed the
wall-clock item by splitting `clock.rs` along the port boundary, and the read replica's socket stack —
the last open item — has since been routed onto the same `Transport` port, so no module in
`reconcile` reaches for `tokio::net` or `bincode` outside of `#[cfg(test)]` code and the default
`UdpTransport::bind` call in each constructor.

### 2.4 Trait landscape

Nine traits exist across the five crates. They fall into four groups:

- **Boundary abstractions — genuine ports (4).** `Persistence` (`lww-register/src/persistence.rs`):
  two implementations (`InMemoryPersistence`, `snapshot::FileSnapshot`). `Clock`
  (`lww-register/src/clock.rs`): the physical-time read behind a port, adapter `HlcClock`.
  `Transport` (`gossip/src/transport.rs`): object-safe, two real implementations (`UdpTransport`,
  `InMemoryTransport` — the latter genuinely needed so tests drive a cluster with no real sockets).
  `Discovery` (`gossip/src/discovery.rs`): `RandomProbe` + `DnsDiscovery`.
- **Contract of the data structure (2).** `rsos::Rsos<K>` (`rsos/src/rsos_trait.rs`) states Def. 3.9's
  seven RSOS operations; `rbsr::RsosView<K>` (`rbsr/src/rsos_view.rs`) is the read-only four-operation
  subset the diff walk needs, blanket-implemented over every `Rsos`. These are not ports in the
  hexagonal sense — they are the literature's contract, written down so `rbsr` is generic over the
  backend rather than welded to `FingerprintTreeMap`.
- **Bound bundles and compile-time selection (3).** `Key` / `Value` (`lww-register/src/bounds.rs`)
  bundle the multi-bound `where` blocks; `Mac` (`gossip/src/auth.rs`) selects a MAC backend chosen at
  compile time.
- **Value-shape helpers — dissolved (migration step 4, done).** `MaybeTombstone`, `Reconcilable` and
  `Timestamped` used to each carry a single implementation over the untyped `(Timestamp, Option<V>)`
  tuple that represented the stored cell. They have been replaced by the `Entry<Timestamp, V>` /
  `State<V>` domain type (`entry.rs`, §3.6): `Entry::is_tombstone` / `Entry::value` replace
  `MaybeTombstone`, `Entry::merge` replaces `Reconcilable`, and direct `.stamp` field access replaces
  `Timestamped`. `HashRangeQueryable` / `Diffable` went the same way in step 2 (§6), replaced by
  `FingerprintTreeMap`'s inherent methods and, since step 6, by `Rsos` / `RsosView` above.

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
(`replica.rs`) and `read_replica_map.rs` call `gossip::bincode::encode` /
`gossip::bincode::decode_stream`
directly. `Transport`, by contrast, remains a genuine port (§3.4/§3.5): object-safe, `pub`, and with
two real implementations (`UdpTransport`, `InMemoryTransport`).

Consequence of the (now-superseded) tuple representation, addressed by the same step:

- The internal tuple used to leak onto the public surface — into the `add_pre_insert` hook and into
  `PersistedState`. Both now carry `Entry<Timestamp, V>` directly (`replicated_map.rs`,
  `persistence.rs`).

#### Future extension points (tracked, not implemented)

The Meyer/Willow-ecosystem reference implementation of range-based set reconciliation
(`github.com/earthstar-project/range-reconcile`) documents three "Bring Your Own …" extension
points: `BYOTransport`, `BYOLiftingMonoid`, `BYOEncoding`. Two of them are worth recording here.

- **A public `Encoding` port (`BYOE`) — a candidate *if* an external consumer ever needs pluggable
  wire formats.** It is deliberately absent today. Compare it with `Transport`, which *is* a real
  port: two real implementations, and the second one (`InMemoryTransport`) is not decorative — it is
  how the gossip protocol gets exercised without real sockets. Wire encoding has exactly one
  implementation and no test-driven need: `bincode.rs`'s own tests call `encode` / `decode_stream`
  directly, with no fake. The reason Meyer's implementation *does* need `BYOE` is structural, not a
  difference of taste: it is a generic library meant to embed into arbitrary host protocols that
  already own a wire format, whereas `reconcile-rs` owns its wire format end to end and has no
  external interop requirement. Reintroducing the port later is additive and non-breaking — bincode
  simply becomes the default implementation behind the trait — so the cost of waiting for a real
  second consumer is low. Until one asks, the codebase's standing convention against speculative
  single-implementation abstractions wins; `Codec` and `Diffable` were dissolved above for exactly
  this reason, and re-adding a third instance of the same shape would contradict both.
- **`BYOLiftingMonoid` names the generic-summary generalization.** `rsos` today hardwires its range
  summary to the 256-bit BLAKE3 `Fingerprint`. Generalizing it to an arbitrary summary monoid is
  already tracked as a future gap in [`SOTA.md`](./SOTA.md) (P0/F6); `lift` / `combine` / `neutral`
  is the right vocabulary for it when that work happens — the term comes from the same reference
  implementation, so adopting it costs nothing and buys shared language with the literature. Not in
  scope here: `Rsos::aggregate` / `RsosView::aggregate` keep the concrete `(usize, Fingerprint)`
  return type until a second summary type actually exists.

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
engine and `read_replica_map.rs` call the plain `gossip::bincode::encode` /
`gossip::bincode::decode_stream`
functions directly rather than naming a `Codec` trait or a codec value.

Message authentication (`Authenticator` / MAC) sits **ahead of** the codec: the MAC is verified on
raw bytes before any decoding occurs. It is never folded into `bincode.rs`.

The ports make the domain testable in isolation: an in-memory `Transport` (`InMemoryNetwork` /
`InMemoryTransport`, `transport.rs`) and a fixed `Clock` make convergence and HLC behaviour
deterministic without real sockets or wall-clock time.

**Visibility asymmetry (`Transport` public, `bincode.rs` `pub(crate)`).** `Transport` is
consumer-wireable: `ReplicatedMap::new_with_transport` takes any
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

The same step also absorbs the **value-only projection** that powers the dateless `ReadReplicaMap`.
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
compiler. With migration step 6 (Steps A–D, §6) landed, this is **the shape of the tree today**, not
a target:

```
rsos           // DATA STRUCTURE: the Rsos<K> trait (Def. 3.9's seven operations, with an
               //   associated Value type) and its one realization, FingerprintTreeMap<K, V>, plus
               //   Fingerprint and the bundled Aggregate (Def. 3.5). Zero workspace deps.
rbsr           // ALGORITHM: start_diff / diff_round / RangeAggregate, generic over RsosView<K>
               //   (four of the seven operations), blanket-implemented for every rsos::Rsos.
               //   Depends on rsos only.
lww-register   // DOMAIN + PORTS: Entry / State, Timestamp + the Clock port + the HLC ordering
               //   arithmetic, the Persistence port + PersistedState + InMemoryPersistence,
               //   Key / Value. One dependency: serde's derive. No tokio / bincode / chrono / ipnet
               //   — the manifest itself makes that a compile error (§2.2).
gossip         // ADAPTERS + I/O PORT: Transport (trait); UdpTransport / InMemoryTransport, the
               //   encode / decode_stream wire functions (no adapter type — §2.4), Authenticator,
               //   replay protection, the Discovery port with RandomProbe / DnsDiscovery
               //   (gen_ip / ipnet / rand). Does NOT depend on lww-register.
reconcile      // WIRING + driving API: ReplicatedMap, Replica, ReadReplicaMap, configuration, the
               //   HlcClock adapter (which holds the chrono read), FileSnapshot (the durable half
               //   of persistence, src/snapshot.rs), the tombstone wheel, observability.
               //   Depends on all four, and re-exports their public types.
```

**`snapshot` was a sixth crate, briefly.** Step C carved `FileSnapshot` out alongside `gossip`, and a
follow-up folded it back into `reconcile` as `src/snapshot.rs`. That is a decision, not a mistake
being undone: a crate boundary has to buy something, and this one bought nothing. `FileSnapshot` is a
single type with no reuse value away from this workspace (it persists `PersistedState`, and nothing
else), no consumer that would take it without `reconcile`, and — once publishing became concrete
(#204) — no available name on crates.io either. The other four boundaries each pay for themselves:
`rsos` and `rbsr` are genuinely reusable on their own terms, `lww-register` buys the compiler-enforced
infrastructure-free domain (§2.2), `gossip` buys the "knows nothing about `Entry`" property above.
Persistence keeps its architectural split regardless — the `Persistence` port and `PersistedState`
stay in `lww-register`, so the domain still touches no filesystem, and the file adapter still cannot
be imported into it. Only the packaging changed.

The `Clock` and `Persistence` ports are defined in `lww-register` (the domain-adjacent logic injects
them). The **`Transport` port is defined in `gossip`**, not the domain crate: it is consumed by the
UDP driver, and the diff/merge domain does no I/O — this also keeps `async_trait` out of the domain.
The wire-encoding functions live in `gossip` too, rather than behind a port (§2.4).

`gossip` carries this further than the earlier `reconcile-net` sketch anticipated: it has **no
dependency on `lww-register` at all**. Nothing in the transport/auth/replay/discovery layer knows
what an `Entry`, a `Timestamp` or a `Key` is — a datagram is a byte slice, a peer is an address. So
`gossip` is a core-independent *sibling* rather than a layer above the domain, and `reconcile` is the
one place the two meet. Adding that edge is the signal that something landed in the wrong crate.

The chrono-reading `HlcClock` adapter lives in `reconcile` (the `Timestamp` *type*, the `Clock` port
and the pure HLC advance stay in `lww-register`, so the domain carries no `chrono`). Adapters depend
on `lww-register`; infrastructure cannot be imported into it without a compile error — the guarantee
a single crate could not provide.

---

## 4. Current → target mapping

| Current | Target |
|---|---|
| `(Timestamp, Option<V>)` tuple | ✅ `Entry<Timestamp, V>` + `State<V>` domain type (`entry.rs`) |
| `MaybeTombstone`, `Timestamped` | ✅ inherent `Entry` methods / field access |
| `Reconcilable` (LWW over tuple) | ✅ `Entry::merge` (LWW); no `Resolve` seam needed yet |
| `Projectable` / `ValueOnly<V>` (dateless read replica) | ✅ `Entry::project` → `State<V>` (the projection cell *is* `State`) |
| `HashRangeQueryable`, `Diffable` (public) | inherent `FingerprintTreeMap` methods + `pub(crate)` diff functions |
| `pub mod diff` exposing wire types | `RangeAggregate` / `DiffRange`, with private fields, in `rbsr` |
| `chrono::Utc` read in `clock.rs` | ✅ `Clock` port (`lww-register`); the `HlcClock` adapter in `src/clock.rs` holds the time read |
| `UdpSocket` in `replica.rs` | ✅ `Transport` port (`gossip/src/transport.rs`); `UdpTransport` adapter (public, `ReplicatedMap::new_with_transport` — D2) |
| `bincode` in `replica.rs` | ✅ `encode`/`decode_stream` (`gossip/src/bincode.rs`), plain wire-encoding functions — no `Codec` trait and no `BincodeCodec` type (both dissolved, §2.4, same as `Diffable`/`Reconcilable`); `decode_stream`'s `max_items` cap closes the datagram-expansion DoS (#151) |
| `Persistence` | unchanged (the model port) |
| `gen_ip` random IP-scan inline in the engine | `Discovery` port; `RandomProbe` (speculative, the default) + `DnsDiscovery` (authoritative, k8s-native) adapters |
| Multi-bound `where` blocks | `Key` / `Value` supertrait bundles |
| Single crate | ✅ five-crate workspace `rsos` / `rbsr` / `lww-register` / `gossip` / `reconcile` (steps A–D done; §3.9, §6). Step C's sixth crate, `snapshot`, was deliberately folded back into `reconcile` as `src/snapshot.rs` — §3.9 |

---

## 5. Invariants

The following load-bearing properties are preserved across any restructuring; they encode the
correctness and security guarantees tracked in [`PROGRESS.md`](./PROGRESS.md).

1. **Fingerprint format & arithmetic** — `[u64; 4]`, per-element BLAKE3, add/sub mod 2²⁵⁶; the
   golden vectors in `fingerprint.rs` hold.
2. **HLC total order** `(wall_ms, counter, node_id)` (`clock.rs:44-54`) — the derived ordering *is* the
   conflict order; the `Clock` port mints `Timestamp` directly, preserving it, and merge uses strict `>`.
3. **Size-not-hash emptiness/equality** in `diff_round` (`rbsr/src/diff.rs`).
4. **Malformed-bound / inverted-range hardening** in `diff_round` (`rbsr/src/diff.rs`).
5. **Authenticate before deserialise** — the MAC is verified on raw bytes before the codec runs;
   `bincode.rs`'s `decode_stream` never absorbs authentication.
6. **Causal-stability tombstone gate** — a tombstone is garbage-collected only after every monotonic
   cluster member has acknowledged the exact version hash. Dynamic discovery (e.g. `DnsDiscovery`)
   only ever feeds the gossip-target `peers` set, **never** the `members` set: membership stays
   earned by an authenticated dated datagram, so a discovered (unverified) address can neither block
   GC nor be the subject of a GC release. Decommissioning a member that has vanished from discovery
   uses the same `decommission_peer` escape hatch as `forget_peer`.
7. **`version_hash` determinism** (`replica.rs:55`) — preserved as `Entry` derives `Hash`.
8. **Value-only projection hash is timestamp-less** — the dated `Entry` hashes with its `stamp`
   (feeding `version_hash`), while its `State<V>` projection hashes the value alone, so a dated
   store and a dateless `ReadReplicaMap` compute identical per-element fingerprints. Guarded by
   `read_replica_map.rs::value_fingerprint_is_timestamp_independent`.

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
   `Reconcilable` / `Projectable` / `ValueOnly<V>`; `replica.rs` and `replicated_map.rs`
   now parameterize the engine over the plain `V` and construct `Entry<Timestamp, V>` /
   `State<V>` internally; `add_pre_insert`, `ReadReplicaMap::add_on_update` and `PersistedState`
   carry `Entry<Timestamp, V>` / `State<V>` directly. *Changed the wire and on-disk formats*, as
   anticipated — acceptable pre-1.0.
5. ✅ **`Transport` port + `bincode.rs` wire-encoding functions** (done, issue #144) — extract the
   `Transport` port and the crate's wire-encoding functions; `UdpTransport` / `encode` /
   `decode_stream` (`transport.rs` / `bincode.rs`); the engine (`replica.rs`) becomes a thin
   driver over them — it imports neither `tokio::net` nor `bincode` directly — with authentication
   ahead of the codec (invariant 5) and `decode_stream`'s `max_items` cap closing the
   datagram-expansion DoS (#151). `Replica` holds the transport as
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
   `Replica`/`SendPorts` therefore take no codec type parameter and hold no codec field;
   call sites use `gossip::bincode::encode`/`gossip::bincode::decode_stream` directly.
   `ReplicatedMap::new_with_transport` makes `Transport` consumer-wireable (infallible: the only
   fallible step in `new` is the socket bind, already done by a caller supplying their own
   transport) and `InMemoryNetwork`/`InMemoryTransport` are public, not test-gated, so downstream
   crates can drive a deterministic in-process cluster in their own tests; `bincode.rs`'s functions
   stay `pub(crate)` (internal mechanism, not a port — see §3.5). `Clock::node_id` was added so
   `ReplicatedMap::node_id()` can read the identity back from the same adapter that stamps it,
   rather than caching it separately. `Value` dropped its `PartialEq` bound: the receive path's
   post-merge change detection is now a stamp comparison (`remote.stamp > local.stamp`, equivalent
   to `Entry::merge` under LWW) rather than an `Entry`/value equality check, which needed no bound on
   `V` and avoids a clone on the hot path. The one item this step left open — `ReadReplicaMap` still
owning
   a raw `tokio::net::UdpSocket` and calling `bincode` directly on its receive path, with only its
   *send* calls rewired onto the shared helpers — was closed in step 7 below.
6. ✅ **Workspace split** (done) — promote the layers to a multi-crate workspace so the inward
   dependency direction is enforced by the compiler rather than by review. The crate names replace
   the earlier `reconcile-core` / `-net` / `-store` sketch: each says what the crate *is* rather than
   prefixing the project name onto every layer (§3.9). It landed as four independently-green
   sub-steps:

   - ✅ **A — extract `rsos`.** The tree, its iterators and `fingerprint.rs` moved into a new leaf
     crate as `fingerprint_tree_map.rs` / `fingerprint_tree_map_iter.rs` / `fingerprint.rs`;
     `HRTree` renamed `FingerprintTreeMap` across the tree (types, doc comments, prose), with
     `insertion_position` / `key_at` / `get_range` promoted to `pub` and aligned on Def. 3.9's own
     terms as `rank` / `select` / `range`. Added `aggregate(range) -> Aggregate`
     (`rsos/src/aggregate.rs`), the paper's Def. 3.5 *bundled* aggregate as a named monoid — one
     tree walk producing the element count and the range fingerprint together, replacing the
     separate `hash`/`tree_size` pair. Defined the `Rsos<K, V>` trait stating Def. 3.9's seven
     operations and implemented it for `FingerprintTreeMap`. The root `Cargo.toml` became a
     workspace manifest.
   - ✅ **B — extract `rbsr`.** `proto.rs` moved into a second leaf-ish crate (`diff.rs` +
     `rsos_view.rs`), depending on `rsos` only. Introduced `RsosView<K>`, the read-only
     four-operation subset of Def. 3.9 that the diff walk actually needs, blanket-implemented over
     every `rsos::Rsos`, and regenericized `start_diff` / `diff_round` over it. `Rsos<K, V>` became
     `Rsos<K>` with an associated `Value` type, so a backend — not the caller — names its own value
     type.
   - ✅ **C — split `lww-register` / `gossip` / `snapshot`.** The domain (`entry.rs`, `bounds.rs`,
     `Timestamp` + the `Clock` port + the HLC ordering arithmetic, the `Persistence` port +
     `PersistedState` + `InMemoryPersistence`) went to `lww-register`; the adapter layer
     (`transport.rs`, `bincode.rs`, `auth.rs`, `replay.rs`, `discovery.rs`, `gen_ip.rs`) to
     `gossip`, which deliberately took **no** `lww-register` dependency; `FileSnapshot` to
     `snapshot` (a boundary later withdrawn on purpose — step 9 below). Landed together, since the
     three are interdependent. The type renames
     `ReconcileEngine`→`Replica`, `ReconcileStore`→`ReplicatedMap`, `ReconcileMirror`→`Mirror` rode
     along in the same pass, the files being moved anyway. (`Mirror` was itself renamed again, to
     `ReadReplicaMap`, in the follow-up naming pass — step 8 below.)
   - ✅ **D — reassemble `reconcile` as the facade; CI, scripts, docs.** What stayed: `replica.rs`,
     `replicated_map.rs`, `read_replica_map.rs`, `observability.rs`, `prometheus.rs`,
     `timeout_wheel.rs`, and the chrono-reading `HlcClock` adapter split out of `clock.rs`; `lib.rs` plus the
     `persistence.rs` / `clock.rs` shims became a re-export surface, so `reconcile::ReplicatedMap`,
     `reconcile::Entry`, `reconcile::FingerprintTreeMap` & co. keep resolving for existing consumers.
     CI and `./pre-commit` normalized `--all` → `--workspace`; `check-domain-purity.sh` gained the
     manifest gate over `rsos` / `rbsr` / `lww-register` (§2.2); this section and §2.4 were brought
     up to date.

7. ✅ **Route the dateless read replica onto the `Transport` port** (done) — the last item carried
   over from step 5. `ReadReplicaMap` now holds an `Arc<dyn Transport<Addr = SocketAddr>>` instead of
   an `Arc<UdpSocket>`,
   receives through `Transport::recv_from`, decodes through `gossip::bincode::decode_stream` under
   the same `MAX_MESSAGES_PER_DATAGRAM` bound as the engine, and reuses the engine's `SendPorts` /
   `send_messages_to` / `send_to_retry` on the way out; `read_replica_map.rs` imports neither
   `bincode` nor `tokio::net`. `ReadReplicaMap::new` keeps its signature (it binds a `UdpTransport`
   internally), and the new `ReadReplicaMap::new_with_transport` matches
   `ReplicatedMap::new_with_transport`. The authentication
   ordering is unchanged — `Authenticator::open` still runs on the raw datagram bytes before any
   decode (invariant #5, §5) — as are the read replica's read-only semantics. Two behavioural details
   moved deliberately: a *malformed* datagram is now dropped whole instead of applying its
   well-formed prefix (the engine's long-standing policy, strictly more conservative), and message
   count per datagram is now explicitly bounded.

8. ✅ **`Mirror` → `ReadReplicaMap`** (done) — a pure rename closing the naming pass that step 6
   started. `Mirror` was the one public type named after a generic metaphor rather than domain or
   literature vocabulary; the type *is* a passive, dateless, read-only replica of a dated
   `ReplicatedMap` — a **read replica** in the standard database sense — so it now carries that name,
   parallel to `ReplicatedMap`. `src/mirror.rs` → `src/read_replica_map.rs`, `tests/mirror.rs` →
   `tests/read_replica_map.rs`; no deprecated alias (pre-1.0, same as
   `ReconcileEngine`→`Replica` / `ReconcileStore`→`ReplicatedMap`). No behaviour, wire format or
   on-disk format changed.

9. ✅ **Fold `snapshot` back into `reconcile`** (done) — the workspace went from six crates to five.
   `snapshot/src/lib.rs` became `src/snapshot.rs`, a private module of the facade whose one public
   type is re-exported exactly where it always was (`reconcile::FileSnapshot`,
   `reconcile::persistence::FileSnapshot`); its six unit tests came along, including the two that
   need `bincode` — free in `reconcile`, and the reason they could never live in `lww-register`.
   Nothing about the public surface, the wire format or the on-disk format changed.

   This is a deliberate follow-up decision, not a step-6 mistake being reverted. Step 6 was right to
   *separate* the durable adapter from the domain — that separation still stands, it is what keeps
   `std::fs` and `bincode` out of `lww-register`. What did not survive scrutiny was giving that
   adapter its own **crate**: a crate boundary should buy compiler-enforced independence, reuse, or
   a publishable identity, and this one bought none of the three. `FileSnapshot` is a single type
   that serializes `PersistedState` and is useful to nobody who is not already using `reconcile`;
   the boundary it needed was a module boundary. Making publishing concrete (#204) sharpened the
   point — `snapshot` is taken on crates.io, and the workspace would have been renaming a crate that
   should not have been published in the first place. The other four boundaries were re-examined the
   same way and all kept: see §3.9.

Steps 1–3 are independent of the format change; step 4 is the single format-breaking step (now
done); step 5 is done; step 6 completed the boundary extraction and now enforces it at the crate
level; step 7 closed the last residual coupling; step 8 was a naming-only follow-up; step 9 tuned
step 6's crate granularity back down by one.

**Is #138 complete?** Yes. The structural migration was complete after step 6 — every layer is its
own crate, every port is defined on the right side of the boundary, and the domain's
infrastructure-freedom is a compile error rather than a convention (§2.2) — and step 7 closed the
one remaining item, `src/read_replica_map.rs`'s own socket and codec calls. Grepping the `reconcile`
crate for
`tokio::net` / `UdpSocket` / `bincode` now returns only `#[cfg(test)]` code and the `UdpTransport`
default-adapter constructions, which are the intended locations. What remains outside the port
system (`tokio::time` timers, `rand::StdRng` discovery randomness, `ipnet` CIDR geography in
`replica.rs` / `replicated_map.rs` / `read_replica_map.rs`) is deliberately not ported: these are not
swappable infrastructure boundaries the way datagram I/O is (§2.3).

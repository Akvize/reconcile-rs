# Architecture review & refonte — hexagonal target (`reconcile-rs`)

> Forward-looking **architecture** document. Organizing principle: **hexagonal architecture
> (ports & adapters)**. The goal is not to add or remove traits for their own sake, but to put
> *well-designed* traits where they belong — at the boundary between the domain and the
> infrastructure — and to stop exposing internal mechanism as public API.
>
> - **Date:** 2026-06-02 · **Manifest:** `0.0.0-git` (unpublished; API and on-wire/on-disk formats
>   may be broken freely).
> - **Scope:** architecture, contracts, coupling. Not a defect review — for correctness/security
>   see [`REVIEW.md`](./REVIEW.md), whose critical findings are already fixed in the current tree
>   (256-bit BLAKE3 fingerprint, HLC, per-datagram MAC, causal-stability tombstone gate,
>   `checked_sub` hardening). This document assumes that corrected baseline.
> - **Method:** static reading of `src/`, `tests/`, `Cargo.toml`. Every claim carries a `file:line`
>   proof. No source file is modified by this document.

---

## 1. Reframing: the problem is not *how many* traits, it's *which* traits and *where*

A trait is only worth its weight when it is a **port** — a domain-revealing boundary that inverts a
dependency on infrastructure. The current codebase has the two failure modes that make a trait set
feel like an "usine à gaz", and they are *opposite* problems:

1. **Internal mechanism exposed as public API.** `HashRangeQueryable`, `Diffable` and
   `MaybeTombstone` describe *how the engine computes a diff* or *the shape of a stored tuple*.
   They are plumbing — **not "screaming", not domain-revealing** — yet they are published
   (`lib.rs:30` `pub mod diff`) and carry ceremony (vacuous associated types). Exposing them is the
   defect: they leak the mechanism and pin the public surface to it.
2. **Missing ports.** The domain has *no* boundary against infrastructure. The reconciliation
   engine depends **directly** on `tokio::net::UdpSocket`, `bincode`, `rand::StdRng`, `ipnet`, and
   the HLC reads `chrono::Utc` itself. There is no dependency inversion, so the domain cannot be
   exercised without real sockets and real wall-clock time, and the transport/codec/clock cannot be
   swapped.

The refonte fixes both: **hide and dissolve the plumbing traits**, and **introduce the handful of
ports the domain actually needs** (`Clock`, `Transport`, `Codec`, `Persistence`).

---

## 2. Critical review

### 2.1 The coupling map — where infrastructure leaks into the domain

`grep` of infrastructure imports across the modules (audited tree):

| Module | Infra it imports directly | Verdict |
|---|---|---|
| `hrtree.rs`, `hrtree_iter.rs`, `fingerprint.rs`, `diff.rs`, `reconcilable.rs` | none (only `rand` inside `#[cfg(test)]`) | **already infra-free** — this is the hexagon interior, it exists today |
| `hlc.rs` | `chrono::Utc` (`hlc.rs:35`) — physical time read inside the domain | one small leak: the *time source* |
| `reconcile_engine.rs` | `tokio::net::UdpSocket`, `tokio::time`, `bincode::{Serializer,Deserializer,DefaultOptions}`, `rand::StdRng`, `ipnet` (`reconcile_engine.rs:21-28,67`) | **infrastructure-soaked** — transport + codec + discovery all hard-wired |
| `reconcile_store.rs` | `chrono`, `ipnet` (`reconcile_store.rs:19-20`) | facade, leaks time + addressing |

The domain mechanism (HRTree, fingerprint, diff algorithm) is **already clean**. The whole infra
dependency is concentrated in one god-module, `reconcile_engine.rs`, plus the wall-clock read in
`hlc.rs`. That is precisely the surface a port extraction targets.

### 2.2 Trait-by-trait classification

Three distinct categories, three distinct fixes — *not* "delete all single-impl traits".

| Trait | `file:line` | What it really is | Fix |
|---|---|---|---|
| `Diffable` | `diff.rs:58` | **Plumbing**, wrongly public. Blanket impl, associated types always `HashSegment<K>`/`DiffRange<K>` — vacuous. Never hand-implemented. | **Delete the trait.** `start_diff`/`diff_round` become `pub(crate)` free functions over the concrete `HRTree`. |
| `HashRangeQueryable` | `diff.rs:30` | **Plumbing**, wrongly public. One real impl (`HRTree`); the only other is a test `MockStore`. Describes the data structure's internal capability, not a domain boundary. | **Delete the trait.** Inherent methods on `HRTree`; the diff test exercises `HRTree` directly. |
| `MaybeTombstone` | `reconcilable.rs:43` | **Value-shape plumbing.** One impl on `(Hlc, Option<V>)`. Not a port. | Dissolve into a **domain type** `Entry`/`State` with `is_tombstone()` inherent. |
| `Reconcilable` | `reconcilable.rs:27` | **Domain policy** (LWW), but mis-shaped as a tuple trait. | Inherent `Entry::merge` (default LWW); optional `Resolve` *domain* seam only if multiple policies become a real goal (§3.5). |
| `Timestamped` | `hlc.rs:171` | **Value-shape plumbing.** One impl on `(Hlc, V)`. | Field access on `Entry` (`entry.stamp`). |
| `Mac` | `auth.rs:92` | Compile-time-selected backend (`mac-blake3`/`mac-hmac`), never both at once. | Borderline. Keep as a small `pub(crate)` contract *or* collapse to a `cfg`-selected concrete `ClusterMac`. Not a public port. |
| **`Persistence`** | `persistence.rs:77` | **A real port.** Two adapters (`InMemory`, `FileSnapshot`) + genuine external extension need. Domain-revealing. | **Keep — this is the model every other port should follow.** |

So the existing trait set has exactly **one well-formed port** (`Persistence`) and a pile of
exposed mechanism. The refonte keeps `Persistence`, **adds the missing ports** (`Clock`,
`Transport`, `Codec`), and **hides/dissolves the rest**.

### 2.3 Verbosity and leaks (orthogonal, still real)

- The 9-bound key constraint and 11-bound value constraint are spelled out at every impl site
  (`reconcile_engine.rs:122-135`, `reconcile_store.rs:75`, the `get_mut` impl). No bound bundle.
- `MaybeTombstone + Reconcilable + Timestamped` sit inside a *value* bound, although they are
  properties of the **entry**, not of any plain value.
- The internal wrapper `(Hlc, Option<V>)` leaks into the **public** hook
  `add_pre_insert<F: … Fn(&K, &(Hlc, Option<V>)) …>` (`reconcile_store.rs:171`) and into
  `PersistedState`/`DatedEntries` (`persistence.rs:51`). The mechanism is on the public surface.

---

## 3. Target architecture: the hexagon

```
                         ┌─────────────────────── adapters (infra) ───────────────────────┐
   driving side          │                                                                │
   (application)         │   HlcClock        UdpTransport      BincodeCodec    FileSnapshot│
        │                │  (chrono/time)   (tokio/UDP)        (bincode)     / InMemory     │
        ▼                │       │               │                 │              │         │
  ┌───────────┐  impl    └───────┼───────────────┼─────────────────┼──────────────┼─────────┘
  │  Store    │           ports: │ Clock         │ Transport       │ Codec        │ Persistence
  │  (facade) │◀──────────────── ▼ ───────────── ▼ ─────────────── ▼ ──────────── ▼ ────────
  └───────────┘                 ┌──────────────────────────────────────────────────────────┐
        ▲                       │                    DOMAIN  (hexagon)                       │
        │  inbound/driving port │  reconciliation algorithm · conflict policy (LWW)          │
        └───────────────────────│  tombstone lifecycle · HRTree + Fingerprint (mechanism)    │
                                │  Hlc (value) · Entry/State (value)  —  NO tokio/bincode    │
                                └──────────────────────────────────────────────────────────┘
```

- **Domain (interior):** the reconciliation diff algorithm, the conflict-resolution policy, the
  tombstone lifecycle + causal-stability rule, the `HRTree` and `Fingerprint` (internal mechanism),
  and the value types `Hlc`, `Entry`, `State`. **Depends on no infrastructure crate.** The grep in
  §2.1 shows this is *almost already true* — only the wall-clock read needs to move out.
- **Outbound (driven) ports** — the well-designed traits the domain depends on:
  `Clock`, `Transport`, `Codec`, `Persistence`.
- **Adapters** implement the ports: `HlcClock` (system time), `UdpTransport` (tokio), `BincodeCodec`
  (bincode), `FileSnapshot`/`InMemory` (durability).
- **Inbound (driving) port:** the `Store` API the application calls (insert/remove/get/run).

The hexagonal payoff is concrete and testable: an in-memory `Transport` adapter makes the
convergence proptest deterministic (no real sockets); a fixed `Clock` makes HLC behaviour
reproducible.

### 3.2 The ports (well-designed, domain-revealing, the only public traits)

```rust
// ── Clock ─────────────────────────────────────────────────────────────────────
// Removes the domain's dependency on chrono::Utc (hlc.rs:35). The HLC algorithm
// stays domain logic; only the physical-time source crosses the boundary.
pub trait Clock: Send + Sync + 'static {
    type Timestamp: Copy + Ord + Hash + Serialize + DeserializeOwned + Send + Sync + 'static;
    /// Mint a fresh, strictly-monotonic local timestamp.
    fn now(&self) -> Self::Timestamp;
    /// Advance past a timestamp received from a peer (causality).
    fn observe(&self, remote: Self::Timestamp);
}

// ── Transport ───────────────────────────────────────────────────────────────────
// Removes the dependency on tokio::net::UdpSocket. Datagram in / datagram out.
#[async_trait::async_trait]
pub trait Transport: Send + Sync + 'static {
    type Addr: Clone + Eq + Hash + Send + Sync;
    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, Self::Addr)>;
    async fn send_to(&self, buf: &[u8], dst: &Self::Addr) -> io::Result<usize>;
    fn local_addr(&self) -> io::Result<Self::Addr>;
}

// ── Codec ─────────────────────────────────────────────────────────────────────
// Removes the dependency on bincode. Encodes/decodes protocol messages; auth wraps it.
pub trait Codec: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    fn encode<T: Serialize>(&self, value: &T, out: &mut Vec<u8>) -> Result<(), Self::Error>;
    fn decode_stream<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<Vec<T>, Self::Error>;
}

// ── Persistence (already exists, persistence.rs:77 — the model port) ────────────
pub trait Persistence<K, V>: Send + Sync + 'static {
    fn load(&self) -> io::Result<Option<PersistedState<K, V>>>;
    fn save(&self, state: &PersistedState<K, V>) -> io::Result<()>;
}
```

Default adapters: `HlcClock: Clock<Timestamp = Hlc>` (its `now`/`observe`, `hlc.rs:121,146`, already
match the port exactly), `UdpTransport(Arc<UdpSocket>)`, `BincodeCodec(DefaultOptions)`,
`FileSnapshot`/`InMemoryPersistence`. The `Authenticator`/MAC stays a `pub(crate)` wrapper
**outside and ahead of** the `Codec` — auth-before-deserialize is a security property (issue #108,
§6.5), never folded into the codec.

### 3.3 Domain types that replace the value-shape plumbing traits

```rust
// One stored, replicated cell — replaces (Hlc, Option<V>) and dissolves
// MaybeTombstone / Reconcilable / Timestamped into a concrete, screaming domain type.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Entry<T, V> { pub stamp: T, pub state: State<V> }

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum State<V> { Present(V), Tombstone }

impl<T: Ord + Copy, V: Clone> Entry<T, V> {
    pub fn is_tombstone(&self) -> bool { matches!(self.state, State::Tombstone) }
    pub fn value(&self) -> Option<&V> { /* … */ }
    /// Default conflict policy: last-write-wins over the timestamp's total order.
    pub fn merge(&self, other: &Self) -> Self {
        if other.stamp > self.stamp { other.clone() } else { self.clone() }
    }
}
```

This removes `(Hlc, Option<V>)` from the public `add_pre_insert` signature and gives
`PersistedState` a named field type. `version_hash` (`reconcile_engine.rs:55`) keeps working since
`Entry` derives `Hash`. **On-wire/on-disk change** (acceptable, unpublished); sequence it where the
proptest oracle re-runs (Phase 4, §5).

### 3.4 The internal mechanism stays internal

`HashRangeQueryable` + `Diffable` are deleted as *traits*. The range-hash capability becomes
inherent methods on the concrete `HRTree`; `start_diff`/`diff_round` become `pub(crate)` free
functions over it, living in a `proto` module. **None of this is public.** The wire types
`HashSegment`/`DiffRange` are `pub(crate)`. This is the "popote interne" — it must not appear in the
crate's external contract. Move `diff_round` **verbatim**: it carries the #106/#111 size-not-hash
rule and the #112 malformed-bound hardening (§6.3, §6.4).

### 3.5 Conflict resolution: domain policy, not an infra port

LWW-on-`Hlc` is **domain logic**, so it lives in the domain as the concrete default (`Entry::merge`
above). It is *not* an outbound port. Expose a `Resolve` seam **only if** swapping LWW for another
strategy (e.g. a CRDT) is a real roadmap item; otherwise a trait here would be the same speculative
ceremony we are removing. Recommendation: ship the concrete LWW default; add the seam when a second
policy actually exists.

### 3.6 Bound bundles to de-verbose (stable supertrait + blanket impl)

```rust
pub trait Key:
    Clone + Debug + Hash + Ord + Send + Sync + Serialize + DeserializeOwned + 'static {}
impl<T> Key for T where T: /* same */ {}

pub trait Value:
    Clone + Debug + Hash + PartialEq + Send + Sync + Serialize + DeserializeOwned + 'static {}
impl<T> Value for T where T: /* same */ {}
```

The 11-line `where` block at `reconcile_engine.rs:122-135` collapses to `impl<K: Key, V: Value>`;
the entry semantics travel with `Entry`, not with `V`.

### 3.7 Workspace = compiler-enforced dependency inversion

This is where the **multi-crate workspace you asked for earns its keep** (and supersedes the
"single-crate-first" hedge from the previous draft): a crate boundary is the only thing that
*mechanically prevents* the domain from importing tokio/bincode. The hexagon becomes:

```
reconcile-core     // DOMAIN + PORTS. Defines Clock, Transport, Codec, Persistence (traits),
                   //   Entry/State, Hlc, Fingerprint, HRTree, the diff algorithm, LWW.
                   //   deps: serde, blake3 only. NO tokio, NO bincode, NO chrono-IO, NO ipnet.
reconcile-net      // ADAPTERS: UdpTransport (tokio), BincodeCodec (bincode), Authenticator,
                   //   peer discovery (ipnet/rand).            deps: reconcile-core + infra.
reconcile-store    // ADAPTERS: FileSnapshot / InMemory persistence.  deps: reconcile-core + infra.
reconcile          // WIRING + driving API: ReconcileStore, Config.   deps: all of the above.
```

The dependency arrow points **only inward** (adapters → core, never the reverse). If someone writes
`use tokio::…` in `reconcile-core`, it fails to compile — that is the guarantee a single crate
cannot give. Ports are defined **in `reconcile-core`** (hexagonal "dependency inversion": the domain
owns the interface, the adapter depends on it).

---

## 4. Iso-functional cleanups (orthogonal to the ports work, ~450 lines)

Mechanical, behaviour-preserving simplifications, valid under any philosophy. Verified by the
existing test suite (a clean compile + green tests *is* the proof).

| Cleanup | `file:line` | Win |
|---|---|---|
| **8 iterator types → one shared cursor + projection.** `IntoIter/Iter/IterMut/Values/ValuesMut/Keys/IntoKeys/IntoValues` recopy the same descent; 3 twin `*Layer` enums. | `hrtree_iter.rs:63-530` | **~250 lines** |
| **7 flat `Arc<RwLock<…>>` + 11-field manual `Clone` → one `Arc<Inner>` + `#[derive(Clone)]`.** One lock-discipline story instead of eight. | `reconcile_engine.rs:64-105` | **~50 lines** |
| **Broadcast copy-pasted 3× (insert / insert_bulk / acks) → one `broadcast_messages` helper.** | `reconcile_engine.rs:194-254,407-502` | **~30 lines** |
| **Nested `fn aux(...)` re-declaring bounds** already fixed by the `impl` block. | `hrtree.rs` `get/insert/remove/position/…` | **~30 lines** |
| **Manual `Clone` on `ReconcileStore`/`TimeoutWheel` → derive** once `Arc<Inner>` lands; dual-index wheel behind one lock. | `reconcile_store.rs:60-72`, `timeout_wheel.rs:11-25` | **~20 lines** |
| **steal-left/steal-right rebalance dup; silent `.unwrap()` on network serialize → `.expect()`.** | `hrtree.rs:142-259`, `reconcile_engine.rs:320,579+` | clarity |

The **core algorithm is not over-engineered** (HLC, causal-stability gate, fingerprint caching are
tight and necessary). The bloat is in **scaffolding**, not logic.

---

## 5. Migration phases (each independently committable, test-green)

| Phase | What | Risk | Oracle |
|---|---|---|---|
| **0 — Iso-functional cleanups** (§4) | Iterator cursor, `Arc<Inner>` + derive `Clone`, broadcast helper, drop redundant bounds. Pure refactor, no contract change. | low | whole suite compiles + passes unchanged. |
| **1 — Bound bundles + hide the plumbing** | Add `Key`/`Value`; demote `diff`, `fingerprint` internals, `hrtree_iter`, `gen_ip`, `hlc::HlcClock`, `auth` from `pub` to `pub(crate)` in `lib.rs`. | low | suite green; check `tests/diff.rs` imports of demoted paths. |
| **2 — Dissolve plumbing traits** | Delete `Diffable`/`HashRangeQueryable`; `diff_round`/`start_diff` → `pub(crate)` free fns over `HRTree` (**verbatim**). | low | `tests/diff.rs` (#106/#111/#112), `proptest_hrtree.rs`. |
| **3 — `Clock` port** | Extract `Clock`; `HlcClock` becomes its adapter; move the `chrono::Utc` read into the adapter so the domain is time-source-free. `Timestamped` → field access. | low | `hlc.rs` unit tests (monotonicity, observe, tie-break). |
| **4 — `Entry`/`State` domain type** | Replace `(Hlc,Option<V>)`; dissolve `MaybeTombstone`; `Reconcilable` → `Entry::merge`; fix public `add_pre_insert`; update `PersistedState`. **Changes encoding.** | **highest** | `tests/service.rs` (resurrection, conflicts, convergence), `proptest_hrtree.rs`, `FileSnapshot` round-trips. |
| **5 — `Transport` + `Codec` ports** | Extract both; `UdpTransport`/`BincodeCodec` adapters; rewrite the engine loop as a thin driver delegating to ports; keep `Authenticator` ahead of the codec; freeze `DefaultOptions`/`Message` tag order. **Unlocks in-memory transport for deterministic tests.** | medium | `tests/service.rs` convergence, `fuzz_packets.rs`, lossy-transport proptest. |
| **6 — Workspace split** | Promote modules to `reconcile-core` / `reconcile-net` / `reconcile-store` / `reconcile`; ports defined in `-core`; verify `-core` has no infra deps. | medium | full suite green; `cargo tree` shows no tokio/bincode under `reconcile-core`. |

Phase 0 delivers immediate, risk-free value. The encoding change (Phase 4) sits in the middle where
the oracle is richest. The workspace split is last, once layers are stable — but it is now a
*goal*, not a hedge, because it enforces the hexagon's dependency direction.

---

## 6. What must NOT change (load-bearing)

1. **`Fingerprint` wire format & arithmetic** (`[u64;4]`, add/sub mod 2²⁵⁶, per-element BLAKE3) +
   golden vectors in `fingerprint.rs`. Relocate only.
2. **HLC total order `(wall_ms, counter, node_id)`** (`hlc.rs:44-54`) — the derived `Ord` *is* the
   conflict order. `Clock::Timestamp = Hlc` must preserve it; do not weaken `>` to `>=` in merge.
3. **Size-not-hash emptiness/equality in `diff_round`** (#106/#111, `diff.rs:135-141`).
4. **Malformed-bound / inverted-range hardening in `diff_round`** (#112, `diff.rs:100-134`).
5. **Auth-before-deserialize** (#108): MAC verified on raw bytes *before* `Codec::decode`; the
   `Codec` port never absorbs auth.
6. **Causal-stability tombstone gate:** GC only after every monotonic `members` entry acked the
   exact `version_hash`. Pinned by the resurrection test in `tests/service.rs`.
7. **`version_hash` determinism** (`reconcile_engine.rs:55`, `DefaultHasher` fixed keys) — keep it
   when `Entry` derives `Hash`.

---

## 7. Summary

The crate already has a clean, infra-free domain mechanism (HRTree/fingerprint/diff) and exactly one
well-formed port (`Persistence`). The "usine à gaz" is (a) internal mechanism — `Diffable`,
`HashRangeQueryable`, `MaybeTombstone` — *published* as if it were the contract, and (b) the absence
of ports, leaving the domain welded to tokio/UDP/bincode/chrono inside one god-module. The refonte
is therefore not "fewer traits" nor "more traits" — it is **the right traits as ports** (`Clock`,
`Transport`, `Codec`, `Persistence`) defined by the domain, **the plumbing hidden** (`diff`,
range-hash) or **dissolved into screaming domain types** (`Entry`/`State` for the tombstone/version
model), **bound bundles** to kill the verbosity, and a **workspace** whose crate boundaries enforce
the inward-only dependency direction the hexagon requires. The seven load-bearing invariants of §6
are held fixed, and the existing test suite keeps every phase honest.

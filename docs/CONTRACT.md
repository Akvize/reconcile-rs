# Contract

What `reconcile` promises, what it asks of you, and what it reserves the right to change.

The other three documents answer different questions: [`ARCHITECTURE.md`](./ARCHITECTURE.md) how it
is built, [`PROGRESS.md`](./PROGRESS.md) where it stands, [`SOTA.md`](./SOTA.md) where it sits.
This one is the only normative document. If it disagrees with them, it wins.

---

## 1. Stability

**Version 0.2.1. Nothing here is stable yet.** Every contract below can break in a minor release
until 1.0. Breaks are batched rather than dribbled
([D10](./ARCHITECTURE.md#d10--030-is-one-coordinated-break)).

| Contract | Guarantee today | Broken by |
|---|---|---|
| Public API | none | any minor release |
| Wire format | identical bytes across a cluster running the same minor version | any minor release. **Not rolling-upgrade safe** |
| On-disk snapshot | versioned header, older versions rejected loudly | any minor release |
| MSRV (1.85) | a bump is a minor increment; patches never raise it | minor release only |
| Semantics (§3) | the most stable thing here. Changing one is a deliberate decision, recorded in the ledger | a recorded decision |

**Upgrade procedure across a minor version: quiesce the cluster, upgrade every node, restart.** A
0.2.x node and a 0.3.0 node do not interoperate, and the failure is not always loud.

---

## 2. Public surface

Supported. Everything else is internal, whatever its Rust visibility.

Names below are the module's public items. Most are also re-exported at the crate root; `Config`
and `MAX_NETS` are not, so they are reached as `reconcile::reconcile_store::{Config, MAX_NETS}`.

| Module | Public items |
|---|---|
| `reconcile_store` | `ReconcileStore`, `Config`, `MAX_NETS` |
| `mirror` | `ReconcileMirror` |
| `reconcilable` | `Entry`, `State` |
| `clock` | `Clock`, `Timestamp` |
| `bounds` | `Key`, `Value` |
| `hrtree` | `HRTree` and its iterators (`Iter`, `Keys`, `Values`, `IntoIter`, `IntoKeys`, `IntoValues`) |
| `fingerprint` | `Fingerprint` |
| `persistence` | `Persistence`, `PersistedState`, `FileSnapshot`, `InMemoryPersistence`, `LoadError` |
| `transport` | `Transport`, `UdpTransport`, `InMemoryNetwork`, `InMemoryTransport` |
| `discovery` | `Discovery`, `DiscoverFuture`, `RandomProbe`, `DnsDiscovery` |
| `prometheus` | `serve` (async), `install_recorder` — only under `metrics-prometheus` |

**Not public, do not depend on it:**

- The anti-entropy protocol: `proto`, `HashSegment`, `DiffRange`, `start_diff`, `diff_round`. It is
  mechanism, not contract ([§3.5](./ARCHITECTURE.md#35-internal-mechanism)).
- The `Codec` port and `BincodeCodec`. Internal on purpose
  ([D2](./ARCHITECTURE.md#d2--transport-is-consumer-wireable-codec-is-not)).
- `auth`, `replay`, `reconcile_engine`, `timeout_wheel`, `gen_ip`, `observability`.
- `reconcile::testing`, and the `peers_map_len` / `peers_contains` / `replay_filter_len` /
  `tombstone_acks_len` / `bulk_dumps_in_flight_count` methods. All gated on
  `--cfg reconcile_internal_testing`, which only this repository sets and which is not packaged, so
  they never reach a consumer build.

`HRTree` moves to its own crate under a different name at migration step 6
([D1](./ARCHITECTURE.md#d1--hrtree-becomes-its-own-product-correctly-named)). Depend on
`ReconcileStore`, not on the tree, unless you want to follow that move.

---

## 3. Semantics

### 3.1 What you get

| | |
|---|---|
| **Strong eventual consistency** | Replicas that received the same updates hold the same state, whatever the order. Conditional on §4. |
| **Conflict resolution** | Last-write-wins on the HLC total order `(wall_ms, counter, node_id)`, strict `>`. Commutative, associative, idempotent. |
| **Reads** | Local, in-process, `O(log n)`. No network hop, no deserialization. |
| **Read-your-writes** | On the node that wrote, yes. Across nodes, no. |
| **Deletes** | Propagate as tombstones and are collected only once every monotonic member has acknowledged that exact version. No resurrection. |
| **Writes** | Infallible, pushed immediately, and repaired by anti-entropy if the datagram is lost. |
| **Partitions** | Every node keeps serving. Convergence resumes on heal. |

### 3.2 What you do not get

| | |
|---|---|
| Counters, quotas, sums | LWW overwrites. Concurrent `+1` and `+1` yields `+1`, and no key encoding recovers it ([D6](./ARCHITECTURE.md#d6--conflict-resolution-stays-hardcoded-last-write-wins)). |
| Concurrency detection | `Timestamp` is a *total* order, so one of any two stamps is greater. Concurrency is destroyed at mint time, not recoverable downstream. |
| Transactions, linearizability, strong consistency | Not offered, not planned. |
| Cross-key ordering | Two keys written in order may arrive in either order on a peer. |
| Delivery guarantees | UDP. Datagrams are lost, duplicated, reordered. Anti-entropy is the repair. |
| A bound on convergence time | Anti-entropy is periodic and gossip is fan-out-limited. Convergence is eventual, not timed. |
| Datasets beyond one node's RAM | Closed, not deferred ([D7](./ARCHITECTURE.md#d7--larger-than-ram-datasets-are-permanently-out-of-scope)). |
| A pluggable merge policy | LWW is hardcoded (D6). |

---

## 4. Your obligations

Break one of these and convergence breaks, usually **silently**.

| Obligation | Why | Failure mode |
|---|---|---|
| **`Hash` on your `K` and `V` must be self-delimiting** | Per-element fingerprints hash key and value into one stream with no separator | Two distinct elements collide across the key/value boundary and replicas never converge. The std impls (integers, `str`/`String`, slices, `Vec`) are already fine; a hand-written impl may not be |
| **`Hash` must be stable across nodes** | The fingerprint is a wire token | Peers compute different fingerprints for identical content and refine forever |
| **Distinct `node_id` per node** | It is the deterministic tie-break making the order total | Two nodes can each keep their own value at equal `(wall_ms, counter)`. Random by default, so distinctness is only probabilistic |
| **Pin `node_id` when you persist** | A random id changes on every restart, so the LWW identity changes with it | The total order holds only within one process lifetime. The store warns at startup if you plug in persistence with a random id |
| **Same cluster key, same MAC backend, same `encryption` setting, on every node** | The MAC covers the framing | Datagrams are silently dropped. The cluster looks up and never converges |
| **Keep values under the datagram ceiling** (§6) | The send path packs but never fragments | The update is never delivered and that key never converges anywhere. Local reads still show it, so the node cannot detect its own failure. Only a `warn!` on the send path ([#230](https://github.com/Akvize/reconcile-rs/issues/230)) |
| **Never ignore `LoadError::Corrupt`** | Starting empty drops tombstones | Deleted values resurrect across the cluster. Halt and alert an operator instead |
| **Only declare nets you operate** | `with_net` tells the node where to send discovery probes. It is not a security boundary | You scan someone else's address range |
| **Do not hold the guard from `get()`** | It is a `MappedRwLockReadGuard`: it holds a read lock on the map | Writers block for as long as you hold it. Never hold one across an `.await` |
| **Upgrade the whole cluster at once** | §1 | Mixed versions do not interoperate |
| **Do not expose `/metrics` publicly** | Unauthenticated, and it fingerprints your deployment | Dataset size, churn and peer activity leak |

The `pre_insert` hook runs **outside** the map's write lock, on both the local and the gossip path.
It may call back into the store without deadlocking. It runs on the receive path, so keep it cheap.

---

## 5. Extending it

Three ports are consumer-wireable. The fourth deliberately is not. Each carries obligations
beyond its type signature.

| Port | You may supply | Contract on your impl |
|---|---|---|
| `Persistence` | `ReconcileStore::with_persistence` | `save` must be atomic: a crash mid-save leaves either the old snapshot or the new one, never a torn one. `load` returns `Ok(None)` for "no snapshot yet", which is a clean fresh start. **On failure the error kind is load-bearing:** `io::ErrorKind::InvalidData` is classified `LoadError::Corrupt` and everything else `LoadError::Io`. Return the wrong kind for a corrupt snapshot and the caller retries forever instead of halting. |
| `Transport` | `ReconcileStore::new_with_transport` | May lose, duplicate, reorder and delay: the protocol already assumes it. Must not truncate silently — return the byte count you sent. **`recv_from` must be cancel-safe**: the receive loop wraps it in `tokio::time::timeout`, so the future is dropped mid-poll on every idle tick. An implementation that consumes a datagram before yielding loses one per timeout. |
| `Discovery` | `ReconcileStore::with_discovery` | Return addresses only. `with_discovery` requires `is_authoritative() == true`, because an absence there is read as a vanished peer and decommissions it after a grace period. A speculative prober belongs in the engine's per-round probe, not here. Either way discovery never grants membership, so it cannot break tombstone GC. |
| `Clock` | **nothing — test-gated on purpose** | An unreliable transport cannot break an invariant. A non-monotonic clock silently breaks causal ordering and tombstone GC ([D2](./ARCHITECTURE.md#d2--transport-is-consumer-wireable-codec-is-not)). |

`Codec` is internal (D2). Compression and cross-language interop are not delivered by swapping the
trait: compression interacts with authenticate-before-decode and with datagram size accounting, and
interop needs a published wire specification.

---

## 6. Wire

| | |
|---|---|
| Encoding | `bincode` with `DefaultOptions` and a decode limit |
| Message tags | The `Message` variant order is the tag order: `ComparisonItem`, `Update`, `Ack`, `ValueComparisonItem`, `ValueUpdate`. Frozen within a format version. Append only, never reorder |
| Datagram budget | 65507 bytes total |
| Keyless framing | `payload`, byte-for-byte the unauthenticated protocol. Overhead 0, so **65507 bytes** of payload |
| MAC framing | `tag(replay_header ‖ payload) ‖ replay_header ‖ payload`. 32-byte tag + 16-byte replay header = 48 bytes overhead, so **65459 bytes** of payload |
| AEAD framing | `nonce ‖ ciphertext(replay_header ‖ payload) ‖ tag`, XChaCha20-Poly1305. 24 + 16 + 16 = 56 bytes overhead, so **65451 bytes** of payload |
| Ordering rule | The MAC is verified on raw bytes **before** any decoding. Never folded into the codec ([invariant 5](./ARCHITECTURE.md#5-invariants)) |
| Fragmentation | **None.** One protocol message must fit one datagram (invariant 9) |
| Topology on the wire | Nothing. Nets are derived from IP addresses, so a single-net and a multi-net cluster speak identically |

A datagram that fails any check is dropped silently: nothing is returned to the sender, and the peer
is not told. The only signal is the `reconcile_datagrams_dropped_total` counter, whose `reason`
label currently takes `bad_mac`, `replay`, `malformed`, `too_large`, `peer_cap` and `recv_error`.
Those label values are informative, not contractual (§9) — alert on the counter, not on a
particular reason string.

---

## 7. On disk

| | |
|---|---|
| Layout | `RCNL` magic (4 bytes) ‖ format version (`u32`, little-endian, currently **1**) ‖ bincode body |
| Body | `PersistedState`: every key with its dated `Entry`, the causal-stability `members` set, and the per-tombstone acknowledgment map |
| Compatibility | A header-less 0.2.x snapshot is rejected as `LoadError::Corrupt` with a descriptive message, never silently misread |
| Not persisted | `node_id`. Pin it in `Config` if you want a stable LWW identity across restarts |

---

## 8. Build

| | |
|---|---|
| MSRV | 1.85. A bump is a minor increment; patch releases never raise it |
| Safety | `#![forbid(unsafe_code)]` |
| `mac-blake3` (default) / `mac-hmac` | **Mutually exclusive.** `mac-blake3` wins if both are on. At least one must be enabled or the crate does not compile. Every node in a cluster must use the same one |
| `encryption` | Enables `Config::with_encryption`. All nodes must enable it together |
| `zeroize` | Wipes the cluster key on drop |
| `metrics` / `metrics-prometheus` | Off by default; every metric call site compiles to a no-op when off. `metrics-prometheus` implies `metrics` |
| `dns-hickory` | Reserved, not implemented. The default `DnsDiscovery` uses the system resolver |

---

## 9. What we change without warning

Anything not listed above. Specifically: the diff algorithm and its wire types, the codec, the
engine, timing and pacing constants, default `Config` values, log and span content, metric names,
and the internal module layout. Fingerprint *arithmetic* is an invariant; the fingerprint *encoding*
is not, and is scheduled to change in 0.3.0.
